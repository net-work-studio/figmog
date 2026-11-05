//! Record vocabulary shared by flatten, store, and the CLI.
//!
//! Determinism contract: every map-shaped field is a **sorted** `Vec` of
//! pairs, and canonical-JSON strings come from `serde_json` without
//! `preserve_order`. `KeyedStream` diffs records by postcard bytes, so two
//! flattens of the same file JSON must be byte-identical.

use serde::{Deserialize, Serialize};

/// Primary key of every mirrored record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Id {
    Node(String),
    Component(String),
    ComponentSet(String),
    Style(String),
    Variable(String),
    VariableCollection(String),
    Meta,
    /// Cached upstream proxy response, keyed by its hash (spec §12). APPEND
    /// ONLY: postcard encodes variant indices, so inserting a variant
    /// earlier in this enum would corrupt every existing store.
    ProxyCache(String),
    /// Sticky per-mirror settings row (v0.0.2 spec §4), one per store, like
    /// [`Id::Meta`]. APPEND ONLY — see [`Id::ProxyCache`]'s note.
    MirrorConfig,
    /// Cached image bytes (render or fill, v0.0.2 spec §5), keyed by
    /// [`crate::images::image_key`]'s hash — same pattern as
    /// [`Id::ProxyCache`]. APPEND ONLY — see that variant's note.
    ImageBlob(String),
    /// Render cache-generation marker for the [`Id::ImageBlob`] row under
    /// the same key (NWS-2960): present only for renders fetched with the
    /// request pinned to the mirrored file version. APPEND ONLY — see
    /// [`Id::ProxyCache`]'s note.
    ImagePin(String),
    /// The single variable-snapshot provenance row (NWS-2961), one per
    /// store, like [`Id::Meta`]. APPEND ONLY — see [`Id::ProxyCache`]'s
    /// note.
    VariableSnapshot,
    /// Fingerprint-indexed render cache entry (NWS-2958), keyed by the
    /// render fingerprint ([`crate::images::render_fingerprint`]). APPEND
    /// ONLY — see [`Id::ProxyCache`]'s note.
    RenderIndex(String),
    /// Immutable, content-addressed render bytes (NWS-2958), keyed by the
    /// SHA-256 hex of the bytes themselves. APPEND ONLY — see
    /// [`Id::ProxyCache`]'s note; this must stay the last variant until
    /// the next one is appended after it.
    RenderBlob(String),
}

/// One mirrored record; variant always matches its [`Id`] variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Rec {
    Node(NodeRec),
    Component(ComponentRec),
    ComponentSet(ComponentSetRec),
    Style(StyleRec),
    Variable(VariableRec),
    VariableCollection(VariableCollectionRec),
    Meta(FileMeta),
    /// See [`Id::ProxyCache`]. APPEND ONLY — see that variant's note.
    ProxyCache(ProxyCacheRec),
    /// See [`Id::MirrorConfig`]. APPEND ONLY — see that variant's note.
    MirrorConfig(MirrorConfigRec),
    /// See [`Id::ImageBlob`]. APPEND ONLY — see that variant's note.
    ImageBlob(ImageBlobRec),
    /// See [`Id::ImagePin`]. APPEND ONLY — see [`Id::ProxyCache`]'s note.
    ImagePin(ImagePinRec),
    /// See [`Id::VariableSnapshot`]. APPEND ONLY — see [`Id::ProxyCache`]'s note.
    VariableSnapshot(VariableSnapshotRec),
    /// See [`Id::RenderIndex`]. APPEND ONLY — see [`Id::ProxyCache`]'s note.
    RenderIndex(RenderIndexRec),
    /// See [`Id::RenderBlob`]. APPEND ONLY — see [`Id::ProxyCache`]'s note.
    RenderBlob(RenderBlobRec),
}

/// One node of the document tree (children stripped from `raw`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRec {
    pub id: String,
    pub parent_id: Option<String>,
    pub child_index: u32,
    /// Enclosing CANVAS id; the document root and CANVAS nodes carry their own id.
    pub page_id: String,
    pub node_type: String,
    pub name: String,
    pub visible: bool,
    /// `characters` for TEXT nodes.
    pub text: Option<String>,
    /// INSTANCE → the component's node id.
    pub component_id: Option<String>,
    /// INSTANCE `componentProperties` as (name, canonical-JSON value), sorted.
    pub component_properties: Vec<(String, String)>,
    /// `componentPropertyDefinitions` (COMPONENT / COMPONENT_SET) as canonical JSON.
    pub property_definitions: Option<String>,
    /// Node `styles` map as (style_type, style_id), sorted.
    pub style_refs: Vec<(String, String)>,
    /// Variable bindings as (json-pointer to the bound property, variable id), sorted.
    /// The pointer addresses the *resolved value* location, e.g. `/fills/0/color`.
    pub bound_variables: Vec<(String, String)>,
    /// absoluteBoundingBox x, y, w, h.
    pub abs_bounds: Option<[f64; 4]>,
    /// Canonical JSON of the node with `children` removed.
    pub raw: String,
}

/// Entry of the file response's `components` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentRec {
    pub node_id: String,
    pub key: String,
    pub name: String,
    pub description: String,
    pub component_set_id: Option<String>,
    pub remote: bool,
}

/// Entry of the file response's `componentSets` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentSetRec {
    pub node_id: String,
    pub key: String,
    pub name: String,
    pub description: String,
    pub remote: bool,
}

/// Entry of the file response's `styles` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleRec {
    pub style_id: String,
    pub key: String,
    pub name: String,
    pub style_type: String,
    pub description: String,
    pub remote: bool,
}

/// Authoritative variable definition (from `import-variables` or an
/// Enterprise `variables/local` pull).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableRec {
    pub id: String,
    pub name: String,
    pub resolved_type: String,
    pub collection_id: String,
    /// mode id -> value-or-alias, as sorted (mode_id, canonical JSON) pairs.
    pub values_by_mode: Vec<(String, String)>,
    pub description: String,
    pub scopes: Vec<String>,
}

/// Authoritative variable collection (from `import-variables` or an
/// Enterprise `variables/local` pull).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableCollectionRec {
    pub id: String,
    pub name: String,
    /// (mode_id, mode_name), sorted by mode_id.
    pub modes: Vec<(String, String)>,
    pub default_mode_id: String,
}

/// The single file-level row (key [`Id::Meta`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub version: String,
    pub last_modified: String,
    pub synced_at_unix_ms: u64,
}

/// One cached upstream proxy response (spec §12). A hit requires
/// `file_version` to equal the mirror's current [`FileMeta::version`]; a
/// version bump makes the row stale and eligible for eviction
/// (`store::stale_cache_ids` / `store::evict_stale_cache`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyCacheRec {
    /// Hash of `tool` + `args_canonical`; identical to the [`Id::ProxyCache`] key.
    pub key_hash: String,
    pub tool: String,
    /// Canonical JSON (`serde_json::to_string`) of the call arguments.
    pub args_canonical: String,
    /// File version this response was fetched at.
    pub file_version: String,
    /// Canonical JSON of the upstream MCP result content.
    pub content: String,
}

/// The single sticky-config row (key [`Id::MirrorConfig`], v0.0.2 spec
/// §4): whether pulls of this mirror should request vector geometry
/// (`fillGeometry`/`strokeGeometry`) from Figma. An absent row (every
/// pre-v0.0.2 store, and any store that's never been pulled with
/// `--geometry`) means `false`.
///
/// This lives in its own record kind rather than as a new `FileMeta`
/// field on purpose: postcard structs encode fields positionally, so
/// widening `FileMeta` would silently corrupt decoding of every existing
/// store, while an enum-variant *append* (this type, `Id::MirrorConfig`,
/// `Rec::MirrorConfig`) is safe — see `CLAUDE.md`'s struct-vs-enum-append
/// rule and `Id::ProxyCache`'s note.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorConfigRec {
    pub geometry: bool,
}

/// One cached image blob — a node render or a fill's referenced bytes
/// (v0.0.2 spec §5). A hit requires `file_version` to equal the mirror's
/// current [`FileMeta::version`], exactly like [`ProxyCacheRec`]; a
/// version bump makes the row stale and eligible for eviction
/// (`store::stale_image_ids` / `store::evict_stale_cache`).
///
/// Lives in its own append-only enum variant rather than widening an
/// existing struct, for the same reason as [`MirrorConfigRec`] — see that
/// type's doc comment and `CLAUDE.md`'s struct-vs-enum-append rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlobRec {
    /// Hash of `kind` + `subject` + `format` + `scale_milli`; identical to
    /// the [`Id::ImageBlob`] key ([`crate::images::image_key`]).
    pub key_hash: String,
    /// `"render"` (a `GET /v1/images/:key` node render) or `"fill"` (a
    /// `GET /v1/files/:key/images` fill image, addressed by its
    /// `imageRef` hash).
    pub kind: String,
    /// Node id (`kind == "render"`) or `imageRef` hash (`kind == "fill"`).
    pub subject: String,
    /// Requested render format (`"png"`/`"svg"`/…), or the format
    /// inferred from a fill's download URL — informational only for
    /// fills, since [`crate::images::image_key`] never varies a fill's
    /// cache key by format (Figma's fills endpoint takes no format
    /// parameter; the same `imageRef` always resolves to the same bytes).
    pub format: String,
    /// Requested scale × 1000 (e.g. `scale: 1.5` ⇒ `1500`); always `1000`
    /// for fills, which aren't scale-parameterized.
    pub scale_milli: u32,
    /// File version this blob was fetched at.
    pub file_version: String,
    /// The raw image bytes.
    pub bytes: Vec<u8>,
}

/// Cache-generation marker for one render row in the `images` table
/// (key [`Id::ImagePin`], identical to the [`Id::ImageBlob`] key it
/// marks). Renders fetched before NWS-2960 were requested without a
/// `version` parameter, so their bytes may show a newer design than the
/// `file_version` they were stored under; only a render row with a
/// matching marker (`generation == crate::images::RENDER_PIN_GENERATION`
/// and the same `file_version`) is a cache hit. Fill rows need no marker:
/// they are addressed by content hash, so their bytes can't differ by
/// version.
///
/// A separate record kind rather than a new [`ImageBlobRec`] field, for
/// the same reason as [`MirrorConfigRec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImagePinRec {
    /// Render-cache generation the marked row was fetched under.
    pub generation: u32,
    /// File version the render request was pinned to — always equal to
    /// the marked row's own [`ImageBlobRec::file_version`] when written.
    pub file_version: String,
}

/// Provenance of the variable snapshot currently in the `variables` /
/// `variable_collections` tables (key [`Id::VariableSnapshot`], NWS-2961),
/// plus the per-variable publishing metadata the token pipeline needs.
/// Written in the same transaction as the snapshot it describes — by
/// `import-variables` (which replaces the previous snapshot) and by a
/// network pull that fetched an Enterprise `variables/local` export. An
/// absent row with variables stored means a pre-NWS-2961 ("legacy")
/// snapshot of unknown provenance.
///
/// A separate record kind rather than new [`VariableRec`] fields, for the
/// same reason as [`MirrorConfigRec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableSnapshotRec {
    /// Where the snapshot came from: `"enterprise_rest"` (a pull's
    /// `variables/local` fetch) or `"file"` (`import-variables`).
    pub source: String,
    /// Detected export shape: `"rest"` (`{meta: {...}}`, the REST
    /// response) or `"bare"` (the plugin-console object).
    pub format: String,
    /// The export's top-level `formatVersion`, when it carries one.
    pub format_version: Option<String>,
    /// The export's top-level `exportedAt`, when it carries one.
    pub exported_at: Option<String>,
    /// `fnv1a64:<16 hex>` over the raw file bytes (`import-variables`) or
    /// the response's canonical JSON (pulls). Non-cryptographic: a change
    /// detector, not an integrity proof.
    pub digest: String,
    /// One entry per variable in the snapshot, sorted by id.
    pub variables: Vec<VariableCodeRec>,
}

/// Publishing metadata for one variable of a [`VariableSnapshotRec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableCodeRec {
    pub id: String,
    /// `codeSyntax` as sorted (platform, name) pairs, e.g. `("WEB", "--x")`.
    pub code_syntax: Vec<(String, String)>,
    pub hidden_from_publishing: bool,
    pub remote: bool,
}

/// One fingerprint-indexed render (key [`Id::RenderIndex`], NWS-2958):
/// the render of `subject` at `format`/`scale_milli` whose subtree
/// fingerprint is the key. Unlike an [`ImageBlobRec`] render row it is not
/// tied to a file version — it stays a hit for as long as the node's
/// recomputed fingerprint still equals the key, across any number of
/// file versions. Only written and read when a caller opts in to render
/// reuse (`images::RenderPolicy::reuse`); never evicted by the
/// version-change pass (`store::stale_image_ids`), only by
/// `images`'s orphan cleanup and byte quota.
///
/// The bytes live in a separate, content-addressed [`RenderBlobRec`] row
/// so the orphan and quota scans read only these small metadata rows.
/// A new record kind rather than new [`ImageBlobRec`] fields, for the
/// same reason as [`MirrorConfigRec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderIndexRec {
    /// The render fingerprint; identical to the [`Id::RenderIndex`] key.
    pub fingerprint: String,
    /// Fingerprint scheme the key was computed under
    /// (`images::RENDER_FINGERPRINT_SCHEME`); a row from another scheme is
    /// an orphan.
    pub scheme: u32,
    /// The rendered node id.
    pub subject: String,
    /// Requested render format.
    pub format: String,
    /// Requested scale × 1000.
    pub scale_milli: u32,
    /// File version the render request was pinned to (NWS-2960) —
    /// provenance only, never compared on a hit.
    pub file_version: String,
    /// Store-local insertion sequence: eviction removes the lowest first,
    /// ties broken by fingerprint.
    pub seq: u64,
    /// SHA-256 hex of the render bytes; the [`Id::RenderBlob`] key.
    pub sha256: String,
    /// Byte length of the render bytes (what the byte quota counts).
    pub byte_len: u64,
}

/// Immutable render bytes (key [`Id::RenderBlob`], NWS-2958), addressed by
/// their own SHA-256 hex. Written only with bytes whose digest equals the
/// key, never modified afterwards, and re-validated against the key on
/// every read: a row whose bytes no longer hash to its key reads as a miss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderBlobRec {
    /// SHA-256 hex of `bytes`; identical to the [`Id::RenderBlob`] key.
    pub sha256: String,
    /// The raw image bytes.
    pub bytes: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node() -> NodeRec {
        NodeRec {
            id: "1:2".into(),
            parent_id: Some("0:1".into()),
            child_index: 0,
            page_id: "0:1".into(),
            node_type: "TEXT".into(),
            name: "Title".into(),
            visible: true,
            text: Some("hello".into()),
            component_id: None,
            component_properties: vec![("Size".into(), "\"Large\"".into())],
            property_definitions: None,
            style_refs: vec![("text".into(), "S:2".into())],
            bound_variables: vec![("/style/fontSize".into(), "VariableID:9".into())],
            abs_bounds: Some([0.0, 0.0, 100.0, 20.0]),
            raw: "{}".into(),
        }
    }

    #[test]
    fn rec_postcard_roundtrip() {
        let rec = Rec::Node(sample_node());
        let bytes = postcard::to_allocvec(&rec).unwrap();
        let back: Rec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn mirror_config_rec_postcard_roundtrip() {
        let rec = Rec::MirrorConfig(MirrorConfigRec { geometry: true });
        let bytes = postcard::to_allocvec(&rec).unwrap();
        let back: Rec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(rec, back);
    }

    fn sample_image_blob() -> ImageBlobRec {
        ImageBlobRec {
            key_hash: "abc123".into(),
            kind: "render".into(),
            subject: "1:2".into(),
            format: "png".into(),
            scale_milli: 2000,
            file_version: "100".into(),
            bytes: vec![0x89, b'P', b'N', b'G'],
        }
    }

    #[test]
    fn image_blob_rec_postcard_roundtrip() {
        let rec = Rec::ImageBlob(sample_image_blob());
        let bytes = postcard::to_allocvec(&rec).unwrap();
        let back: Rec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(rec, back);
    }

    /// Postcard-append safety (v0.0.2 spec §4 + §5): a store written before
    /// `MirrorConfig` existed encoded `Rec::ProxyCache` at variant index 7
    /// (the 8th of 8 variants, 0-indexed) — postcard's enum encoding is a
    /// leading varint variant index, one byte for fewer than 128 variants.
    /// Appending `MirrorConfig` *after* `ProxyCache` must not move that
    /// index (it would corrupt every existing cache row's discriminant on
    /// decode), and each new variant must land at the next free index (8
    /// for `MirrorConfig`, 9 for the later-appended `ImageBlob`, 10 for
    /// `ImagePin`, 11 for `VariableSnapshot`, 12 and 13 for `RenderIndex` and
    /// `RenderBlob`). A
    /// pre-change build isn't available to literally round-trip against,
    /// so this pins the concrete bytes instead — the same guarantee the
    /// append-only doc comments on `Id`/`Rec` assert, made checkable.
    #[test]
    fn append_only_preserves_existing_variant_indices() {
        let proxy = Rec::ProxyCache(ProxyCacheRec {
            key_hash: "h".into(),
            tool: "t".into(),
            args_canonical: "{}".into(),
            file_version: "1".into(),
            content: "{}".into(),
        });
        let bytes = postcard::to_allocvec(&proxy).unwrap();
        assert_eq!(bytes[0], 7, "ProxyCache's variant index must not move");

        let cfg = Rec::MirrorConfig(MirrorConfigRec { geometry: true });
        let bytes = postcard::to_allocvec(&cfg).unwrap();
        assert_eq!(bytes[0], 8, "MirrorConfig is the newly appended variant");

        let img = Rec::ImageBlob(sample_image_blob());
        let bytes = postcard::to_allocvec(&img).unwrap();
        assert_eq!(bytes[0], 9, "ImageBlob's variant index must not move");

        let pin = Rec::ImagePin(ImagePinRec {
            generation: 1,
            file_version: "100".into(),
        });
        let bytes = postcard::to_allocvec(&pin).unwrap();
        assert_eq!(bytes[0], 10, "ImagePin is the newly appended variant");
        let back: Rec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(pin, back);
        assert_eq!(
            postcard::to_allocvec(&Id::ImagePin("k".into())).unwrap()[0],
            10,
            "Id::ImagePin's variant index must not move"
        );

        let snap = Rec::VariableSnapshot(VariableSnapshotRec {
            source: "file".into(),
            format: "bare".into(),
            format_version: None,
            exported_at: Some("2026-01-01T00:00:00Z".into()),
            digest: "fnv1a64:0000000000000000".into(),
            variables: vec![VariableCodeRec {
                id: "VariableID:1".into(),
                code_syntax: vec![("WEB".into(), "--x".into())],
                hidden_from_publishing: true,
                remote: false,
            }],
        });
        let bytes = postcard::to_allocvec(&snap).unwrap();
        assert_eq!(
            bytes[0], 11,
            "VariableSnapshot is the newly appended variant"
        );
        let back: Rec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(snap, back);
        assert_eq!(
            postcard::to_allocvec(&Id::VariableSnapshot).unwrap()[0],
            11,
            "Id::VariableSnapshot's variant index must not move"
        );

        // NWS-2958: the render cache's two record kinds land at 12 and 13.
        let index = Rec::RenderIndex(RenderIndexRec {
            fingerprint: "f".into(),
            scheme: 1,
            subject: "1:2".into(),
            format: "png".into(),
            scale_milli: 1000,
            file_version: "100".into(),
            seq: 1,
            sha256: "s".into(),
            byte_len: 3,
        });
        let bytes = postcard::to_allocvec(&index).unwrap();
        assert_eq!(bytes[0], 12, "RenderIndex is a newly appended variant");
        assert_eq!(postcard::from_bytes::<Rec>(&bytes).unwrap(), index);
        let blob = Rec::RenderBlob(RenderBlobRec {
            sha256: "s".into(),
            bytes: vec![1, 2, 3],
        });
        let bytes = postcard::to_allocvec(&blob).unwrap();
        assert_eq!(bytes[0], 13, "RenderBlob is the newly appended variant");
        assert_eq!(postcard::from_bytes::<Rec>(&bytes).unwrap(), blob);
        assert_eq!(
            postcard::to_allocvec(&Id::RenderIndex("f".into())).unwrap()[0],
            12
        );
        assert_eq!(
            postcard::to_allocvec(&Id::RenderBlob("s".into())).unwrap()[0],
            13
        );
    }

    #[test]
    fn identical_records_encode_identically() {
        let a = postcard::to_allocvec(&Rec::Node(sample_node())).unwrap();
        let b = postcard::to_allocvec(&Rec::Node(sample_node())).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn ids_order_and_roundtrip() {
        let ids = vec![Id::Meta, Id::Node("1:1".into()), Id::Style("S:1".into())];
        let set: std::collections::BTreeSet<Id> = ids.iter().cloned().collect();
        assert_eq!(set.len(), 3);
        let bytes = postcard::to_allocvec(&ids).unwrap();
        let back: Vec<Id> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(ids, back);
    }
}
