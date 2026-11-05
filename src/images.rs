//! Image bytes fetch orchestration (v0.0.2 spec §5): version-cached node
//! renders and fill images, shared by the CLI `figmog images` command
//! (`cli::images`) and `figmog serve`'s `figmog_images` tool
//! (`sessions::FileSession::images`, wired in `serve.rs`).
//!
//! Two fetch kinds behind one call: **renders** (`GET /v1/images/:key`,
//! one node → one image) and **fills** (`GET /v1/files/:key/images`, a
//! file-wide `imageRef` hash → URL map, scanned out of the *requested*
//! nodes' raw `fills` arrays). Both are cached in the `images` table,
//! keyed by [`image_key`] and version-gated exactly like `proxy_cache`
//! (`store::stale_image_ids` / `store::evict_stale_cache`) — a repeat
//! request against an unchanged file spends zero Figma API calls.
//!
//! **Opt-in render reuse (NWS-2958).** With [`RenderPolicy::reuse`] a
//! render is cached against its [`render_fingerprint`] instead — the
//! node's subtree plus the render settings — in the `render_index` /
//! `render_blobs` tables, and stays a hit across file versions until the
//! fingerprint changes. Those rows are outside the version-change
//! eviction; [`plan_render_eviction`] removes orphans and enforces a byte
//! quota instead. Off by default: see `docs/SPEC.md` §15.
//!
//! **Two-step call shape.** [`scan`] (read-only, against already-open
//! table readers) then [`resolve`] (network + cache-write, against
//! `&mut KeyedStream<Id, Rec, P>`) rather than one function that does
//! both: `KeyedStream`'s pipeline type is unnameable outside its own
//! `open_store!` expansion site (see `dispatch.rs`'s and `store.rs`'s own
//! doc comments on the same constraint), so a function generic over
//! `P: Push<..>` can call `st.wtx(..)` (as `resolve` does — writes don't
//! hit this) but can't destructure `st.rtx(..)`'s reader tuple, since that
//! tuple's type is `P`'s own opaque associated type. Every caller
//! therefore calls `st.rtx(|tuple| scan(..))` at its own concrete
//! `open_store!` call site, then passes the result into `resolve` —
//! exactly the shape `store::collect_sweepable` + `store::sync` already
//! use at `cli::pull::do_pull` and `sessions::open_session_at`.
//!
//! `resolve`'s network dependencies are both injected (`api: Option<&dyn
//! FigmaApi>`, `download: &dyn Fn(&str) -> Result<Vec<u8>, ApiError>`) so
//! this module's own tests exercise the full orchestration — cache
//! hit/miss, the fills dedup, per-item 429/error handling — against a
//! scripted fake, with no live network call anywhere in this crate's test
//! suite (documented per the v0.0.2 plan: images has no automated
//! live-network test).

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use fold::pipeline::terminal::{MultimapReader, TableReader};
use fold::pipeline::{Keyed, Push};
use fold::stream::{KeyedStream, Readable};
use serde_json::{Value, json};

use crate::api::{ApiError, FigmaApi};
use crate::assets::sha256_hex;
use crate::ident::normalize_node_ref;
use crate::model::{
    FileMeta, Id, ImageBlobRec, ImagePinRec, NodeRec, Rec, RenderBlobRec, RenderIndexRec,
};

/// [`ImageItem::kind`] for a node render.
pub const RENDER: &str = "render";
/// [`ImageItem::kind`] for a fill image.
pub const FILL: &str = "fill";

/// Current render cache generation (NWS-2960). A render row is a cache
/// hit only when its [`ImagePinRec`] marker carries this generation:
/// generation 1 means "requested with `version=<the row's file_version>`",
/// so the bytes really are that version's render. Render rows an older
/// binary stored (requested unpinned — possibly showing a newer design
/// than their label) have no marker and read as misses, so they get
/// refetched and overwritten. Fill rows never need a marker.
pub const RENDER_PIN_GENERATION: u32 = 1;

/// Version of the render fingerprint scheme (NWS-2958), folded into every
/// fingerprint and stored on every [`RenderIndexRec`]. Bump it whenever
/// what [`render_fingerprint`] hashes changes: every row stored under an
/// older scheme then reads as an orphan and is evicted, never served.
pub const RENDER_FINGERPRINT_SCHEME: u32 = 2;

/// Key of the render cache's cleanup marker (NWS-2958): a `render_index`
/// row that is not a render. Its `scheme` and `file_version` record the
/// fingerprint scheme and mirror version of the last orphan cleanup, so
/// a request whose renders all hit skips the orphan pass until the
/// mirror (or the scheme) changes. Never a valid SHA-256 hex, so it can't
/// collide with a fingerprint; never served, counted or evicted.
pub const RENDER_CLEANUP_KEY: &str = "cleanup";

/// Default hard byte quota for fingerprint-indexed render bytes
/// (`render_blobs`, NWS-2958): 256 MiB. Counts each distinct blob once.
pub const RENDER_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// How one image request treats cached renders (NWS-2958). The default
/// serves and writes renders per file version only, exactly as before
/// NWS-2958; the one thing it does to the fingerprint-indexed render cache
/// is retire a node's fingerprinted render when it fetches a newer
/// per-version one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderPolicy {
    /// Opt in to cross-version render reuse: a render stays a hit for as
    /// long as its node's [`render_fingerprint`] is unchanged, whatever
    /// else in the file was edited. Off by default until the rendering
    /// contract is confirmed against Figma's images endpoint.
    pub reuse: bool,
    /// Ignore every cached render (version-pinned and fingerprinted) and
    /// refetch. Fills are content-addressed and unaffected.
    pub refresh: bool,
    /// Hard byte quota for fingerprint-indexed render bytes, enforced
    /// after every request that wrote or retired a fingerprinted render
    /// or ran the orphan pass ([`plan_render_eviction`]).
    pub max_bytes: u64,
}

impl Default for RenderPolicy {
    fn default() -> Self {
        RenderPolicy {
            reuse: false,
            refresh: false,
            max_bytes: RENDER_CACHE_MAX_BYTES,
        }
    }
}

/// The read handles [`scan`]/[`scan_split`] need, borrowed from one
/// `st.rtx(..)` reader tuple at a concrete `open_store!` site (see this
/// module's doc comment for why the tuple itself can't be passed).
pub struct ScanReaders<'r, 'a, R: Readable> {
    pub nodes: &'r TableReader<'a, R, String, NodeRec>,
    pub children: &'r MultimapReader<'a, R, String, (u32, String)>,
    pub meta: &'r TableReader<'a, R, u8, FileMeta>,
    pub images: &'r TableReader<'a, R, String, ImageBlobRec>,
    pub pins: &'r TableReader<'a, R, String, ImagePinRec>,
    pub render_index: &'r TableReader<'a, R, String, RenderIndexRec>,
    pub render_blobs: &'r TableReader<'a, R, String, RenderBlobRec>,
}

/// Bind a [`ScanReaders`] named `$r` from one `st.rtx(..)` reader tuple
/// (`figmog_pipeline!`'s top-level order — see `dispatch::RootReaders`)
/// and evaluate `$body` with it: the one place that knows which tuple
/// positions [`scan`] reads.
#[macro_export]
macro_rules! with_scan_readers {
    ($tuple:expr, |$r:ident| $body:expr) => {{
        let (
            (nodes, children, ..),
            _,
            _,
            _,
            _,
            _,
            meta,
            _,
            _,
            images,
            pins,
            _,
            render_index,
            render_blobs,
        ) = $tuple;
        let $r = $crate::images::ScanReaders {
            nodes: &nodes,
            children: &children,
            meta: &meta,
            images: &images,
            pins: &pins,
            render_index: &render_index,
            render_blobs: &render_blobs,
        };
        $body
    }};
}

/// Fingerprint of what a render of `node_id` at `format`/`scale_milli`
/// shows (NWS-2958), as SHA-256 hex — or `None` when figmog can't vouch
/// for one, and the render falls back to plain per-version caching.
///
/// Hashed, together with [`RENDER_FINGERPRINT_SCHEME`], `format` and
/// `scale_milli`:
///
/// - **the subtree**, as a Merkle hash over every node's stored raw JSON
///   ([`NodeRec::raw`]) in child order. Instance children arrive expanded
///   in the REST response, variable and style values are baked into the
///   raw properties, image fills are referenced by content hash and fonts
///   are named in text styles, so this is nearly everything that affects
///   the picture. Two things are normalized out first: prototype-only
///   keys ([`PROTOTYPE_ONLY_KEYS`]), and page-absolute position — each
///   node's `absoluteBoundingBox`/`absoluteRenderBounds`/
///   `absoluteTransform`/`relativeTransform` translation is replaced by
///   its offset from its parent (rounded to 1/1000 px), and the rendered
///   node's own offset is dropped. Moving the node, or reflowing it
///   inside an auto-layout parent, keeps its fingerprint; resizing or
///   moving anything inside it changes it. Node ids stay part of the raw
///   JSON, so a fingerprint belongs to exactly one node;
/// - **local main components** (conservative): for every INSTANCE in the
///   subtree whose main component is mirrored, that component's own
///   subtree hash, transitively — so editing a main component invalidates
///   its instances' renders even if an expansion missed the change;
/// - **inherited variable modes** (conservative): every ancestor's
///   `explicitVariableModes`, root first.
///
/// `None` (the conservative fallback) when the node, an ancestor or a
/// subtree node is missing from the mirror, when a sibling drawn before
/// the node or before any of its ancestors is a mask (`isMask`) that
/// might clip it, or when the tree is inconsistent.
pub fn render_fingerprint<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    node_id: &str,
    format: &str,
    scale_milli: u32,
) -> Option<String> {
    Fingerprinter::new(nodes, children).fingerprint(node_id, format, scale_milli)
}

/// Raw node keys that only drive prototyping, never the static picture,
/// left out of [`render_fingerprint`] (scheme 2).
pub const PROTOTYPE_ONLY_KEYS: &[&str] = &[
    "flowStartingPoints",
    "interactions",
    "prototypeBackgrounds",
    "prototypeDevice",
    "prototypeStartNodeID",
    "reactions",
    "transitionDuration",
    "transitionEasing",
    "transitionNodeID",
];

// Test-only count of node hashes computed, so tests can prove a request
// doesn't walk subtrees it has no reason to.
#[cfg(test)]
thread_local! {
    pub(crate) static NODE_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One node's subtree hash and the main components its instances use.
struct NodeHash {
    hash: String,
    components: BTreeSet<String>,
}

/// Where a node sits on the page: what [`node_core`] strips from its raw
/// JSON, kept to express its children's offsets from it.
#[derive(Clone, Copy, Default)]
struct Placement {
    /// `absoluteBoundingBox` x, y.
    origin: Option<(f64, f64)>,
    /// `absoluteTransform` translation.
    absolute: Option<(f64, f64)>,
    /// `relativeTransform` translation.
    relative: Option<(f64, f64)>,
}

/// Round a coordinate offset to 1/1000 px, so float noise from
/// subtracting page positions can't change a fingerprint (and `-0` reads
/// as `0`).
fn round_offset(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0 + 0.0
}

fn xy(v: Option<&Value>) -> Option<(f64, f64)> {
    let v = v?;
    Some((v.get("x")?.as_f64()?, v.get("y")?.as_f64()?))
}

/// Zero a 2×3 affine transform's translation, returning it.
fn take_translation(t: Option<&mut Value>) -> Option<(f64, f64)> {
    let rows = t?.as_array_mut()?;
    if rows.len() != 2 {
        return None;
    }
    let tx = rows[0].get(2)?.as_f64()?;
    let ty = rows[1].get(2)?.as_f64()?;
    rows[0][2] = json!(0.0);
    rows[1][2] = json!(0.0);
    Some((tx, ty))
}

/// A node's raw JSON without prototype-only keys or its page position,
/// as canonical JSON, plus that position. A node without an
/// `absoluteBoundingBox` keeps its absolute render bounds (conservative).
fn node_core(raw: &str) -> Option<(String, Placement)> {
    #[cfg(test)]
    NODE_HASHES.with(|n| n.set(n.get() + 1));
    let mut value: Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object_mut()?;
    for key in PROTOTYPE_ONLY_KEYS {
        obj.remove(*key);
    }
    let origin = xy(obj.get("absoluteBoundingBox"));
    if let Some((ox, oy)) = origin {
        if let Some(bbox) = obj
            .get_mut("absoluteBoundingBox")
            .and_then(Value::as_object_mut)
        {
            bbox.remove("x");
            bbox.remove("y");
        }
        if let Some((rx, ry)) = xy(obj.get("absoluteRenderBounds"))
            && let Some(bounds) = obj
                .get_mut("absoluteRenderBounds")
                .and_then(Value::as_object_mut)
        {
            bounds.insert("x".into(), json!(round_offset(rx - ox)));
            bounds.insert("y".into(), json!(round_offset(ry - oy)));
        }
    }
    let placement = Placement {
        origin,
        absolute: take_translation(obj.get_mut("absoluteTransform")),
        relative: take_translation(obj.get_mut("relativeTransform")),
    };
    Some((serde_json::to_string(&value).ok()?, placement))
}

/// `child`'s offset from `parent`, as canonical JSON: bounding-box and
/// absolute-transform deltas (absolute when the parent has none), and the
/// relative-transform translation only when no absolute transform says
/// it already.
fn offset(child: &Placement, parent: &Placement) -> Value {
    let delta = |c: Option<(f64, f64)>, p: Option<(f64, f64)>| match (c, p) {
        (Some(c), Some(p)) => json!([round_offset(c.0 - p.0), round_offset(c.1 - p.1)]),
        (Some(c), None) => json!(["absolute", c.0, c.1]),
        (None, _) => Value::Null,
    };
    let relative = match (child.absolute, child.relative) {
        (None, Some((x, y))) => json!([round_offset(x), round_offset(y)]),
        _ => Value::Null,
    };
    json!([
        delta(child.origin, parent.origin),
        delta(child.absolute, parent.absolute),
        relative
    ])
}

/// [`render_fingerprint`] with every node's subtree hash, raw-flag and
/// per-subject result memoized, so one request hashes each node at most
/// once however many renders share it.
struct Fingerprinter<'r, 'a, R: Readable> {
    nodes: &'r TableReader<'a, R, String, NodeRec>,
    children: &'r MultimapReader<'a, R, String, (u32, String)>,
    base: BTreeMap<String, Option<String>>,
    hashes: BTreeMap<String, Option<(Rc<NodeHash>, Placement)>>,
    masks: BTreeMap<String, Option<bool>>,
}

impl<'r, 'a, R: Readable> Fingerprinter<'r, 'a, R> {
    fn new(
        nodes: &'r TableReader<'a, R, String, NodeRec>,
        children: &'r MultimapReader<'a, R, String, (u32, String)>,
    ) -> Self {
        Fingerprinter {
            nodes,
            children,
            base: BTreeMap::new(),
            hashes: BTreeMap::new(),
            masks: BTreeMap::new(),
        }
    }

    fn fingerprint(&mut self, node_id: &str, format: &str, scale_milli: u32) -> Option<String> {
        if !self.base.contains_key(node_id) {
            let base = self.compute_base(node_id);
            self.base.insert(node_id.to_string(), base);
        }
        let base = self.base.get(node_id)?.as_ref()?;
        let input = format!(
            "figmog-render-fingerprint\0{RENDER_FINGERPRINT_SCHEME}\0{format}\0{scale_milli}\0{base}"
        );
        Some(sha256_hex(input.as_bytes()))
    }

    fn compute_base(&mut self, node_id: &str) -> Option<String> {
        let node = self.nodes.get(&node_id.to_string())?;
        let context = self.ancestor_context(&node)?;
        let root = self.subtree_hash(node_id)?;

        // Transitive local main components, each hashed once.
        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        let mut pending: BTreeSet<String> = root.components.clone();
        while let Some(component_id) = pending.pop_first() {
            if deps.contains_key(&component_id) {
                continue;
            }
            if self.nodes.contains(&component_id) {
                let component = self.subtree_hash(&component_id)?;
                deps.insert(component_id, component.hash.clone());
                pending.extend(
                    component
                        .components
                        .iter()
                        .filter(|c| !deps.contains_key(*c))
                        .cloned(),
                );
            } else {
                // A remote (library) main component: only the REST
                // expansion inside the instance can speak for it.
                deps.insert(component_id, "remote".to_string());
            }
        }
        let deps = serde_json::to_string(&deps).ok()?;
        Some(format!("{context}\0{deps}\0{}", root.hash))
    }

    /// `root`'s Merkle subtree hash, computed bottom-up without recursion
    /// and memoized per node. `None` if a node is missing or unreadable,
    /// or the children edges loop.
    fn subtree_hash(&mut self, root: &str) -> Option<Rc<NodeHash>> {
        enum Step {
            Enter(String),
            Exit(String, Vec<String>),
        }
        let mut path = BTreeSet::new();
        let mut stack = vec![Step::Enter(root.to_string())];
        while let Some(step) = stack.pop() {
            match step {
                Step::Enter(id) => {
                    if self.hashes.contains_key(&id) {
                        continue;
                    }
                    if !path.insert(id.clone()) {
                        return None;
                    }
                    let mut edges = self.children.get(&id);
                    edges.sort();
                    let kids: Vec<String> = edges.into_iter().map(|(_, kid)| kid).collect();
                    stack.extend(kids.iter().rev().map(|kid| Step::Enter(kid.clone())));
                    // Below its children: popped after all of them.
                    let at = stack.len() - kids.len();
                    stack.insert(at, Step::Exit(id, kids));
                }
                Step::Exit(id, kids) => {
                    path.remove(&id);
                    let hashed = self.hash_node(&id, &kids);
                    self.hashes.insert(id, hashed);
                }
            }
        }
        self.hashes
            .get(root)?
            .as_ref()
            .map(|(hash, _)| hash.clone())
    }

    fn hash_node(&self, id: &str, kids: &[String]) -> Option<(Rc<NodeHash>, Placement)> {
        let node = self.nodes.get(&id.to_string())?;
        let (core, placement) = node_core(&node.raw)?;
        let mut components: BTreeSet<String> = node.component_id.into_iter().collect();
        let mut canonical = format!("[{core},[");
        for (n, kid) in kids.iter().enumerate() {
            let (hash, kid_placement) = self.hashes.get(kid)?.as_ref()?;
            if n > 0 {
                canonical.push(',');
            }
            canonical.push_str(&json!([offset(kid_placement, &placement), hash.hash]).to_string());
            components.extend(hash.components.iter().cloned());
        }
        canonical.push_str("]]");
        let hash = NodeHash {
            hash: sha256_hex(canonical.as_bytes()),
            components,
        };
        Some((Rc::new(hash), placement))
    }

    /// Every ancestor's `explicitVariableModes`, root first, as canonical
    /// JSON. `None` if an ancestor is missing or unreadable, or if a
    /// sibling drawn before the node or before any of its ancestors is a
    /// mask: such a mask clips it in the canvas, and figmog doesn't know
    /// whether a single-node export applies it.
    fn ancestor_context(&mut self, node: &NodeRec) -> Option<String> {
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = node.clone();
        while let Some(parent_id) = current.parent_id.clone() {
            if !seen.insert(parent_id.clone()) {
                return None;
            }
            for (index, sibling) in self.children.get(&parent_id) {
                if index < current.child_index && sibling != current.id && self.is_mask(&sibling)? {
                    return None;
                }
            }
            let parent = self.nodes.get(&parent_id)?;
            if parent.raw.contains("\"explicitVariableModes\"") {
                let raw: Value = serde_json::from_str(&parent.raw).ok()?;
                if let Some(modes) = raw.get("explicitVariableModes") {
                    chain.push(json!([parent_id, modes]));
                }
            }
            current = parent;
        }
        chain.reverse();
        serde_json::to_string(&chain).ok()
    }

    /// Whether `id`'s raw JSON says `isMask: true` (memoized; parsed only
    /// when the canonical text could hold it).
    fn is_mask(&mut self, id: &str) -> Option<bool> {
        if let Some(known) = self.masks.get(id) {
            return *known;
        }
        let mask = self.nodes.get(&id.to_string()).and_then(|node| {
            if !node.raw.contains("\"isMask\":true") {
                return Some(false);
            }
            let raw: Value = serde_json::from_str(&node.raw).ok()?;
            Some(raw.get("isMask").and_then(Value::as_bool) == Some(true))
        });
        self.masks.insert(id.to_string(), mask);
        mask
    }
}

/// The digest a fingerprinted render's bytes are stored and validated
/// under ([`RenderBlobRec::sha256`]): SHA-256, lowercase hex.
pub fn render_blob_digest(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// Whether a stored render blob is intact for `row`: its bytes hash to
/// the key both rows name, at the recorded length.
fn blob_is_valid(blob: &RenderBlobRec, row: &RenderIndexRec) -> bool {
    blob.sha256 == row.sha256
        && blob.bytes.len() as u64 == row.byte_len
        && sha256_hex(&blob.bytes) == row.sha256
}

/// What [`plan_render_eviction`] removes: index rows (fingerprints) in
/// eviction order, and the blobs (SHA-256 keys) no remaining row
/// references, sorted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RenderEviction {
    pub index: Vec<String>,
    pub blobs: Vec<String>,
}

/// Plan the fingerprint-indexed render cache's eviction (NWS-2958), in
/// two separate passes over `index` ordered oldest first (`seq`, then
/// fingerprint as the deterministic tie-break):
///
/// 1. **orphan cleanup**: every row in `orphans` — renders whose
///    fingerprint no node has any more;
/// 2. **hard byte quota**: while the distinct blobs the remaining rows
///    reference exceed `max_bytes`, the oldest remaining row, valid or
///    not — except the `protected` rows, the renders the current request
///    served or wrote, which no request evicts while answering with them
///    (so the cache can exceed `max_bytes` by at most one request's own
///    renders, until the next request that evicts).
///
/// A blob is removed once no remaining row references it — including
/// `displaced` blobs, those an overwritten row used to reference.
pub fn plan_render_eviction(
    index: &BTreeMap<String, RenderIndexRec>,
    orphans: &BTreeSet<String>,
    protected: &BTreeSet<String>,
    displaced: &BTreeSet<String>,
    max_bytes: u64,
) -> RenderEviction {
    let mut order: Vec<&RenderIndexRec> = index.values().collect();
    order.sort_by(|a, b| {
        a.seq
            .cmp(&b.seq)
            .then_with(|| a.fingerprint.cmp(&b.fingerprint))
    });

    let mut evicted: Vec<&RenderIndexRec> = Vec::new();
    let mut live: Vec<&RenderIndexRec> = Vec::new();
    for row in order {
        if orphans.contains(&row.fingerprint) {
            evicted.push(row);
        } else {
            live.push(row);
        }
    }

    // Distinct blob -> (byte length, referencing rows).
    let mut refs: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
    for row in &live {
        refs.entry(row.sha256.as_str())
            .or_insert((row.byte_len, 0))
            .1 += 1;
    }
    let mut total: u64 = refs.values().map(|(len, _)| *len).sum();
    let mut live = live
        .into_iter()
        .filter(|row| !protected.contains(&row.fingerprint));
    while total > max_bytes {
        let Some(row) = live.next() else { break };
        evicted.push(row);
        if let Some(entry) = refs.get_mut(row.sha256.as_str()) {
            entry.1 -= 1;
            if entry.1 == 0 {
                total -= entry.0;
            }
        }
    }

    let blobs: BTreeSet<String> = evicted
        .iter()
        .map(|row| row.sha256.clone())
        .chain(displaced.iter().cloned())
        .filter(|sha| refs.get(sha.as_str()).is_none_or(|(_, count)| *count == 0))
        .collect();
    RenderEviction {
        index: evicted.iter().map(|row| row.fingerprint.clone()).collect(),
        blobs: blobs.into_iter().collect(),
    }
}

/// A caller with no Figma API token still gets every cache hit for free
/// (spec §5: "a repeat request on an unchanged file spends zero budget");
/// every miss gets this exact per-item manifest error instead of a wasted
/// (guaranteed-401) network attempt.
const NO_TOKEN_MSG: &str = "FIGMA_TOKEN is unset or empty and no saved token is available — required for image downloads; an empty FIGMA_TOKEN disables saved credentials, so unset it to use saved credentials; run `figmog auth login` or set FIGMA_TOKEN";

/// An explicit store can serve cached bytes, but its records do not
/// establish the file key needed to fetch a miss safely.
const NO_FILE_KEY_MSG: &str = "image is not cached for this request and file version; --db selects a store without a file key, so it cannot fetch missing images. Run `figmog pull <file-key-or-url>`, then `figmog images <node-id>...` without --db";

/// Content blocks over roughly this many bytes are excluded from the MCP
/// tool's inline content (spec §5's "1MB" cap) — 1 MiB exactly, matching
/// the spec's own "1MB each" wording as closely as a byte count can.
///
/// **Measured on the encoded payload actually shipped, not raw image
/// bytes.** A raster image ships as base64, which inflates the raw byte
/// count by roughly 4/3 (RFC 4648) — a ~786KB PNG becomes a ~1.05MB
/// `data` string, already over this cap even though the *file* is well
/// under 1MB. `to_mcp_content` therefore encodes once, measures the
/// encoded string's length, and compares that (see its own doc comment).
/// SVG ships as raw text (no base64 step — see that function's SVG
/// branch), so for SVG this cap is measured on the markup's own UTF-8
/// byte length directly, which happens to equal the raw byte count.
pub const CONTENT_SIZE_CAP: usize = 1_048_576;

/// Deterministic hex key for one cached image row: FNV-1a 64 (same
/// constants and separator discipline as `cache::cache_key`'s own doc
/// comment, extended from two fields to four) over `kind`+NUL+`subject`+
/// NUL+`format`+NUL+`scale_milli` (decimal) — a boundary shift across any
/// adjacent pair can't collide two distinct cache rows.
///
/// Fills are never format-parameterized (Figma's fills endpoint takes no
/// `format` query — a given `imageRef` always resolves to the same
/// bytes), so every fill lookup/store call passes `format = ""` here
/// regardless of the format actually discovered on download; only render
/// keys vary by the caller's requested format.
pub fn image_key(kind: &str, subject: &str, format: &str, scale_milli: u32) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let scale_str = scale_milli.to_string();
    let mut hash = OFFSET_BASIS;
    for byte in kind
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0u8))
        .chain(subject.as_bytes())
        .chain(std::iter::once(&0u8))
        .chain(format.as_bytes())
        .chain(std::iter::once(&0u8))
        .chain(scale_str.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// `scale` (a caller-facing `f64`, e.g. `1.5`) as the integer milli-scale
/// [`ImageBlobRec::scale_milli`]/[`image_key`] store — `None` (Figma's own
/// default) is `1000` (1×).
pub fn scale_milli(scale: Option<f64>) -> u32 {
    match scale {
        Some(s) => (s * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32,
        None => 1000,
    }
}

/// Normalize and dedup a caller's raw id list — accepts anything
/// `ident::normalize_node_ref` does (bare ids, `0-1` dash form, Figma URLs
/// carrying `node-id=`, spec §2b) — preserving first-seen order. Callers
/// run this once and pass the same `Vec` to both [`scan`] and [`resolve`].
pub fn normalize_ids(ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for raw in ids {
        let id = normalize_node_ref(raw);
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }
    out
}

/// One fetched (or cache-hit) image: a node render or a fill image, ready
/// for a caller to write to disk and/or wrap as MCP content.
#[derive(Debug, Clone)]
pub struct ImageItem {
    /// Node id (`kind == RENDER`) or `imageRef` hash (`kind == FILL`).
    pub subject: String,
    pub kind: &'static str,
    pub format: String,
    /// Empty when `error.is_some()`.
    pub bytes: Vec<u8>,
    pub cached: bool,
    pub error: Option<String>,
}

impl ImageItem {
    /// This item's manifest row: `{"id"|"ref": subject, "kind", "format",
    /// "bytes": <byte count>, "cached", "error"?}` — `id` for a render,
    /// `ref` for a fill (spec §5: `[{id|ref, kind, path, bytes, cached}]`).
    /// Never carries raw bytes or a `path` — callers (`cli::images`,
    /// `serve.rs`'s tool wrapping) layer those in themselves, since only
    /// the CLI knows about `--out` files at all.
    pub fn manifest_entry(&self) -> Value {
        let mut obj = serde_json::Map::new();
        let subject_key = if self.kind == RENDER { "id" } else { "ref" };
        obj.insert(subject_key.to_string(), json!(self.subject));
        obj.insert("kind".to_string(), json!(self.kind));
        obj.insert("format".to_string(), json!(self.format));
        obj.insert("bytes".to_string(), json!(self.bytes.len()));
        obj.insert("cached".to_string(), json!(self.cached));
        if let Some(e) = &self.error {
            obj.insert("error".to_string(), json!(e));
        }
        Value::Object(obj)
    }
}

/// Everything the read-only half of image fetching can determine from the
/// mirror alone, with no network access: the current file version, every
/// render/fill cache hit, and the set of `imageRef`s the requested nodes'
/// raw `fills` reference — plus, when render reuse is opted in
/// (NWS-2958), each requested render's fingerprint and the render index
/// with its orphans. Produced by [`scan`], consumed by [`resolve`] —
/// see this module's doc comment for why they're split at all.
pub struct Scanned {
    version: String,
    render_hits: BTreeMap<String, ImageItem>,
    fill_refs: BTreeSet<String>,
    fill_hits: BTreeMap<String, ImageItem>,
    policy: RenderPolicy,
    /// Requested render id -> fingerprint (opted-in requests only; ids
    /// without one fall back to per-version caching).
    render_fps: BTreeMap<String, String>,
    /// Every stored render index row but the cleanup marker, by
    /// fingerprint. Read whatever the policy (metadata only, and empty
    /// unless a request ever opted in), so a per-version write can retire
    /// the same node's fingerprinted render.
    render_index: BTreeMap<String, RenderIndexRec>,
    /// Fingerprints in `render_index` no node has any more (found only
    /// when the orphan pass ran).
    orphans: BTreeSet<String>,
    /// Fingerprints this request serves from the render index.
    hit_fps: BTreeSet<String>,
    /// Requested render id -> fingerprints of its stored renders at this
    /// request's format and scale.
    same_node: BTreeMap<String, Vec<String>>,
    /// Whether the orphan pass ran, and the cleanup marker didn't already
    /// record this version and scheme (so `resolve` must record them).
    mark_cleanup: bool,
}

/// Whether a row fetched by its key hash actually belongs to the
/// requested `(kind, subject)` pair (and, for a render, the requested
/// `format`/`scale_milli` too) — the same "never trust the key hash
/// alone" discipline `cache::lookup` documents (I-3 there): a
/// non-cryptographic FNV-64 hash is trivially collidable, and a colliding
/// row must read as a miss, not get served as a different item's bytes.
/// Fills skip the format/scale check on purpose: [`image_key`] always
/// keys a fill's row with `format = ""`/`scale_milli = 1000` regardless
/// of the format actually discovered on download (see that function's
/// own doc comment), so checking the *stored* `format` there would reject
/// every legitimate fill hit, not just collisions.
fn matches_identity(
    rec: &ImageBlobRec,
    kind: &str,
    subject: &str,
    format: &str,
    scale_milli: u32,
) -> bool {
    if rec.kind != kind || rec.subject != subject {
        return false;
    }
    kind != RENDER || (rec.format == format && rec.scale_milli == scale_milli)
}

/// Read-only half of image fetching: cache lookups plus fill-ref
/// detection, against already-open table readers — call inside
/// `st.rtx(|tuple| images::scan(..))` at a concrete `open_store!` site.
/// `node_ids` must already be normalized/deduped ([`normalize_ids`]).
///
/// The mirrored version read here is captured in [`Scanned`] and is the
/// only version [`resolve`] uses — both as the render request's
/// `version` parameter and as the stored rows' `file_version` — so a
/// pull landing between the two halves can't relabel a fetch.
pub fn scan<R: Readable>(
    readers: &ScanReaders<'_, '_, R>,
    node_ids: &[String],
    format: &str,
    scale_milli: u32,
    policy: RenderPolicy,
) -> Scanned {
    scan_split(readers, node_ids, node_ids, format, scale_milli, policy)
}

/// [`scan`] with the renders and the fill scan asked for separately
/// (NWS-2952): `render_ids` are rendered, `fill_ids`' own `fills` are
/// scanned for imageRefs. `figmog assets export` renders its icons
/// without fetching their fills, and fetches image fills without
/// rendering the nodes that hold them; pass the same `render_ids` to
/// [`resolve`].
///
/// With `policy.reuse` (NWS-2958) a render is first looked up by its
/// [`render_fingerprint`] in the render index, whose blob must still hash
/// to its recorded digest; a miss there, or a node without a fingerprint,
/// falls back to the per-version lookup. `policy.refresh` skips every
/// render lookup.
pub fn scan_split<R: Readable>(
    readers: &ScanReaders<'_, '_, R>,
    render_ids: &[String],
    fill_ids: &[String],
    format: &str,
    scale_milli: u32,
    policy: RenderPolicy,
) -> Scanned {
    let ScanReaders {
        nodes,
        children,
        meta,
        images,
        pins,
        render_index,
        render_blobs,
    } = readers;
    let version = meta.get(&0).map(|m| m.version.clone()).unwrap_or_default();

    let mut fingerprinter = Fingerprinter::new(nodes, children);
    let index: BTreeMap<String, RenderIndexRec> = render_index
        .iter()
        .filter(|(key, _)| key != RENDER_CLEANUP_KEY)
        .collect();
    let mut same_node: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in index.values() {
        if row.format == format
            && row.scale_milli == scale_milli
            && render_ids.contains(&row.subject)
        {
            same_node
                .entry(row.subject.clone())
                .or_default()
                .push(row.fingerprint.clone());
        }
    }
    let mut render_fps: BTreeMap<String, String> = BTreeMap::new();
    let mut hit_fps: BTreeSet<String> = BTreeSet::new();
    let mut render_hits: BTreeMap<String, ImageItem> = BTreeMap::new();
    for id in render_ids {
        let fingerprint = if policy.reuse {
            fingerprinter.fingerprint(id, format, scale_milli)
        } else {
            None
        };
        if let Some(fp) = &fingerprint {
            render_fps.insert(id.clone(), fp.clone());
        }
        if policy.refresh {
            continue;
        }
        let fingerprinted = fingerprint
            .as_ref()
            .and_then(|fp| index.get(fp))
            .filter(|row| {
                row.scheme == RENDER_FINGERPRINT_SCHEME
                    && row.subject == *id
                    && row.format == format
                    && row.scale_milli == scale_milli
            })
            .and_then(|row| {
                render_blobs
                    .get(&row.sha256)
                    .filter(|blob| blob_is_valid(blob, row))
            });
        let bytes = match fingerprinted {
            Some(blob) => {
                hit_fps.extend(fingerprint);
                Some(blob.bytes)
            }
            None => {
                let key = image_key(RENDER, id, format, scale_milli);
                images
                    .get(&key)
                    .filter(|rec| {
                        rec.file_version == version
                            && matches_identity(rec, RENDER, id, format, scale_milli)
                            && pins.get(&key).is_some_and(|pin| {
                                pin.generation == RENDER_PIN_GENERATION
                                    && pin.file_version == rec.file_version
                            })
                    })
                    .map(|rec| rec.bytes)
            }
        };
        if let Some(bytes) = bytes {
            render_hits.insert(
                id.clone(),
                ImageItem {
                    subject: id.clone(),
                    kind: RENDER,
                    format: format.to_string(),
                    bytes,
                    cached: true,
                    error: None,
                },
            );
        }
    }

    // Orphan pass (opted in only): index rows whose subject's current
    // fingerprint (at the row's own format and scale) is no longer the
    // row's key. Orphans appear only when the mirror (or the scheme)
    // changes, so a request whose renders all hit skips it while the
    // cleanup marker still records this version and scheme.
    let marker_current = render_index
        .get(&RENDER_CLEANUP_KEY.to_string())
        .is_some_and(|m| m.scheme == RENDER_FINGERPRINT_SCHEME && m.file_version == version);
    let has_misses = render_ids.iter().any(|id| !render_hits.contains_key(id));
    let run_orphan_pass = policy.reuse && (has_misses || !marker_current);
    let orphans: BTreeSet<String> = if run_orphan_pass {
        index
            .iter()
            .filter(|(fp, row)| {
                row.scheme != RENDER_FINGERPRINT_SCHEME
                    || fingerprinter
                        .fingerprint(&row.subject, &row.format, row.scale_milli)
                        .as_ref()
                        != Some(*fp)
            })
            .map(|(fp, _)| fp.clone())
            .collect()
    } else {
        BTreeSet::new()
    };
    let mark_cleanup = run_orphan_pass && !marker_current && !version.is_empty();

    let mut fill_refs: BTreeSet<String> = BTreeSet::new();
    for id in fill_ids {
        if let Some(n) = nodes.get(id)
            && let Ok(raw) = serde_json::from_str::<Value>(&n.raw)
        {
            fill_refs.extend(extract_image_refs(&raw));
        }
    }

    let mut fill_hits: BTreeMap<String, ImageItem> = BTreeMap::new();
    for r in &fill_refs {
        let key = image_key(FILL, r, "", 1000);
        if let Some(rec) = images.get(&key)
            && rec.file_version == version
            && matches_identity(&rec, FILL, r, "", 1000)
        {
            fill_hits.insert(
                r.clone(),
                ImageItem {
                    subject: r.clone(),
                    kind: FILL,
                    format: rec.format.clone(),
                    bytes: rec.bytes.clone(),
                    cached: true,
                    error: None,
                },
            );
        }
    }

    Scanned {
        version,
        render_hits,
        fill_refs,
        fill_hits,
        policy,
        render_fps,
        render_index: index,
        orphans,
        hit_fps,
        same_node,
        mark_cleanup,
    }
}

/// Network + cache-write half of image fetching: fetch every render/fill
/// miss `scan` didn't already resolve — at most one `images_render` call
/// covering every render miss, at most one `file_image_fills` call
/// covering every distinct `imageRef` miss (spec §5) — cache the results,
/// and return every item in `node_ids`' request order followed by the
/// scanned fill refs' sorted order. `api: None` means "no Figma API token
/// available": cache hits still resolve fully; every miss gets a
/// missing-credential error instead of a network attempt. `file_key: None`
/// allows cache extraction from an explicit store without a known file
/// identity; misses report that missing identity, even if a token exists.
///
/// With render reuse opted in (the policy `scan` captured), a fetched
/// render that has a fingerprint is stored as a fingerprinted render
/// rather than a per-version row, and the render cache's eviction plan
/// ([`plan_render_eviction`]) is applied afterwards.
#[allow(clippy::too_many_arguments)]
pub fn resolve<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    scanned: Scanned,
    api: Option<&dyn FigmaApi>,
    download: &dyn Fn(&str) -> Result<Vec<u8>, ApiError>,
    file_key: Option<&str>,
    node_ids: &[String],
    format: &str,
    scale: Option<f64>,
) -> Vec<ImageItem> {
    let Scanned {
        version,
        mut render_hits,
        fill_refs,
        mut fill_hits,
        policy,
        render_fps,
        mut render_index,
        mut orphans,
        hit_fps,
        same_node,
        mark_cleanup,
    } = scanned;
    // Fingerprinted renders written by this call (NWS-2958) get insertion
    // sequence numbers after every stored row's.
    let mut next_seq = render_index.values().map(|row| row.seq).max().unwrap_or(0) + 1;
    let mut displaced: BTreeSet<String> = BTreeSet::new();
    // Renders this request serves or writes: never evicted by its quota.
    let mut protected: BTreeSet<String> = hit_fps;
    let mut wrote_fingerprinted = false;
    let scale_m = scale_milli(scale);
    let unavailable_reason = if file_key.is_none() {
        NO_FILE_KEY_MSG
    } else {
        NO_TOKEN_MSG
    };
    let api = api.zip(file_key);
    // NWS-2960: pin every render request to the version captured by
    // `scan` — the same value stored as each fetched row's `file_version`.
    let pinned_version = (!version.is_empty()).then_some(version.as_str());

    let render_misses: Vec<String> = node_ids
        .iter()
        .filter(|id| !render_hits.contains_key(*id))
        .cloned()
        .collect();
    if !render_misses.is_empty() {
        match api {
            None => {
                for id in &render_misses {
                    render_hits.insert(
                        id.clone(),
                        error_item(
                            RENDER,
                            id,
                            format.to_string(),
                            unavailable_reason.to_string(),
                        ),
                    );
                }
            }
            Some((api, key)) => {
                match api.images_render(key, &render_misses, format, scale, pinned_version) {
                    Ok(resp) => {
                        let url_map = resp.get("images").and_then(Value::as_object);
                        for id in &render_misses {
                            let url = url_map.and_then(|m| m.get(id)).and_then(Value::as_str);
                            let item = match url {
                                None => error_item(
                                    RENDER,
                                    id,
                                    format.to_string(),
                                    "no render available for this node".to_string(),
                                ),
                                Some(url) => match download(url) {
                                    Ok(bytes) => {
                                        if let Some(fp) = render_fps.get(id)
                                            && pinned_version.is_some()
                                        {
                                            // Opted-in reuse (NWS-2958): the
                                            // render is pinned to the version
                                            // the fingerprint was computed at.
                                            let row = RenderIndexRec {
                                                fingerprint: fp.clone(),
                                                scheme: RENDER_FINGERPRINT_SCHEME,
                                                subject: id.clone(),
                                                format: format.to_string(),
                                                scale_milli: scale_m,
                                                file_version: version.clone(),
                                                seq: next_seq,
                                                sha256: sha256_hex(&bytes),
                                                byte_len: bytes.len() as u64,
                                            };
                                            next_seq += 1;
                                            // Also drops the node's
                                            // per-version row, so no policy
                                            // can serve bytes older than
                                            // these (a refresh stays one).
                                            store_render(st, &row, &bytes);
                                            if let Some(old) = render_index.insert(fp.clone(), row)
                                            {
                                                displaced.insert(old.sha256);
                                            }
                                            protected.insert(fp.clone());
                                            wrote_fingerprinted = true;
                                        } else {
                                            let key = image_key(RENDER, id, format, scale_m);
                                            store_image(
                                                st,
                                                &key,
                                                RENDER,
                                                id,
                                                format,
                                                scale_m,
                                                &version,
                                                &bytes,
                                                pinned_version.is_some(),
                                            );
                                        }
                                        // Either way, this node's other
                                        // fingerprinted renders at this
                                        // format and scale are now older
                                        // than its newest bytes: retire
                                        // them.
                                        let current = render_fps.get(id);
                                        orphans.extend(
                                            same_node
                                                .get(id)
                                                .into_iter()
                                                .flatten()
                                                .filter(|fp| Some(*fp) != current)
                                                .cloned(),
                                        );
                                        ImageItem {
                                            subject: id.clone(),
                                            kind: RENDER,
                                            format: format.to_string(),
                                            bytes,
                                            cached: false,
                                            error: None,
                                        }
                                    }
                                    Err(e) => {
                                        error_item(RENDER, id, format.to_string(), e.to_string())
                                    }
                                },
                            };
                            render_hits.insert(id.clone(), item);
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        for id in &render_misses {
                            render_hits.insert(
                                id.clone(),
                                error_item(RENDER, id, format.to_string(), msg.clone()),
                            );
                        }
                    }
                }
            }
        }
    }

    // Render cache upkeep (NWS-2958): after a request that wrote a
    // fingerprinted render, retired or orphaned one, or ran the orphan
    // pass for a new mirror version.
    if wrote_fingerprinted || !orphans.is_empty() || mark_cleanup {
        let plan = plan_render_eviction(
            &render_index,
            &orphans,
            &protected,
            &displaced,
            policy.max_bytes,
        );
        st.wtx(|tx| {
            for fp in &plan.index {
                tx.remove(&Id::RenderIndex(fp.clone()));
            }
            for sha in &plan.blobs {
                tx.remove(&Id::RenderBlob(sha.clone()));
            }
            if mark_cleanup {
                tx.upsert(
                    &Id::RenderIndex(RENDER_CLEANUP_KEY.to_string()),
                    &Rec::RenderIndex(RenderIndexRec {
                        fingerprint: RENDER_CLEANUP_KEY.to_string(),
                        scheme: RENDER_FINGERPRINT_SCHEME,
                        subject: String::new(),
                        format: String::new(),
                        scale_milli: 0,
                        file_version: version.clone(),
                        seq: 0,
                        sha256: String::new(),
                        byte_len: 0,
                    }),
                );
            }
        });
    }

    let fill_misses: Vec<String> = fill_refs
        .iter()
        .filter(|r| !fill_hits.contains_key(*r))
        .cloned()
        .collect();
    if !fill_misses.is_empty() {
        match api {
            None => {
                for r in &fill_misses {
                    fill_hits.insert(
                        r.clone(),
                        error_item(FILL, r, String::new(), unavailable_reason.to_string()),
                    );
                }
            }
            Some((api, key)) => match api.file_image_fills(key) {
                Ok(resp) => {
                    // Figma's own response nests under `meta.images`; tolerate
                    // a bare `images` map too (this crate's own test fixtures
                    // don't need to reproduce the wrapper exactly).
                    let url_map = resp
                        .get("meta")
                        .and_then(|m| m.get("images"))
                        .and_then(Value::as_object)
                        .or_else(|| resp.get("images").and_then(Value::as_object));
                    for r in &fill_misses {
                        let url = url_map.and_then(|m| m.get(r)).and_then(Value::as_str);
                        let item = match url {
                            None => error_item(
                                FILL,
                                r,
                                String::new(),
                                "no fill image available for this imageRef".to_string(),
                            ),
                            Some(url) => match download(url) {
                                Ok(bytes) => {
                                    let fmt = infer_fill_format(url);
                                    let key = image_key(FILL, r, "", 1000);
                                    store_image(
                                        st, &key, FILL, r, &fmt, 1000, &version, &bytes, false,
                                    );
                                    ImageItem {
                                        subject: r.clone(),
                                        kind: FILL,
                                        format: fmt,
                                        bytes,
                                        cached: false,
                                        error: None,
                                    }
                                }
                                Err(e) => error_item(FILL, r, String::new(), e.to_string()),
                            },
                        };
                        fill_hits.insert(r.clone(), item);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    for r in &fill_misses {
                        fill_hits
                            .insert(r.clone(), error_item(FILL, r, String::new(), msg.clone()));
                    }
                }
            },
        }
    }

    let mut out = Vec::with_capacity(node_ids.len() + fill_refs.len());
    for id in node_ids {
        if let Some(item) = render_hits.remove(id) {
            out.push(item);
        }
    }
    for r in &fill_refs {
        if let Some(item) = fill_hits.remove(r) {
            out.push(item);
        }
    }
    out
}

fn error_item(kind: &'static str, subject: &str, format: String, error: String) -> ImageItem {
    ImageItem {
        subject: subject.to_string(),
        kind,
        format,
        bytes: Vec::new(),
        cached: false,
        error: Some(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn store_image<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    key: &str,
    kind: &str,
    subject: &str,
    format: &str,
    scale_milli: u32,
    file_version: &str,
    bytes: &[u8],
    pinned: bool,
) {
    let rec = ImageBlobRec {
        key_hash: key.to_string(),
        kind: kind.to_string(),
        subject: subject.to_string(),
        format: format.to_string(),
        scale_milli,
        file_version: file_version.to_string(),
        bytes: bytes.to_vec(),
    };
    st.wtx(|tx| {
        tx.upsert(
            &Id::ImageBlob(key.to_string()),
            &Rec::ImageBlob(rec.clone()),
        );
        // The marker is written (or cleared) in the same transaction as
        // the row it describes, so the two can never disagree.
        let pin_id = Id::ImagePin(key.to_string());
        if pinned {
            tx.upsert(
                &pin_id,
                &Rec::ImagePin(ImagePinRec {
                    generation: RENDER_PIN_GENERATION,
                    file_version: file_version.to_string(),
                }),
            );
        } else {
            tx.remove(&pin_id);
        }
    });
}

/// Write one fingerprinted render (NWS-2958): its immutable,
/// content-addressed blob and its index row, in one transaction that also
/// drops the node's per-version row at the same format and scale.
fn store_render<P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    row: &RenderIndexRec,
    bytes: &[u8],
) {
    let per_version = image_key(RENDER, &row.subject, &row.format, row.scale_milli);
    st.wtx(|tx| {
        tx.remove(&Id::ImageBlob(per_version.clone()));
        tx.remove(&Id::ImagePin(per_version));
        tx.upsert(
            &Id::RenderBlob(row.sha256.clone()),
            &Rec::RenderBlob(RenderBlobRec {
                sha256: row.sha256.clone(),
                bytes: bytes.to_vec(),
            }),
        );
        tx.upsert(
            &Id::RenderIndex(row.fingerprint.clone()),
            &Rec::RenderIndex(row.clone()),
        );
    });
}

/// Distinct `imageRef` hashes among a raw node JSON's `fills` array
/// (`fills[].type == "IMAGE"`, `fills[].imageRef`) — the fill-detection
/// half of spec §5. Any other fill type (`SOLID`, `GRADIENT_*`) or a
/// missing/non-array `fills` yields nothing.
fn extract_image_refs(raw: &Value) -> Vec<String> {
    let Some(fills) = raw.get("fills").and_then(Value::as_array) else {
        return Vec::new();
    };
    fills
        .iter()
        .filter(|f| f.get("type").and_then(Value::as_str) == Some("IMAGE"))
        .filter_map(|f| f.get("imageRef").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Best-effort format for a fill's bytes, from its download URL's file
/// extension (Figma's fills endpoint carries no format of its own — see
/// [`image_key`]'s doc comment) — informational only, defaulting to
/// `"png"` when the URL has no recognizable extension.
fn infer_fill_format(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    if let Some(idx) = path.rfind('.') {
        let ext = &path[idx + 1..];
        if !ext.is_empty() && ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return ext.to_ascii_lowercase();
        }
    }
    "png".to_string()
}

fn mime_for(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "svg" => "image/svg+xml",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "image/png",
    }
}

/// Build the MCP `tools/call` result for `figmog_images` (spec §5):
/// content is always `[manifest text block, per-item blocks...]` — the
/// manifest first and always text, so a socket-routed CLI client (see
/// `cli::images`) can find it at `content[0]` unambiguously regardless of
/// how many (if any) data-bearing blocks follow. Per successful item, in
/// the same order as `items`:
///
/// - **SVG** (`item.format` case-insensitively `"svg"`) ships as a
///   **`text`** block carrying the raw markup verbatim — never base64 nor
///   `image/svg+xml` (spec ruling): raw SVG text is more directly useful
///   to an agent generating HTML/CSS than an opaque image blob, and MCP
///   clients render `image/svg+xml` image blocks inconsistently. The
///   block also carries a `mimeType: "image/svg+xml"` field (informational
///   only — MCP's `text` content type doesn't define one) so a consumer
///   can tell an actual-SVG-content block apart from an oversized note
///   below without guessing from prose.
/// - **Every other format** ships as an `image` block, base64-encoded.
///
/// Either way, over [`CONTENT_SIZE_CAP`] (measured on the *encoded*
/// payload — the block's own `text`/`data` string — not the raw byte
/// count; see that constant's doc comment) gets a plain text note instead
/// (no `mimeType`, the marker above's absence is what makes it
/// distinguishable from a real SVG block), pointing at `figmog images
/// --out`. An error item gets neither — its manifest entry already
/// carries the reason.
pub fn to_mcp_content(items: &[ImageItem]) -> Value {
    let manifest: Vec<Value> = items.iter().map(ImageItem::manifest_entry).collect();
    let mut content = vec![json!({
        "type": "text",
        "text": serde_json::to_string(&manifest).unwrap_or_default(),
    })];
    for item in items {
        if item.error.is_some() {
            continue;
        }
        let subject_key = if item.kind == RENDER { "id" } else { "ref" };
        let is_svg = item.format.eq_ignore_ascii_case("svg");

        // Non-standard extra field(s) on every arm below, ignored by MCP
        // clients that don't know them — let a socket-routed CLI caller
        // (`cli::images`, the only consumer that reads them) match a
        // block back to its manifest row, and tell payload from note
        // apart, without relying on content order alone (an oversized
        // item's block sits at the same position a payload block would).
        let mut block = if is_svg {
            let text = String::from_utf8_lossy(&item.bytes).into_owned();
            if text.len() <= CONTENT_SIZE_CAP {
                serde_json::Map::from_iter([
                    ("type".to_string(), json!("text")),
                    ("text".to_string(), json!(text)),
                    ("mimeType".to_string(), json!(mime_for(&item.format))),
                ])
            } else {
                oversized_note(item, subject_key, text.len())
            }
        } else {
            let encoded = base64_encode(&item.bytes);
            if encoded.len() <= CONTENT_SIZE_CAP {
                serde_json::Map::from_iter([
                    ("type".to_string(), json!("image")),
                    ("data".to_string(), json!(encoded)),
                    ("mimeType".to_string(), json!(mime_for(&item.format))),
                ])
            } else {
                oversized_note(item, subject_key, encoded.len())
            }
        };
        block.insert(subject_key.to_string(), json!(item.subject));
        content.push(Value::Object(block));
    }
    json!({"content": content, "isError": false})
}

/// The "too large to inline" text block for one item — deliberately never
/// carries a `mimeType` field, the signal [`to_mcp_content`]'s consumers
/// use to tell "this text block IS the payload" (an SVG success) from
/// "this text block is a note" (this function's output).
/// `encoded_len` is whatever [`to_mcp_content`] actually measured against
/// [`CONTENT_SIZE_CAP`] (an encoded byte count, not necessarily
/// `item.bytes.len()` — see that function's own doc comment).
fn oversized_note(
    item: &ImageItem,
    subject_key: &str,
    encoded_len: usize,
) -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([
        ("type".to_string(), json!("text")),
        (
            "text".to_string(),
            json!(format!(
                "{} {}={} is {encoded_len} bytes encoded, over the 1MB inline cap — run `figmog images {} --out <dir>` to download it to disk.",
                item.kind, subject_key, item.subject, item.subject
            )),
        ),
    ])
}

// ---- base64 (no crate dependency — see `api::download_bytes`'s doc
// comment for why the MCP wire format needs base64 at all) ----

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(B64_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub(crate) fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(b: u8) -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok((b - b'A') as u32),
            b'a'..=b'z' => Ok((b - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((b - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 byte: {b}")),
        }
    }
    let s = s.trim();
    if !s.len().is_multiple_of(4) {
        return Err("base64 input length must be a multiple of 4".to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        let mut n: u32 = 0;
        for &b in chunk {
            n <<= 6;
            if b != b'=' {
                n |= val(b)?;
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    use crate::api::FileMetaResp;
    use crate::model::NodeRec;

    // ---- image_key ----

    #[test]
    fn image_key_is_deterministic() {
        assert_eq!(
            image_key(RENDER, "1:2", "png", 1000),
            image_key(RENDER, "1:2", "png", 1000)
        );
    }

    #[test]
    fn image_key_distinguishes_kind_subject_format_and_scale() {
        let base = image_key(RENDER, "1:2", "png", 1000);
        assert_ne!(base, image_key(FILL, "1:2", "png", 1000));
        assert_ne!(base, image_key(RENDER, "1:3", "png", 1000));
        assert_ne!(base, image_key(RENDER, "1:2", "svg", 1000));
        assert_ne!(base, image_key(RENDER, "1:2", "png", 2000));
    }

    #[test]
    fn image_key_distinguishes_boundary_shift() {
        // Same separator-discipline proof as `cache::cache_key`'s own test.
        assert_ne!(
            image_key("ab", "c", "", 1000),
            image_key("a", "bc", "", 1000)
        );
    }

    #[test]
    fn scale_milli_defaults_to_1000_and_rounds() {
        assert_eq!(scale_milli(None), 1000);
        assert_eq!(scale_milli(Some(1.0)), 1000);
        assert_eq!(scale_milli(Some(1.5)), 1500);
        assert_eq!(scale_milli(Some(0.333)), 333);
    }

    #[test]
    fn normalize_ids_dedups_and_normalizes_dash_form() {
        let ids = vec!["0-1".to_string(), "0:1".to_string(), "1-2".to_string()];
        assert_eq!(
            normalize_ids(&ids),
            vec!["0:1".to_string(), "1:2".to_string()]
        );
    }

    // ---- extract_image_refs / infer_fill_format ----

    #[test]
    fn extract_image_refs_finds_image_fills_and_ignores_others() {
        let raw = json!({
            "fills": [
                {"type": "SOLID", "color": {"r": 1}},
                {"type": "IMAGE", "imageRef": "abc123"},
                {"type": "IMAGE", "imageRef": "def456"},
            ]
        });
        let mut refs = extract_image_refs(&raw);
        refs.sort();
        assert_eq!(refs, vec!["abc123".to_string(), "def456".to_string()]);
    }

    #[test]
    fn extract_image_refs_empty_when_no_fills() {
        assert_eq!(extract_image_refs(&json!({})), Vec::<String>::new());
    }

    #[test]
    fn infer_fill_format_reads_the_url_extension() {
        assert_eq!(infer_fill_format("https://s3/x/y.jpg?sig=1"), "jpg");
        assert_eq!(infer_fill_format("https://s3/x/y.PNG"), "png");
        assert_eq!(infer_fill_format("https://s3/x/y"), "png");
    }

    // ---- base64 ----

    #[test]
    fn base64_round_trips_classic_vectors() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(raw.as_bytes()), encoded, "encode {raw:?}");
            assert_eq!(
                base64_decode(encoded).unwrap(),
                raw.as_bytes().to_vec(),
                "decode {encoded:?}"
            );
        }
    }

    // ---- manifest_entry / to_mcp_content ----

    #[test]
    fn manifest_entry_uses_id_for_render_and_ref_for_fill() {
        let render = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![1, 2, 3],
            cached: false,
            error: None,
        };
        assert_eq!(
            render.manifest_entry(),
            json!({"id": "1:2", "kind": "render", "format": "png", "bytes": 3, "cached": false})
        );

        let fill = ImageItem {
            subject: "abc".into(),
            kind: FILL,
            format: "jpg".into(),
            bytes: vec![],
            cached: true,
            error: Some("boom".into()),
        };
        assert_eq!(
            fill.manifest_entry(),
            json!({"ref": "abc", "kind": "fill", "format": "jpg", "bytes": 0, "cached": true, "error": "boom"})
        );
    }

    #[test]
    fn to_mcp_content_orders_manifest_first_then_images_skipping_errors_and_oversized() {
        let ok = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![1, 2, 3],
            cached: false,
            error: None,
        };
        let failed = ImageItem {
            subject: "1:3".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![],
            cached: false,
            error: Some("rate limited".into()),
        };
        let oversized = ImageItem {
            subject: "1:4".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![0u8; CONTENT_SIZE_CAP + 1],
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[ok.clone(), failed, oversized]);
        let content = out["content"].as_array().unwrap();
        // manifest first, always text
        assert_eq!(content[0]["type"], json!("text"));
        let manifest: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(manifest.as_array().unwrap().len(), 3);
        // one image block for the successful, under-cap item, tagged with
        // its node id so a socket-routed CLI caller can match it back up.
        assert_eq!(content[1]["type"], json!("image"));
        assert_eq!(content[1]["mimeType"], json!("image/png"));
        assert_eq!(content[1]["data"], json!(base64_encode(&ok.bytes)));
        assert_eq!(content[1]["id"], json!("1:2"));
        // one text note for the oversized item, also tagged; nothing at
        // all for the error item.
        assert_eq!(content[2]["type"], json!("text"));
        assert!(content[2]["text"].as_str().unwrap().contains("--out"));
        assert_eq!(content[2]["id"], json!("1:4"));
        assert_eq!(content.len(), 3);
        assert_eq!(out["isError"], json!(false));
    }

    /// I2: the cap must bound the *encoded* payload, not the raw byte
    /// count — 800,000 raw bytes is comfortably under
    /// `CONTENT_SIZE_CAP` (1,048,576), but base64's ~4/3 inflation (RFC
    /// 4648) pushes the actual `data` string this item would ship as over
    /// it, so it must still fall back to the oversized note.
    #[test]
    fn to_mcp_content_caps_on_encoded_length_not_raw_bytes() {
        let raw_len = 800_000;
        assert!(raw_len <= CONTENT_SIZE_CAP, "sanity: raw is under the cap");
        let encoded_len = base64_encode(&vec![0u8; raw_len]).len();
        assert!(
            encoded_len > CONTENT_SIZE_CAP,
            "sanity: encoded crosses the cap"
        );

        let item = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "png".into(),
            bytes: vec![0u8; raw_len],
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[item]);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(
            content[1]["type"],
            json!("text"),
            "must fall back to the oversized note, not an image block, once the encoded size crosses the cap"
        );
        assert!(
            content[1]["mimeType"].is_null(),
            "an oversized note must never carry mimeType — that field is the payload marker"
        );
    }

    /// SVG ruling: an SVG render ships as raw markup in a `text` block
    /// (never base64/`image/svg+xml`), tagged with `mimeType:
    /// "image/svg+xml"` so a consumer can tell it apart from an oversized
    /// note (also `text`, but with no `mimeType`).
    #[test]
    fn to_mcp_content_ships_svg_as_raw_text_markup_not_base64_image() {
        let svg = "<svg><rect/></svg>";
        let item = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "svg".into(),
            bytes: svg.as_bytes().to_vec(),
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[item]);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], json!("text"));
        assert_eq!(content[1]["text"], json!(svg));
        assert_eq!(content[1]["mimeType"], json!("image/svg+xml"));
        assert_eq!(content[1]["id"], json!("1:2"));
    }

    /// SVG ruling, size-cap half: the cap still applies to SVG text, same
    /// encoded-length comparison — for SVG there's no base64 step, so
    /// "encoded length" is just the markup's own UTF-8 byte length.
    #[test]
    fn to_mcp_content_oversized_svg_falls_back_to_a_note_without_mimetype() {
        let item = ImageItem {
            subject: "1:2".into(),
            kind: RENDER,
            format: "svg".into(),
            bytes: vec![b'a'; CONTENT_SIZE_CAP + 1],
            cached: false,
            error: None,
        };
        let out = to_mcp_content(&[item]);
        let content = out["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], json!("text"));
        assert!(
            content[1]["mimeType"].is_null(),
            "an oversized SVG note must not carry mimeType either"
        );
        assert!(content[1]["text"].as_str().unwrap().contains("--out"));
    }

    // ---- scan + resolve: scripted FigmaApi fake ----

    /// Pops one scripted response per call (same pattern as
    /// `watch::tests::Script`), and records every call it received so
    /// tests can assert on dedup/call counts.
    struct Script {
        render_responses: RefCell<Vec<Result<Value, ApiError>>>,
        fills_responses: RefCell<Vec<Result<Value, ApiError>>>,
        render_calls: RefCell<Vec<Vec<String>>>,
        render_versions: RefCell<Vec<Option<String>>>,
        fills_calls: RefCell<u32>,
    }
    impl Script {
        fn new() -> Self {
            Script {
                render_responses: RefCell::new(Vec::new()),
                fills_responses: RefCell::new(Vec::new()),
                render_calls: RefCell::new(Vec::new()),
                render_versions: RefCell::new(Vec::new()),
                fills_calls: RefCell::new(0),
            }
        }
    }
    impl FigmaApi for Script {
        fn file_meta(&self, _key: &str) -> Result<FileMetaResp, ApiError> {
            unimplemented!("not exercised by images::resolve")
        }
        fn file(&self, _key: &str, _geometry: bool) -> Result<Value, ApiError> {
            unimplemented!("not exercised by images::resolve")
        }
        fn images_render(
            &self,
            _key: &str,
            ids: &[String],
            _format: &str,
            _scale: Option<f64>,
            version: Option<&str>,
        ) -> Result<Value, ApiError> {
            self.render_calls.borrow_mut().push(ids.to_vec());
            self.render_versions
                .borrow_mut()
                .push(version.map(str::to_string));
            self.render_responses.borrow_mut().remove(0)
        }
        fn file_image_fills(&self, _key: &str) -> Result<Value, ApiError> {
            *self.fills_calls.borrow_mut() += 1;
            self.fills_responses.borrow_mut().remove(0)
        }
    }

    #[allow(clippy::type_complexity)]
    fn downloader(
        bytes: HashMap<String, Vec<u8>>,
    ) -> (
        impl Fn(&str) -> Result<Vec<u8>, ApiError>,
        std::rc::Rc<RefCell<u32>>,
    ) {
        let calls = std::rc::Rc::new(RefCell::new(0u32));
        let calls2 = calls.clone();
        let f = move |url: &str| -> Result<Vec<u8>, ApiError> {
            *calls2.borrow_mut() += 1;
            bytes
                .get(url)
                .cloned()
                .ok_or_else(|| ApiError::Network(format!("no fixture bytes for {url}")))
        };
        (f, calls)
    }

    fn insert_node<P: Push<Keyed<Id, Rec>>>(
        st: &mut KeyedStream<Id, Rec, P>,
        node_id: &str,
        raw: Value,
    ) {
        st.wtx(|tx| {
            tx.upsert(
                &Id::Node(node_id.to_string()),
                &Rec::Node(NodeRec {
                    id: node_id.to_string(),
                    parent_id: Some("0:1".into()),
                    child_index: 0,
                    page_id: "0:1".into(),
                    node_type: "RECTANGLE".into(),
                    name: "N".into(),
                    visible: true,
                    text: None,
                    component_id: None,
                    component_properties: vec![],
                    property_definitions: None,
                    style_refs: vec![],
                    bound_variables: vec![],
                    abs_bounds: None,
                    raw: raw.to_string(),
                }),
            );
        });
    }

    fn insert_meta<P: Push<Keyed<Id, Rec>>>(st: &mut KeyedStream<Id, Rec, P>, version: &str) {
        st.wtx(|tx| {
            tx.upsert(
                &Id::Meta,
                &Rec::Meta(crate::model::FileMeta {
                    name: "F".into(),
                    version: version.to_string(),
                    last_modified: "t".into(),
                    synced_at_unix_ms: 0,
                }),
            );
        });
    }

    /// Seed `rec` under `key` together with a valid render pin marker
    /// (current generation, matching `file_version`), so the marker check
    /// passes and only `rec`'s own identity fields can reject a hit.
    fn seed_pinned_row<P: Push<Keyed<Id, Rec>>>(
        st: &mut KeyedStream<Id, Rec, P>,
        key: String,
        rec: ImageBlobRec,
    ) {
        st.wtx(|tx| {
            tx.upsert(
                &Id::ImagePin(key.clone()),
                &Rec::ImagePin(ImagePinRec {
                    generation: RENDER_PIN_GENERATION,
                    file_version: rec.file_version.clone(),
                }),
            );
            tx.upsert(&Id::ImageBlob(key), &Rec::ImageBlob(rec));
        });
    }

    // ---- I-3-style collision safety (mirrors cache.rs's own test) ----

    /// A row that collides on `image_key`'s hash but actually belongs to a
    /// different `(kind, subject)` pair must read as a miss, not get
    /// served as a different item's bytes — exactly the guarantee
    /// `cache::lookup`'s own collision test proves for `proxy_cache`.
    #[test]
    fn scan_misses_on_collision_where_key_matches_but_kind_and_subject_dont() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_meta(&mut st, "100");

        let requested_id = "1:2".to_string();
        let key = image_key(RENDER, &requested_id, "png", 1000);
        // Constructed directly under the exact key a render lookup for
        // `requested_id` will compute, but tagged as a *fill* for a
        // different subject — exactly what a real FNV-64 collision would
        // look like, without needing to actually find one.
        let colliding = ImageBlobRec {
            key_hash: key.clone(),
            kind: FILL.to_string(),
            subject: "some-other-ref".to_string(),
            format: "jpg".to_string(),
            scale_milli: 1000,
            file_version: "100".to_string(),
            bytes: vec![9, 9, 9],
        };
        seed_pinned_row(&mut st, key, colliding);

        let ids = vec![requested_id.clone()];
        let scanned = st.rtx(|readers| {
            crate::with_scan_readers!(readers, |r| scan(
                &r,
                &ids,
                "png",
                1000,
                RenderPolicy::default()
            ))
        });
        assert!(
            !scanned.render_hits.contains_key(&requested_id),
            "a key-hash collision must never be served as a cache hit"
        );
    }

    /// Same collision guarantee, the render-specific half: a row under the
    /// right key with the *right* kind/subject but a different requested
    /// format or scale must still read as a miss — `image_key` folds
    /// format/scale into the hash, but a real collision could still land
    /// two distinct `(format, scale)` requests on the same bucket.
    #[test]
    fn scan_render_hit_requires_matching_format_and_scale() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_meta(&mut st, "100");

        let requested_id = "1:2".to_string();
        let key = image_key(RENDER, &requested_id, "png", 1000);
        let colliding = ImageBlobRec {
            key_hash: key.clone(),
            kind: RENDER.to_string(),
            subject: requested_id.clone(),
            format: "svg".to_string(), // requested "png"
            scale_milli: 2000,         // requested 1000
            file_version: "100".to_string(),
            bytes: vec![9, 9, 9],
        };
        seed_pinned_row(&mut st, key, colliding);

        let ids = vec![requested_id.clone()];
        let scanned = st.rtx(|readers| {
            crate::with_scan_readers!(readers, |r| scan(
                &r,
                &ids,
                "png",
                1000,
                RenderPolicy::default()
            ))
        });
        assert!(
            !scanned.render_hits.contains_key(&requested_id),
            "a format/scale mismatch under the same key must never be served"
        );
    }

    /// `scan` then `resolve`, exactly the two-step call every production
    /// call site makes (this module's own doc comment) — the one seam
    /// this test suite proves end to end.
    macro_rules! scan_and_resolve {
        ($st:expr, $api:expr, $download:expr, $ids:expr, $format:expr, $scale:expr) => {
            scan_and_resolve!(
                $st,
                $api,
                $download,
                $ids,
                $format,
                $scale,
                RenderPolicy::default()
            )
        };
        ($st:expr, $api:expr, $download:expr, $ids:expr, $format:expr, $scale:expr, $policy:expr) => {{
            let scanned = $st.rtx(|readers| {
                crate::with_scan_readers!(readers, |r| scan(
                    &r,
                    $ids,
                    $format,
                    scale_milli($scale),
                    $policy
                ))
            });
            resolve(
                &mut $st,
                scanned,
                $api,
                $download,
                Some("FILE"),
                $ids,
                $format,
                $scale,
            )
        }};
    }

    #[test]
    fn resolve_render_miss_downloads_and_caches_then_a_repeat_call_hits_cache_with_zero_api_calls()
    {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_meta(&mut st, "100");

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/render.png"}})));
        let (download, download_calls) = downloader(HashMap::from([(
            "https://s3/render.png".to_string(),
            vec![1, 2, 3, 4],
        )]));

        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].bytes, vec![1, 2, 3, 4]);
        assert!(!items[0].cached);
        assert!(items[0].error.is_none());
        assert_eq!(*download_calls.borrow(), 1);
        assert_eq!(script.render_calls.borrow().len(), 1);

        // Second call: cache hit. `Script`'s render_responses is now empty,
        // so a second `images_render` call would panic on `.remove(0)` —
        // proving zero API calls happened, not just asserting a counter.
        let items2 = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items2.len(), 1);
        assert_eq!(items2[0].bytes, vec![1, 2, 3, 4]);
        assert!(items2[0].cached);
        assert_eq!(
            *download_calls.borrow(),
            1,
            "cache hit must not re-download"
        );
    }

    #[test]
    fn resolve_detects_fill_image_refs_and_fetches_the_map_at_most_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(
            &mut st,
            "1:2",
            json!({"fills": [{"type": "IMAGE", "imageRef": "hash1"}]}),
        );
        insert_meta(&mut st, "100");

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/render.png"}})));
        script.fills_responses.borrow_mut().push(Ok(
            json!({"meta": {"images": {"hash1": "https://s3/fill.jpg"}}}),
        ));
        let (download, _calls) = downloader(HashMap::from([
            ("https://s3/render.png".to_string(), vec![9]),
            ("https://s3/fill.jpg".to_string(), vec![5, 5, 5]),
        ]));

        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 2, "one render + one fill");
        let fill = items.iter().find(|i| i.kind == FILL).unwrap();
        assert_eq!(fill.subject, "hash1");
        assert_eq!(fill.bytes, vec![5, 5, 5]);
        assert_eq!(fill.format, "jpg");
        assert_eq!(*script.fills_calls.borrow(), 1);

        // Cache hit path for the fill: `fills_responses` is now empty, so a
        // second call would panic if the fill map were re-fetched.
        let items2 = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        let fill2 = items2.iter().find(|i| i.kind == FILL).unwrap();
        assert!(fill2.cached);
        assert_eq!(*script.fills_calls.borrow(), 1);
    }

    #[test]
    fn resolve_records_a_missing_render_url_as_a_per_item_error_without_failing_other_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_node(&mut st, "1:3", json!({}));
        insert_meta(&mut st, "100");

        let script = Script::new();
        // Figma omits 1:3 from the map — not renderable.
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/render.png"}})));
        let (download, _calls) = downloader(HashMap::from([(
            "https://s3/render.png".to_string(),
            vec![7],
        )]));

        let ids = vec!["1:2".to_string(), "1:3".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 2);
        let ok = items.iter().find(|i| i.subject == "1:2").unwrap();
        assert!(ok.error.is_none());
        assert_eq!(ok.bytes, vec![7]);
        let missing = items.iter().find(|i| i.subject == "1:3").unwrap();
        assert!(missing.error.is_some());
        assert!(missing.bytes.is_empty());
    }

    #[test]
    fn resolve_without_api_serves_cache_hits_and_marks_misses_with_the_no_token_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_node(&mut st, "1:3", json!({}));
        insert_meta(&mut st, "100");
        // Pre-seed a cache hit for 1:2 directly (as if fetched earlier).
        {
            let key = image_key(RENDER, "1:2", "png", 1000);
            store_image(
                &mut st,
                &key,
                RENDER,
                "1:2",
                "png",
                1000,
                "100",
                &[1, 1],
                true,
            );
        }
        let (download, download_calls) = downloader(HashMap::new());

        let ids = vec!["1:2".to_string(), "1:3".to_string()];
        let items = scan_and_resolve!(st, None, &download, &ids, "png", None);
        assert_eq!(items.len(), 2);
        let cached = items.iter().find(|i| i.subject == "1:2").unwrap();
        assert!(cached.cached);
        assert_eq!(cached.bytes, vec![1, 1]);
        let miss = items.iter().find(|i| i.subject == "1:3").unwrap();
        assert_eq!(miss.error.as_deref(), Some(NO_TOKEN_MSG));
        let message = miss.error.as_deref().unwrap();
        assert!(message.contains("empty FIGMA_TOKEN disables saved credentials"));
        assert!(message.contains("unset it to use saved credentials"));
        assert!(message.contains("figmog auth login"));
        assert!(message.contains("set FIGMA_TOKEN"));
        assert_eq!(*download_calls.borrow(), 0);
    }

    /// NWS-2960: a render requested while the mirror is at "100" asks
    /// Figma for version "100" — even if Figma's latest has since moved on
    /// — and the cached row is labelled and marked with that same version.
    #[test]
    fn resolve_pins_the_render_request_to_the_mirrored_version_and_marks_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_meta(&mut st, "100");

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/v100.png"}})));
        let (download, _calls) = downloader(HashMap::from([(
            "https://s3/v100.png".to_string(),
            vec![1, 0, 0],
        )]));

        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items[0].bytes, vec![1, 0, 0]);
        assert_eq!(
            *script.render_versions.borrow(),
            vec![Some("100".to_string())],
            "the render request must carry the mirrored version"
        );

        let key = image_key(RENDER, "1:2", "png", 1000);
        let (row, pin) = st.rtx(|(_, _, _, _, _, _, _, _, _, images, pins, _, _, _)| {
            (images.get(&key), pins.get(&key))
        });
        assert_eq!(row.unwrap().file_version, "100");
        assert_eq!(
            pin,
            Some(ImagePinRec {
                generation: RENDER_PIN_GENERATION,
                file_version: "100".to_string(),
            })
        );
    }

    /// NWS-2960 upgrade path: a render row an older binary stored (fetched
    /// unpinned, so possibly showing a newer design than its "100" label)
    /// carries no pin marker. It must be refetched, not served — and the
    /// refetched row is marked, so the next request is a normal hit.
    #[test]
    fn an_unmarked_legacy_render_row_is_refetched_instead_of_served() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(
            &mut st,
            "1:2",
            json!({"fills": [{"type": "IMAGE", "imageRef": "hash1"}]}),
        );
        insert_meta(&mut st, "100");
        let render_key = image_key(RENDER, "1:2", "png", 1000);
        let fill_key = image_key(FILL, "hash1", "", 1000);
        st.wtx(|tx| {
            // Falsely labelled legacy render: same version, no marker.
            tx.upsert(
                &Id::ImageBlob(render_key.clone()),
                &Rec::ImageBlob(ImageBlobRec {
                    key_hash: render_key.clone(),
                    kind: RENDER.to_string(),
                    subject: "1:2".to_string(),
                    format: "png".to_string(),
                    scale_milli: 1000,
                    file_version: "100".to_string(),
                    bytes: vec![6, 6, 6],
                }),
            );
            // Legacy fill row: content-addressed, stays valid unmarked.
            tx.upsert(
                &Id::ImageBlob(fill_key.clone()),
                &Rec::ImageBlob(ImageBlobRec {
                    key_hash: fill_key.clone(),
                    kind: FILL.to_string(),
                    subject: "hash1".to_string(),
                    format: "jpg".to_string(),
                    scale_milli: 1000,
                    file_version: "100".to_string(),
                    bytes: vec![5, 5],
                }),
            );
        });

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({"images": {"1:2": "https://s3/v100.png"}})));
        let (download, download_calls) = downloader(HashMap::from([(
            "https://s3/v100.png".to_string(),
            vec![1, 0, 0],
        )]));

        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        let render = items.iter().find(|i| i.kind == RENDER).unwrap();
        assert!(!render.cached, "an unmarked legacy render must be a miss");
        assert_eq!(render.bytes, vec![1, 0, 0]);
        assert_eq!(
            *script.render_versions.borrow(),
            vec![Some("100".to_string())]
        );
        let fill = items.iter().find(|i| i.kind == FILL).unwrap();
        assert!(fill.cached, "an unmarked fill row is still a hit");
        assert_eq!(fill.bytes, vec![5, 5]);
        assert_eq!(*script.fills_calls.borrow(), 0);

        // Repeat: the refetched row is marked, so it's a hit with zero
        // API calls (`render_responses` is empty — a call would panic).
        let items2 = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        let render2 = items2.iter().find(|i| i.kind == RENDER).unwrap();
        assert!(render2.cached);
        assert_eq!(render2.bytes, vec![1, 0, 0]);
        assert_eq!(*download_calls.borrow(), 1);
    }

    #[test]
    fn resolve_records_a_429_uniformly_across_every_render_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        insert_node(&mut st, "1:2", json!({}));
        insert_meta(&mut st, "100");

        let script = Script::new();
        script
            .render_responses
            .borrow_mut()
            .push(Err(ApiError::RateLimited {
                retry_after: std::time::Duration::from_secs(30),
            }));
        let (download, _calls) = downloader(HashMap::new());
        let ids = vec!["1:2".to_string()];
        let items = scan_and_resolve!(st, Some(&script), &download, &ids, "png", None);
        assert_eq!(items.len(), 1);
        let err = items[0].error.as_deref().unwrap();
        assert!(err.contains("rate limited"), "{err}");
    }

    // ---- NWS-2958: fingerprinted render reuse ----

    /// What a synthetic file's editable parts hold; `Default` is v1.
    #[derive(Clone)]
    struct Design {
        version: &'static str,
        /// Frame A's text (`1:2` characters).
        title: &'static str,
        /// Frame A's text font.
        font: &'static str,
        /// Main component `3:1`'s fill red channel — also resolved into its
        /// instance `4:2`, as the REST response would.
        main_red: f64,
        /// Main component child `3:2`'s text — deliberately *not*
        /// propagated into the instance's expansion, to prove the
        /// main-component dependency alone invalidates.
        main_label: &'static str,
        /// Frame D's child image fill content hash.
        image_ref: &'static str,
        /// Frame E's explicit variable mode, inherited by `6:2`.
        mode: &'static str,
        /// Frame B's name.
        b_name: &'static str,
        /// Frame A's page x position (its children move with it).
        a_x: f64,
        /// Frame A's text width.
        title_width: f64,
        /// Frame A's prototype link target (prototype-only).
        link: &'static str,
    }

    impl Default for Design {
        fn default() -> Self {
            Design {
                version: "v1",
                title: "Hello",
                font: "Inter",
                main_red: 0.1,
                main_label: "Go",
                image_ref: "img1",
                mode: "light",
                b_name: "B",
                a_x: 0.1,
                title_width: 120.0,
                link: "2:1",
            }
        }
    }

    /// A synthetic `GET /v1/files/:key` response: frames A (text), B,
    /// C (an instance of main component `3:1`), D (an image fill), E
    /// (explicit variable mode) and a masked group.
    fn design_file(d: &Design) -> Value {
        json!({
            "name": "Synthetic",
            "version": d.version,
            "lastModified": "2026-11-01T00:00:00Z",
            "document": {"id": "0:0", "type": "DOCUMENT", "name": "Document", "children": [
                {"id": "0:1", "type": "CANVAS", "name": "Page", "children": [
                    {"id": "1:1", "type": "FRAME", "name": "A",
                     "absoluteBoundingBox": {"x": d.a_x, "y": 40.0, "width": 200.0, "height": 100.0},
                     "absoluteRenderBounds": {"x": d.a_x - 2.0, "y": 38.0, "width": 204.0, "height": 104.0},
                     "absoluteTransform": [[1.0, 0.0, d.a_x], [0.0, 1.0, 40.0]],
                     "relativeTransform": [[1.0, 0.0, d.a_x], [0.0, 1.0, 40.0]],
                     "interactions": [{"trigger": {"type": "ON_CLICK"},
                                       "actions": [{"type": "NODE", "destinationId": d.link}]}],
                     "transitionNodeID": d.link,
                     "children": [
                        {"id": "1:2", "type": "TEXT", "name": "Title", "characters": d.title,
                         "style": {"fontFamily": d.font, "fontSize": 16.0},
                         "absoluteBoundingBox": {"x": d.a_x + 10.3, "y": 50.7, "width": d.title_width, "height": 20.0},
                         "absoluteRenderBounds": {"x": d.a_x + 10.3, "y": 50.7, "width": d.title_width, "height": 20.0},
                         "absoluteTransform": [[1.0, 0.0, d.a_x + 10.3], [0.0, 1.0, 50.7]],
                         "relativeTransform": [[1.0, 0.0, 10.3], [0.0, 1.0, 10.7]]}
                    ]},
                    {"id": "2:1", "type": "FRAME", "name": d.b_name, "children": [
                        {"id": "2:2", "type": "RECTANGLE", "name": "R",
                         "fills": [{"type": "SOLID", "color": {"r": 0.5, "g": 0.5, "b": 0.5, "a": 1.0}}]}
                    ]},
                    {"id": "3:1", "type": "COMPONENT", "name": "Button",
                     "fills": [{"type": "SOLID", "color": {"r": d.main_red, "g": 0.0, "b": 0.0, "a": 1.0}}],
                     "children": [
                        {"id": "3:2", "type": "TEXT", "name": "Label", "characters": d.main_label}
                    ]},
                    {"id": "4:1", "type": "FRAME", "name": "C", "children": [
                        {"id": "4:2", "type": "INSTANCE", "name": "Button", "componentId": "3:1",
                         "fills": [{"type": "SOLID", "color": {"r": d.main_red, "g": 0.0, "b": 0.0, "a": 1.0}}],
                         "children": [
                            {"id": "I4:2;3:2", "type": "TEXT", "name": "Label", "characters": "Go"}
                        ]}
                    ]},
                    {"id": "5:1", "type": "FRAME", "name": "D", "children": [
                        {"id": "5:2", "type": "RECTANGLE", "name": "Photo",
                         "fills": [{"type": "IMAGE", "imageRef": d.image_ref, "scaleMode": "FILL"}]}
                    ]},
                    {"id": "6:1", "type": "FRAME", "name": "E",
                     "explicitVariableModes": {"VariableCollectionId:1:1": d.mode},
                     "children": [
                        {"id": "6:2", "type": "RECTANGLE", "name": "Swatch"}
                    ]},
                    {"id": "7:1", "type": "GROUP", "name": "Masked", "children": [
                        {"id": "7:2", "type": "RECTANGLE", "name": "Mask", "isMask": true},
                        {"id": "7:3", "type": "FRAME", "name": "Clipped", "children": [
                            {"id": "7:4", "type": "RECTANGLE", "name": "Inside"}
                        ]}
                    ]}
                ]}
            ]},
            "components": {"3:1": {"key": "kb", "name": "Button", "description": ""}},
            "componentSets": {},
            "styles": {}
        })
    }

    // The pipeline type is only nameable at an `open_store!` site, so the
    // reads these tests repeat are macros, like `scan_and_resolve!`.

    /// Apply `d` exactly as a pull does: sync, then the version-triggered
    /// eviction of `images` rows (`cli::pull::do_pull`'s own sequence).
    macro_rules! mirror {
        ($st:expr, $d:expr) => {{
            let d: &Design = $d;
            let flattened = crate::flatten::flatten_file(&design_file(d)).unwrap();
            let prior = $st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
                crate::store::collect_sweepable(&nodes, &components, &component_sets, &styles)
            });
            crate::store::sync(&mut $st, &prior, &flattened, 0);
            let stale = $st.rtx(|(_, _, _, _, _, _, _, cache, _, images, _, _, _, _)| {
                let mut ids = crate::store::stale_cache_ids(&cache, d.version);
                ids.extend(crate::store::stale_image_ids(&images, d.version));
                ids
            });
            crate::store::evict_stale_cache(&mut $st, &stale);
        }};
    }

    macro_rules! fp {
        ($st:expr, $id:expr) => {
            fp!($st, $id, "png", 1000)
        };
        ($st:expr, $id:expr, $format:expr, $scale:expr) => {
            $st.rtx(|((nodes, children, ..), ..)| {
                render_fingerprint(&nodes, &children, $id, $format, $scale)
            })
        };
    }

    macro_rules! index_rows {
        ($st:expr) => {
            $st.rtx(|(.., index, _)| {
                index
                    .iter()
                    .filter(|(key, _)| key != RENDER_CLEANUP_KEY)
                    .map(|(_, row)| row)
                    .collect::<Vec<RenderIndexRec>>()
            })
        };
    }

    macro_rules! blob_keys {
        ($st:expr) => {
            $st.rtx(|(.., blobs)| blobs.iter().map(|(k, _)| k).collect::<Vec<String>>())
        };
    }

    fn reuse() -> RenderPolicy {
        RenderPolicy {
            reuse: true,
            ..RenderPolicy::default()
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Script one `images_render` answer covering `ids`, each node's URL
    /// `https://s3/<id>-<tag>.png`.
    fn answer(script: &Script, ids: &[String], tag: &str) {
        let map: serde_json::Map<String, Value> = ids
            .iter()
            .map(|id| (id.clone(), json!(format!("https://s3/{id}-{tag}.png"))))
            .collect();
        script
            .render_responses
            .borrow_mut()
            .push(Ok(json!({ "images": map })));
    }

    /// A downloader serving 4 bytes (`tag`'s first byte plus the node id's
    /// leading digit) for every URL `answer` builds.
    fn url_bytes(url: &str) -> Result<Vec<u8>, ApiError> {
        let name = url.trim_start_matches("https://s3/");
        let (id, tag) = name
            .trim_end_matches(".png")
            .rsplit_once('-')
            .ok_or_else(|| ApiError::Network(format!("unexpected url {url}")))?;
        Ok(vec![tag.as_bytes()[0], id.as_bytes()[0], b'p', b'g'])
    }

    // -- the fingerprint itself --

    /// The rendering-contract fixtures (NWS-2958 revised: text, instances,
    /// image fills, ancestor effects): each edit changes exactly the
    /// fingerprints of the renders that show it.
    #[test]
    fn a_fingerprint_changes_only_with_its_own_subtree_and_render_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let v1 = Design::default();
        mirror!(st, &v1);
        let frames = ["1:1", "2:1", "4:1", "5:1", "6:2"];
        let base: Vec<Option<String>> = frames.iter().map(|id| fp!(st, id)).collect();
        assert!(base.iter().all(Option::is_some), "{base:?}");
        let distinct: BTreeSet<_> = base.iter().collect();
        assert_eq!(distinct.len(), frames.len(), "one fingerprint per node");

        // Deterministic, and blind to the version: re-mirroring identical
        // content under a new version keeps every fingerprint.
        mirror!(
            st,
            &Design {
                version: "v2",
                ..v1.clone()
            }
        );
        assert_eq!(
            frames.iter().map(|id| fp!(st, id)).collect::<Vec<_>>(),
            base
        );

        // Render settings are part of it.
        assert_ne!(fp!(st, "1:1", "svg", 1000), base[0]);
        assert_ne!(fp!(st, "1:1", "png", 2000), base[0]);

        // Each edit, and the fingerprints it must change.
        let edits: [(Design, &[&str]); 10] = [
            (
                Design {
                    title: "Hi",
                    ..v1.clone()
                },
                &["1:1"],
            ), // text
            (
                Design {
                    font: "Basis",
                    ..v1.clone()
                },
                &["1:1"],
            ), // font
            (
                Design {
                    b_name: "B2",
                    ..v1.clone()
                },
                &["2:1"],
            ), // own props
            // Main component edit, resolved into the instance (REST).
            (
                Design {
                    main_red: 0.9,
                    ..v1.clone()
                },
                &["4:1"],
            ),
            // Main component edit the expansion doesn't show.
            (
                Design {
                    main_label: "Buy",
                    ..v1.clone()
                },
                &["4:1"],
            ),
            (
                Design {
                    image_ref: "img2",
                    ..v1.clone()
                },
                &["5:1"],
            ), // fill
            (
                Design {
                    mode: "dark",
                    ..v1.clone()
                },
                &["6:2"],
            ), // inherited mode
            // Moving frame A (its children move with it) keeps it...
            (
                Design {
                    a_x: 1234.7,
                    ..v1.clone()
                },
                &[],
            ),
            // ...as does a prototype-only edit...
            (
                Design {
                    link: "4:1",
                    ..v1.clone()
                },
                &[],
            ),
            // ...but resizing its child doesn't.
            (
                Design {
                    title_width: 121.5,
                    ..v1.clone()
                },
                &["1:1"],
            ),
        ];
        const VERSIONS: [&str; 10] = ["e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9"];
        for (n, (design, changed)) in edits.into_iter().enumerate() {
            let version = VERSIONS[n];
            mirror!(st, &Design { version, ..design });
            for (id, before) in frames.iter().zip(&base) {
                let now = fp!(st, id);
                assert!(now.is_some(), "{id} keeps a fingerprint after edit {n}");
                if changed.contains(id) {
                    assert_ne!(&now, before, "edit {n} must change {id}");
                } else {
                    assert_eq!(&now, before, "edit {n} must leave {id} alone");
                }
            }
            mirror!(
                st,
                &Design {
                    version: "back",
                    ..v1.clone()
                }
            );
        }
    }

    /// The conservative fallback: no fingerprint (so per-version caching
    /// only) for a node a sibling mask may clip, or one not mirrored.
    #[test]
    fn a_masked_or_unmirrored_node_has_no_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        mirror!(st, &Design::default());
        assert!(fp!(st, "7:2").is_some(), "the mask itself is fine");
        assert_eq!(fp!(st, "7:3"), None, "drawn after a sibling mask");
        assert_eq!(
            fp!(st, "7:4"),
            None,
            "inside a node drawn after a sibling mask"
        );
        assert_eq!(fp!(st, "99:9"), None, "not in the mirror");
    }

    // -- reuse across versions, through scan + resolve --

    /// Done-when: editing one frame leaves cached renders of unrelated
    /// frames valid (no image request for them), while editing a frame's
    /// own content, or a main component it instances, invalidates it.
    #[test]
    fn with_reuse_an_edit_refetches_only_the_renders_that_show_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let v1 = Design::default();
        mirror!(st, &v1);
        let script = Script::new();
        let frames = ids(&["1:1", "2:1", "4:1"]);

        answer(&script, &frames, "a");
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frames, "png", None, reuse());
        assert!(items.iter().all(|i| !i.cached && i.error.is_none()));

        // Edit frame A's text.
        mirror!(
            st,
            &Design {
                version: "v2",
                title: "Edited",
                ..v1.clone()
            }
        );
        answer(&script, &ids(&["1:1"]), "b");
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frames, "png", None, reuse());
        assert_eq!(script.render_calls.borrow()[1], ids(&["1:1"]));
        assert_eq!(
            script.render_versions.borrow()[1].as_deref(),
            Some("v2"),
            "a refetch is still pinned to the mirrored version"
        );
        let cached: Vec<bool> = items.iter().map(|i| i.cached).collect();
        assert_eq!(cached, [false, true, true]);
        assert_eq!(items[0].bytes[0], b'b');
        assert_eq!(items[1].bytes[0], b'a', "B is served from the v1 render");

        // Edit the main component C instances.
        mirror!(
            st,
            &Design {
                version: "v3",
                title: "Edited",
                main_red: 0.8,
                ..v1.clone()
            }
        );
        answer(&script, &ids(&["4:1"]), "c");
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frames, "png", None, reuse());
        assert_eq!(script.render_calls.borrow()[2], ids(&["4:1"]));
        let cached: Vec<bool> = items.iter().map(|i| i.cached).collect();
        assert_eq!(cached, [true, true, false]);

        // Nothing left to fetch: even without a token every render hits.
        let items = scan_and_resolve!(st, None, &url_bytes, &frames, "png", None, reuse());
        assert!(items.iter().all(|i| i.cached && i.error.is_none()));
        assert_eq!(script.render_calls.borrow().len(), 3);

        // Orphan cleanup: the superseded A and C renders are gone, along
        // with their blobs; one row and one blob per frame remain.
        let rows = index_rows!(st);
        let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(rows.len(), 3, "{subjects:?}");
        let live: BTreeSet<String> = rows.iter().map(|r| r.sha256.clone()).collect();
        assert_eq!(blob_keys!(st).into_iter().collect::<BTreeSet<_>>(), live);
        for row in &rows {
            assert_eq!(Some(row.fingerprint.clone()), fp!(st, &row.subject));
        }
    }

    /// The default (no opt-in) behaves exactly as before NWS-2958: any
    /// version change refetches every render, and the fingerprint tables
    /// are never written.
    #[test]
    fn without_reuse_a_version_change_refetches_every_render() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let v1 = Design::default();
        mirror!(st, &v1);
        let script = Script::new();
        let frames = ids(&["1:1", "2:1", "4:1"]);

        answer(&script, &frames, "a");
        scan_and_resolve!(st, Some(&script), &url_bytes, &frames, "png", None);
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frames, "png", None);
        assert!(items.iter().all(|i| i.cached), "same version: all hits");

        mirror!(
            st,
            &Design {
                version: "v2",
                title: "Edited",
                ..v1.clone()
            }
        );
        answer(&script, &frames, "b");
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frames, "png", None);
        assert_eq!(script.render_calls.borrow()[1], frames);
        assert!(items.iter().all(|i| !i.cached));
        assert!(index_rows!(st).is_empty());
        assert!(blob_keys!(st).is_empty());
    }

    #[test]
    fn refresh_refetches_a_valid_fingerprinted_render() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        mirror!(st, &Design::default());
        let script = Script::new();
        let frame = ids(&["2:1"]);
        answer(&script, &frame, "a");
        scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None, reuse());
        let first_blob = index_rows!(st)[0].sha256.clone();

        answer(&script, &frame, "b");
        let refresh = RenderPolicy {
            refresh: true,
            ..reuse()
        };
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None, refresh);
        assert!(!items[0].cached);
        assert_eq!(items[0].bytes[0], b'b');
        assert_eq!(script.render_calls.borrow().len(), 2);
        // The row now points at the new bytes; the displaced blob is gone.
        let rows = index_rows!(st);
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].sha256, first_blob);
        assert_eq!(blob_keys!(st), vec![rows[0].sha256.clone()]);
    }

    /// Blobs are validated by digest on read: bytes that no longer hash
    /// to their key are never served.
    #[test]
    fn a_corrupted_render_blob_is_a_miss_and_is_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        mirror!(st, &Design::default());
        let script = Script::new();
        let frame = ids(&["2:1"]);
        answer(&script, &frame, "a");
        let first = scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None, reuse());
        let sha = index_rows!(st)[0].sha256.clone();
        st.wtx(|tx| {
            tx.upsert(
                &Id::RenderBlob(sha.clone()),
                &Rec::RenderBlob(RenderBlobRec {
                    sha256: sha.clone(),
                    bytes: vec![0xde, 0xad, 0xbe, 0xef],
                }),
            );
        });

        answer(&script, &frame, "a");
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None, reuse());
        assert!(!items[0].cached, "a tampered blob must read as a miss");
        assert_eq!(items[0].bytes, first[0].bytes);
        assert_eq!(script.render_calls.borrow().len(), 2);
        // Refetching the same bytes repaired the blob under its key.
        let items = scan_and_resolve!(st, None, &url_bytes, &frame, "png", None, reuse());
        assert!(items[0].cached);
        assert_eq!(items[0].bytes, first[0].bytes);
    }

    /// The hard byte quota evicts still-valid renders, oldest first.
    #[test]
    fn the_byte_quota_evicts_the_oldest_renders_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        mirror!(st, &Design::default());
        let script = Script::new();
        // Every render is 4 bytes: room for two.
        let policy = RenderPolicy {
            max_bytes: 8,
            ..reuse()
        };
        for id in ["1:1", "2:1", "4:1"] {
            answer(&script, &ids(&[id]), "a");
            scan_and_resolve!(
                st,
                Some(&script),
                &url_bytes,
                &ids(&[id]),
                "png",
                None,
                policy
            );
        }
        let mut subjects: Vec<String> = index_rows!(st).into_iter().map(|r| r.subject).collect();
        subjects.sort();
        assert_eq!(subjects, ids(&["2:1", "4:1"]), "1:1 was oldest");
        assert_eq!(blob_keys!(st).len(), 2);

        // The evicted render is fetched again while 2:1 — now the oldest
        // — is served by the same request: the quota never evicts a
        // render the request is answering with, so 4:1 goes instead.
        answer(&script, &ids(&["1:1"]), "a");
        let request = ids(&["2:1", "1:1"]);
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &request, "png", None, policy);
        assert_eq!(script.render_calls.borrow()[3], ids(&["1:1"]));
        let cached: Vec<bool> = items.iter().map(|i| i.cached).collect();
        assert_eq!(cached, [true, false]);
        let mut subjects: Vec<String> = index_rows!(st).into_iter().map(|r| r.subject).collect();
        subjects.sort();
        assert_eq!(
            subjects,
            ids(&["1:1", "2:1"]),
            "4:1 goes, not the served 2:1"
        );
        assert_eq!(blob_keys!(st).len(), 2);

        // One request's own renders may exceed the cap until a later
        // request evicts: three 4-byte renders served together all stay.
        answer(&script, &ids(&["4:1"]), "a");
        let all = ids(&["1:1", "2:1", "4:1"]);
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &all, "png", None, policy);
        assert!(items.iter().all(|i| i.error.is_none()));
        assert_eq!(index_rows!(st).len(), 3);
    }

    // -- eviction planning --

    fn row(fp: &str, seq: u64, sha: &str, len: u64) -> (String, RenderIndexRec) {
        (
            fp.to_string(),
            RenderIndexRec {
                fingerprint: fp.to_string(),
                scheme: RENDER_FINGERPRINT_SCHEME,
                subject: "1:1".to_string(),
                format: "png".to_string(),
                scale_milli: 1000,
                file_version: "v1".to_string(),
                seq,
                sha256: sha.to_string(),
                byte_len: len,
            },
        )
    }

    #[test]
    fn eviction_drops_orphans_oldest_first_then_enforces_the_quota_oldest_first() {
        let index: BTreeMap<String, RenderIndexRec> = [
            row("f-new-orphan", 5, "s1", 10),
            row("f-old-orphan", 1, "s2", 10),
            row("f-valid-old", 2, "s3", 10),
            row("f-valid-mid", 3, "s4", 10),
            row("f-valid-new", 4, "s5", 10),
        ]
        .into_iter()
        .collect();
        let orphans: BTreeSet<String> = ["f-new-orphan", "f-old-orphan"].map(String::from).into();

        let none = BTreeSet::new();
        // Plenty of room: only orphans go.
        let plan = plan_render_eviction(&index, &orphans, &none, &none, 1_000);
        assert_eq!(plan.index, ["f-old-orphan", "f-new-orphan"]);
        assert_eq!(plan.blobs, ["s1", "s2"]);

        // Room for two: then the oldest valid render goes too.
        let plan = plan_render_eviction(&index, &orphans, &none, &none, 20);
        assert_eq!(plan.index, ["f-old-orphan", "f-new-orphan", "f-valid-old"]);
        assert_eq!(plan.blobs, ["s1", "s2", "s3"]);

        // A zero quota empties the cache, deterministically ordered.
        let plan = plan_render_eviction(&index, &BTreeSet::new(), &none, &none, 0);
        assert_eq!(
            plan.index,
            [
                "f-old-orphan",
                "f-valid-old",
                "f-valid-mid",
                "f-valid-new",
                "f-new-orphan"
            ]
        );
    }

    #[test]
    fn eviction_ties_break_by_fingerprint_and_shared_blobs_count_once() {
        let index: BTreeMap<String, RenderIndexRec> = [
            row("b", 1, "shared", 10),
            row("a", 1, "shared", 10),
            row("c", 2, "own", 10),
        ]
        .into_iter()
        .collect();
        let none = BTreeSet::new();
        // 20 distinct bytes fit exactly: nothing to do.
        let plan = plan_render_eviction(&index, &none, &none, &none, 20);
        assert_eq!(plan, RenderEviction::default());

        // Over by one: "a" (tie at seq 1, smaller fingerprint) goes first,
        // but its blob stays while "b" still references it, so "b" goes
        // too before the total drops.
        let plan = plan_render_eviction(&index, &none, &none, &none, 19);
        assert_eq!(plan.index, ["a", "b"]);
        assert_eq!(plan.blobs, ["shared"]);

        // A displaced blob is dropped only once nothing references it.
        let displaced: BTreeSet<String> = ["gone", "own"].map(String::from).into();
        let plan = plan_render_eviction(&index, &none, &none, &displaced, 100);
        assert!(plan.index.is_empty());
        assert_eq!(plan.blobs, ["gone"]);
    }

    #[test]
    fn eviction_never_evicts_a_protected_render_for_the_quota() {
        let index: BTreeMap<String, RenderIndexRec> = [
            row("old", 1, "s1", 10),
            row("mid", 2, "s2", 10),
            row("new", 3, "s3", 10),
        ]
        .into_iter()
        .collect();
        let protected: BTreeSet<String> = ["old".to_string()].into();
        let none = BTreeSet::new();
        let plan = plan_render_eviction(&index, &none, &protected, &none, 20);
        assert_eq!(plan.index, ["mid"], "the oldest unprotected row goes");
        assert_eq!(plan.blobs, ["s2"]);
        // Orphans go even when nothing else may.
        let orphans: BTreeSet<String> = ["new".to_string()].into();
        let all: BTreeSet<String> = ["old", "mid"].map(String::from).into();
        let plan = plan_render_eviction(&index, &orphans, &all, &none, 0);
        assert_eq!(plan.index, ["new"]);
    }

    // -- switching policies can't undo a refresh --

    /// Reuse fetches a render, then a plain `--refresh` replaces it: a
    /// later `--reuse-renders` request must serve the refreshed bytes, not
    /// the fingerprinted pre-refresh ones.
    #[test]
    fn a_plain_refresh_retires_the_fingerprinted_render() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        mirror!(st, &Design::default());
        let script = Script::new();
        let frame = ids(&["2:1"]);
        answer(&script, &frame, "a");
        scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None, reuse());
        assert_eq!(index_rows!(st).len(), 1);

        answer(&script, &frame, "b");
        let refresh = RenderPolicy {
            refresh: true,
            ..RenderPolicy::default()
        };
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None, refresh);
        assert_eq!(items[0].bytes[0], b'b');
        assert!(index_rows!(st).is_empty(), "the old render is retired");
        assert!(blob_keys!(st).is_empty());

        let items = scan_and_resolve!(st, None, &url_bytes, &frame, "png", None, reuse());
        assert!(items[0].cached);
        assert_eq!(items[0].bytes[0], b'b', "never the pre-refresh bytes");
    }

    /// The reverse switch: a per-version render, then `--reuse-renders
    /// --refresh`; a later plain request must not serve the older
    /// per-version bytes.
    #[test]
    fn a_reuse_refresh_drops_the_per_version_render() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        mirror!(st, &Design::default());
        let script = Script::new();
        let frame = ids(&["2:1"]);
        answer(&script, &frame, "a");
        scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None);

        answer(&script, &frame, "b");
        let refresh = RenderPolicy {
            refresh: true,
            ..reuse()
        };
        let items = scan_and_resolve!(st, Some(&script), &url_bytes, &frame, "png", None, refresh);
        assert_eq!(items[0].bytes[0], b'b');

        // Plain request: the per-version row is gone, so this is a miss
        // (never the pre-refresh "a")...
        let items = scan_and_resolve!(st, None, &url_bytes, &frame, "png", None);
        assert!(items[0].error.is_some(), "no stale per-version hit");
        // ...while the refreshed render still serves reuse requests.
        let items = scan_and_resolve!(st, None, &url_bytes, &frame, "png", None, reuse());
        assert!(items[0].cached);
        assert_eq!(items[0].bytes[0], b'b');
    }

    // -- the orphan pass's cost --

    fn hashed_nodes() -> usize {
        NODE_HASHES.with(|n| n.replace(0))
    }

    /// A request whose renders all hit, at the version the last cleanup
    /// saw, hashes only its own subtrees — not every cached render's.
    #[test]
    fn a_pure_hit_request_hashes_only_its_own_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let v1 = Design::default();
        mirror!(st, &v1);
        let script = Script::new();
        let frames = ids(&["1:1", "2:1", "5:1"]);
        answer(&script, &frames, "a");
        scan_and_resolve!(st, Some(&script), &url_bytes, &frames, "png", None, reuse());

        // 1:1 is two nodes (the frame and its text).
        let a = ids(&["1:1"]);
        hashed_nodes();
        let items = scan_and_resolve!(st, None, &url_bytes, &a, "png", None, reuse());
        assert!(items[0].cached);
        assert_eq!(hashed_nodes(), 2, "no orphan pass on a pure hit");

        // A new mirror version: one orphan pass, over every cached render
        // (each node hashed once: 1:1's two, 2:1's two, 5:1's two)...
        mirror!(
            st,
            &Design {
                version: "v2",
                b_name: "B2",
                ..v1.clone()
            }
        );
        let items = scan_and_resolve!(st, None, &url_bytes, &a, "png", None, reuse());
        assert!(items[0].cached);
        assert_eq!(hashed_nodes(), 6);
        let subjects: BTreeSet<String> = index_rows!(st).into_iter().map(|r| r.subject).collect();
        assert_eq!(
            subjects,
            ["1:1", "5:1"].map(String::from).into(),
            "B's render is an orphan"
        );

        // ...and none again until the mirror changes.
        scan_and_resolve!(st, None, &url_bytes, &a, "png", None, reuse());
        assert_eq!(hashed_nodes(), 2);
    }
}
