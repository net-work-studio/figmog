//! The 18 read-only `figmog_*` tools: one dispatch function, generic over
//! the store's reader types, shared by `figmog serve`'s request loop and
//! the CLI's `figmog call`/`figmog tools`. (`figmog_sync` is not here — it
//! writes to the store and drives watch/backoff state that only exists at
//! each call site's `open_store!` — see the identical note in `serve.rs`
//! and `cli::dispatch` about the pipeline's unnameable type.)
//!
//! Both call sites destructure the same `st.rtx(|tuple| ...)` reader tuple
//! ([`RootReaders`]) that `open_store!`'s pipeline produces, so
//! [`ReadCommand::execute`] can take it directly and stay the single place
//! that maps a local read to a `query::*` call — for tool JSON
//! ([`dispatch_read_tool`]) and the CLI's direct read commands alike.

use fold::pipeline::terminal::search::Bm25Reader;
use fold::pipeline::terminal::{InvertedIndexReader, MultimapReader, TableReader};
use fold::stream::Readable;
use serde_json::{Value, json};

use crate::mcp::ToolDef;
use crate::model::{
    ComponentRec, ComponentSetRec, FileMeta, ImageBlobRec, ImagePinRec, MirrorConfigRec, NodeRec,
    ProxyCacheRec, StyleRec, VariableCollectionRec, VariableRec, VariableSnapshotRec,
};
use crate::query;

/// Read handles for the pipeline's `nodes` branch (see `figmog_pipeline!`
/// in `store.rs`): table, children edges, BM25 text index, then the three
/// inverted indexes plus `by_type`, in the branch's own nesting order.
pub(crate) type NodeReaders<'a, R> = (
    TableReader<'a, R, String, NodeRec>,
    MultimapReader<'a, R, String, (u32, String)>,
    Bm25Reader<'a, R, String, fn(&str, &mut Vec<u8>)>,
    InvertedIndexReader<'a, R, String, String>,
    InvertedIndexReader<'a, R, String, String>,
    InvertedIndexReader<'a, R, String, String>,
    InvertedIndexReader<'a, R, String, String>,
);

/// Read handles for the whole pipeline, in `figmog_pipeline!`'s top-level
/// order — the exact tuple `st.rtx`'s closure receives. 12 elements: the
/// `nodes` branch bundle, then `components`, `component_sets`, `styles`,
/// `variables`, `variable_collections`, `meta`, `proxy_cache`,
/// `mirror_config` (v0.0.2 spec §4), `images` (v0.0.2 spec §5),
/// `image_pins` (NWS-2960), `variable_snapshot` (NWS-2961 — appended
/// last, non-breaking).
pub(crate) type RootReaders<'a, R> = (
    NodeReaders<'a, R>,
    TableReader<'a, R, String, ComponentRec>,
    TableReader<'a, R, String, ComponentSetRec>,
    TableReader<'a, R, String, StyleRec>,
    TableReader<'a, R, String, VariableRec>,
    TableReader<'a, R, String, VariableCollectionRec>,
    TableReader<'a, R, u8, FileMeta>,
    TableReader<'a, R, String, ProxyCacheRec>,
    TableReader<'a, R, u8, MirrorConfigRec>,
    TableReader<'a, R, String, ImageBlobRec>,
    TableReader<'a, R, String, ImagePinRec>,
    TableReader<'a, R, u8, VariableSnapshotRec>,
);

// ---- arg extraction ----

/// The value of an optional argument, treating an omitted key and an
/// explicit JSON `null` alike as "absent" (NWS-2944) — `figmog images`
/// itself sends `"scale": null` for an unset `--scale`, and a client that
/// spells "no value" as `null` means the default, not an error.
fn optional_arg<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
}

/// Optional string field. Absent/`null` ⇒ `Ok(None)`; present but some
/// other JSON type ⇒ the same `"expected string for `key`, got …"` error
/// [`require_str`] reports, rather than silently treating it as absent
/// (NWS-2944).
pub(crate) fn arg_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match optional_arg(args, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!(
            "expected string for `{key}`, got {}",
            json_type_name(other)
        )),
    }
}

/// JSON type name for an error message — `Value`'s own variant names
/// ("Number", "String", ...) aren't what a caller expects to read, so this
/// spells out the JSON vocabulary instead (`"number"`, `"string"`, ...).
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A required field that's present but the wrong JSON type is a different,
/// more actionable error than an absent one — `"expected string for `id`,
/// got number"` tells the caller what to fix; `"missing required field"`
/// would send them looking for a key that's actually right there.
pub(crate) fn require_str(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        None => Err(format!("missing required field: {key}")),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!(
            "expected string for `{key}`, got {}",
            json_type_name(other)
        )),
    }
}

/// Optional non-negative integer field (`depth`, `limit`). Absent/`null`
/// ⇒ `Ok(None)`. A non-number ⇒ `"expected integer for `key`, got
/// string"`; a fractional number ⇒ `"… got number"`; a negative integer ⇒
/// `"expected non-negative integer for `key`, got -1"` (NWS-2944 — a
/// string `depth` used to read as "unlimited").
pub(crate) fn arg_usize(args: &Value, key: &str) -> Result<Option<usize>, String> {
    let Some(v) = optional_arg(args, key) else {
        return Ok(None);
    };
    if let Some(n) = v.as_u64() {
        return Ok(Some(usize::try_from(n).unwrap_or(usize::MAX)));
    }
    match v.as_i64() {
        Some(n) => Err(format!(
            "expected non-negative integer for `{key}`, got {n}"
        )),
        None => Err(format!(
            "expected integer for `{key}`, got {}",
            json_type_name(v)
        )),
    }
}

/// Optional boolean flag. Absent/`null` ⇒ `Ok(false)`; present but not a
/// JSON boolean (e.g. the string `"true"`) ⇒ `"expected boolean for `key`,
/// got string"` (NWS-2944).
pub(crate) fn arg_bool(args: &Value, key: &str) -> Result<bool, String> {
    match optional_arg(args, key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!(
            "expected boolean for `{key}`, got {}",
            json_type_name(other)
        )),
    }
}

/// Optional boolean whose absence means something else than `false` —
/// `figmog_spec`'s `tailwind` (absent: the project's default). Absent or
/// `null` ⇒ `Ok(None)`; any other non-boolean is an error.
pub(crate) fn arg_bool_opt(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match optional_arg(args, key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(format!(
            "expected boolean for `{key}`, got {}",
            json_type_name(other)
        )),
    }
}

/// Optional numeric field — [`require_f64`]'s absent-is-`None` sibling,
/// used by `figmog_images`'s optional `scale` (v0.0.2 spec §5). Absent or
/// `null` ⇒ `Ok(None)`; any other non-number is an error (NWS-2944).
pub(crate) fn arg_f64(args: &Value, key: &str) -> Result<Option<f64>, String> {
    match optional_arg(args, key) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("expected number for `{key}`, got {}", json_type_name(v))),
    }
}

/// Optional array-of-strings field — `figmog_subtree`'s `fields`.
/// Absent/`null` ⇒ `Ok(None)`; an empty array is allowed (project to the
/// always-kept keys only). A non-array, or a non-string element, is named
/// precisely, mirroring [`require_str_array`] (NWS-2944 — a bare string
/// used to be silently ignored, returning every field).
pub(crate) fn arg_str_array(args: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    match optional_arg(args, key) {
        None => Ok(None),
        Some(Value::Array(items)) => str_items(key, items).map(Some),
        Some(other) => Err(format!(
            "expected array for `{key}`, got {}",
            json_type_name(other)
        )),
    }
}

fn str_items(key: &str, items: &[Value]) -> Result<Vec<String>, String> {
    items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_str().map(str::to_string).ok_or_else(|| {
                format!(
                    "expected string for `{key}[{i}]`, got {}",
                    json_type_name(v)
                )
            })
        })
        .collect()
}

/// A required array-of-strings field — `figmog_images`'s `ids` (v0.0.2
/// spec §5). Same present-but-wrong-type distinction as [`require_str`];
/// additionally rejects an empty array (nothing to fetch) and a non-string
/// element, both named precisely rather than surfacing as a generic
/// downstream failure.
pub(crate) fn require_str_array(args: &Value, key: &str) -> Result<Vec<String>, String> {
    match args.get(key) {
        None => Err(format!("missing required field: {key}")),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err(format!("`{key}` must not be empty"));
            }
            str_items(key, items)
        }
        Some(other) => Err(format!(
            "expected array for `{key}`, got {}",
            json_type_name(other)
        )),
    }
}

/// See [`require_str`]'s doc comment — same present-but-wrong-type
/// distinction, for numeric fields.
pub(crate) fn require_f64(args: &Value, key: &str) -> Result<f64, String> {
    match args.get(key) {
        None => Err(format!("missing required field: {key}")),
        Some(v) => v
            .as_f64()
            .ok_or_else(|| format!("expected number for `{key}`, got {}", json_type_name(v))),
    }
}

/// The file key named by a full Figma URL in whichever node-id-shaped
/// argument this call carries (spec §2b): tries `id`, then `under`, then
/// `target`, in that order — a given tool call has at most one of these,
/// so trying all three is safe. `None` when no such argument is present,
/// or it's present but isn't a URL naming a file (a bare id, or a URL
/// with no file segment). Used by `figmog serve`'s multi-file session
/// routing (`serve.rs::handle_tool_call`) to auto-infer the target mirror
/// from a pasted node/frame URL when the call omits an explicit `file`.
pub(crate) fn infer_file_from_node_ref(args: &Value) -> Option<String> {
    ["id", "under", "target"].iter().find_map(|key| {
        // Best-effort routing hint only: a wrong-typed value can't name a
        // file, and the dispatched tool itself reports the type error.
        let raw = args.get(key)?.as_str()?;
        crate::ident::parse_node_ref(raw)?.0
    })
}

/// `figmog_search`'s `limit` when omitted (or `null`), and `figmog search
/// -n`'s clap default — one constant so the two adapters can't drift.
pub(crate) const DEFAULT_SEARCH_LIMIT: usize = 10;

/// One of the 18 local reads of the mirror, with typed, already-validated
/// arguments (NWS-2902). Every adapter converges here: tool JSON (MCP,
/// the socket, `figmog call`) via [`ReadCommand::from_tool_args`], the
/// clap read commands via `cli::read_command`, and the CLI's socket
/// client back to tool JSON via [`ReadCommand::to_tool_call`]. Each
/// variant has exactly one query arm, in [`ReadCommand::execute`].
///
/// Defaults are explicit in the payload: an omitted (or `null`) optional
/// is `None`/`false`, except `Search::limit`, which is resolved to
/// [`DEFAULT_SEARCH_LIMIT`]. `Where::equals` is `None` only when the key
/// is omitted — `Some(Value::Null)` is equality to null (NWS-2897).
/// Routing (`file`, URL-inferred files) and status transport metadata
/// stay with the adapters, not here.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReadCommand {
    Status,
    Pages,
    Tree {
        id: Option<String>,
        depth: Option<usize>,
    },
    Node {
        id: String,
        children: bool,
        resolve_vars: bool,
    },
    Subtree {
        id: String,
        depth: Option<usize>,
        fields: Option<Vec<String>>,
        resolve_vars: bool,
    },
    Find {
        node_type: String,
        page: Option<String>,
        under: Option<String>,
    },
    Search {
        query: String,
        limit: usize,
        under: Option<String>,
    },
    Instances {
        target: String,
    },
    Components,
    Styles {
        style_type: Option<String>,
        values: bool,
        resolve_vars: bool,
    },
    Uses {
        id: String,
    },
    Vars {
        id: Option<String>,
        snapshot: bool,
    },
    Stats,
    Path {
        id: String,
    },
    Text {
        page: Option<String>,
        under: Option<String>,
    },
    Where {
        pointer: String,
        equals: Option<Value>,
        page: Option<String>,
        under: Option<String>,
    },
    At {
        x: f64,
        y: f64,
    },
    /// `figmog_spec` (NWS-2968): binding-aware layout/value spec of a subtree.
    Spec {
        id: String,
        /// How the token names and root modes are derived: the project's
        /// `[tokens]` settings (NWS-2948), injected by the CLI and the
        /// stdio relay; `None` is the default mapping.
        tokens: Option<crate::tokens::ExportOptions>,
        /// Add the per-node Tailwind class hint (NWS-2947). `None`: the
        /// CLI and the stdio relay fill it from the project's `[stack]
        /// styling`; left `None`, no hint.
        tailwind: Option<bool>,
        /// The Figma → code component map (NWS-2949): the project's
        /// `figmog.map.json`, injected by the CLI and the stdio relay;
        /// `None`, every `code_component` is null.
        map: Option<crate::map::CodeMap>,
        /// The asset manifest (NWS-2952): the project's `[assets]
        /// manifest`, injected by the CLI and the stdio relay; `None`,
        /// assets are referenced by node id and imageRef only.
        assets: Option<crate::assets::AssetIndex>,
    },
}

impl ReadCommand {
    /// Parse a local read tool call. `Ok(None)` for any name that isn't
    /// one of the 18 local reads (`figmog_sync`, `figmog_images`,
    /// management and desktop tools are the caller's business). Arguments
    /// are validated in schema order with the `arg_*`/`require_*`
    /// helpers, so the first bad argument names the error (NWS-2944).
    /// Unknown keys — the routing `file` among them — are ignored.
    pub(crate) fn from_tool_args(name: &str, args: &Value) -> Result<Option<Self>, String> {
        Ok(Some(match name {
            "figmog_status" => Self::Status,
            "figmog_pages" => Self::Pages,
            "figmog_tree" => Self::Tree {
                id: arg_str(args, "id")?,
                depth: arg_usize(args, "depth")?,
            },
            "figmog_node" => Self::Node {
                id: require_str(args, "id")?,
                children: arg_bool(args, "children")?,
                resolve_vars: arg_bool(args, "resolve_vars")?,
            },
            "figmog_subtree" => Self::Subtree {
                id: require_str(args, "id")?,
                depth: arg_usize(args, "depth")?,
                fields: arg_str_array(args, "fields")?,
                resolve_vars: arg_bool(args, "resolve_vars")?,
            },
            "figmog_find" => Self::Find {
                node_type: require_str(args, "type")?,
                page: arg_str(args, "page")?,
                under: arg_str(args, "under")?,
            },
            "figmog_search" => Self::Search {
                query: require_str(args, "query")?,
                limit: arg_usize(args, "limit")?.unwrap_or(DEFAULT_SEARCH_LIMIT),
                under: arg_str(args, "under")?,
            },
            "figmog_instances" => Self::Instances {
                target: require_str(args, "target")?,
            },
            "figmog_components" => Self::Components,
            "figmog_styles" => Self::Styles {
                style_type: arg_str(args, "type")?,
                values: arg_bool(args, "values")?,
                resolve_vars: arg_bool(args, "resolve_vars")?,
            },
            "figmog_uses" => Self::Uses {
                id: require_str(args, "id")?,
            },
            "figmog_vars" => Self::Vars {
                id: arg_str(args, "id")?,
                snapshot: arg_bool(args, "snapshot")?,
            },
            "figmog_stats" => Self::Stats,
            "figmog_path" => Self::Path {
                id: require_str(args, "id")?,
            },
            "figmog_text" => Self::Text {
                page: arg_str(args, "page")?,
                under: arg_str(args, "under")?,
            },
            "figmog_where" => Self::Where {
                pointer: require_str(args, "pointer")?,
                // Presence means equality to this JSON value, including
                // null; only an omitted key means "pointer exists".
                equals: args.get("equals").cloned(),
                page: arg_str(args, "page")?,
                under: arg_str(args, "under")?,
            },
            "figmog_at" => Self::At {
                x: require_f64(args, "x")?,
                y: require_f64(args, "y")?,
            },
            "figmog_spec" => Self::Spec {
                id: require_str(args, "id")?,
                tokens: match args.get("tokens") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(crate::tokens::ExportOptions::from_json(v)?),
                },
                tailwind: arg_bool_opt(args, "tailwind")?,
                map: match args.get("map") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(crate::map::CodeMap::from_json(v, "map")?),
                },
                assets: match args.get("assets") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(crate::assets::AssetIndex::from_json(v, "assets")?),
                },
            },
            _ => return Ok(None),
        }))
    }

    /// The local tool name and arguments that [`Self::from_tool_args`]
    /// parses back to `self` — what the CLI sends over the socket (spec
    /// §6a). `None` optionals are omitted rather than sent as `null`;
    /// flags and `limit` are always sent; `equals` is sent (possibly as
    /// `null`) exactly when present.
    pub(crate) fn to_tool_call(&self) -> (&'static str, Value) {
        fn put<T: serde::Serialize>(args: &mut Value, key: &str, value: Option<T>) {
            if let Some(value) = value {
                args[key] = json!(value);
            }
        }
        let mut args = json!({});
        let name = match self {
            Self::Status => "figmog_status",
            Self::Pages => "figmog_pages",
            Self::Tree { id, depth } => {
                put(&mut args, "id", id.as_ref());
                put(&mut args, "depth", *depth);
                "figmog_tree"
            }
            Self::Node {
                id,
                children,
                resolve_vars,
            } => {
                args = json!({"id": id, "children": children, "resolve_vars": resolve_vars});
                "figmog_node"
            }
            Self::Subtree {
                id,
                depth,
                fields,
                resolve_vars,
            } => {
                args = json!({"id": id, "resolve_vars": resolve_vars});
                put(&mut args, "depth", *depth);
                put(&mut args, "fields", fields.as_ref());
                "figmog_subtree"
            }
            Self::Find {
                node_type,
                page,
                under,
            } => {
                args = json!({"type": node_type});
                put(&mut args, "page", page.as_ref());
                put(&mut args, "under", under.as_ref());
                "figmog_find"
            }
            Self::Search {
                query,
                limit,
                under,
            } => {
                args = json!({"query": query, "limit": limit});
                put(&mut args, "under", under.as_ref());
                "figmog_search"
            }
            Self::Instances { target } => {
                args = json!({"target": target});
                "figmog_instances"
            }
            Self::Components => "figmog_components",
            Self::Styles {
                style_type,
                values,
                resolve_vars,
            } => {
                args = json!({"values": values, "resolve_vars": resolve_vars});
                put(&mut args, "type", style_type.as_ref());
                "figmog_styles"
            }
            Self::Uses { id } => {
                args = json!({"id": id});
                "figmog_uses"
            }
            Self::Vars { id, snapshot } => {
                args = json!({"snapshot": snapshot});
                put(&mut args, "id", id.as_ref());
                "figmog_vars"
            }
            Self::Stats => "figmog_stats",
            Self::Path { id } => {
                args = json!({"id": id});
                "figmog_path"
            }
            Self::Text { page, under } => {
                put(&mut args, "page", page.as_ref());
                put(&mut args, "under", under.as_ref());
                "figmog_text"
            }
            Self::Where {
                pointer,
                equals,
                page,
                under,
            } => {
                args = json!({"pointer": pointer});
                put(&mut args, "equals", equals.as_ref());
                put(&mut args, "page", page.as_ref());
                put(&mut args, "under", under.as_ref());
                "figmog_where"
            }
            Self::At { x, y } => {
                args = json!({"x": x, "y": y});
                "figmog_at"
            }
            Self::Spec {
                id,
                tokens,
                tailwind,
                map,
                assets,
            } => {
                args = json!({"id": id});
                if let Some(tokens) = tokens {
                    args["tokens"] = tokens.to_json();
                }
                if let Some(map) = map {
                    args["map"] = map.to_json();
                }
                if let Some(assets) = assets {
                    args["assets"] = assets.to_json();
                }
                put(&mut args, "tailwind", *tailwind);
                "figmog_spec"
            }
        };
        (name, args)
    }

    /// Run the query against one open snapshot — the single semantic arm
    /// per local read. Returns the `query::*` value unchanged; adapters
    /// add transport metadata (`figmog_status`'s `upstream`, serve's
    /// `ready`/`pull`) themselves.
    pub(crate) fn execute<R: Readable>(self, r: &RootReaders<'_, R>) -> Result<Value, String> {
        self.execute_counted(r, None)
    }

    /// [`execute`](Self::execute), with the snapshot's node count when the
    /// caller already knows it (a published serve view, NWS-2942), so
    /// `figmog_status` doesn't count the `nodes` table. Other reads ignore it.
    pub(crate) fn execute_counted<R: Readable>(
        self,
        r: &RootReaders<'_, R>,
        node_count: Option<usize>,
    ) -> Result<Value, String> {
        let (
            (nodes, children, text, instances_of, styled_by, bound_to, by_type),
            components,
            component_sets,
            styles,
            variables,
            variable_collections,
            meta,
            _cache,
            _mirror_config,
            _images,
            _image_pins,
            variable_snapshot,
        ) = r;
        match self {
            Self::Status => query::status(
                nodes,
                meta,
                variables,
                variable_collections,
                variable_snapshot,
                node_count,
            ),
            Self::Pages => query::pages(nodes, by_type),
            Self::Tree { id, depth } => query::tree(nodes, children, by_type, id, depth),
            Self::Node {
                id,
                children: with_children,
                resolve_vars,
            } => query::node(
                nodes,
                children,
                variables,
                variable_collections,
                id,
                with_children,
                resolve_vars,
            ),
            Self::Subtree {
                id,
                depth,
                fields,
                resolve_vars,
            } => query::subtree(
                nodes,
                children,
                variables,
                variable_collections,
                id,
                depth,
                fields.as_deref(),
                resolve_vars,
            ),
            Self::Find {
                node_type,
                page,
                under,
            } => query::find(nodes, children, by_type, node_type, page, under),
            Self::Search {
                query: q,
                limit,
                under,
            } => query::search(text, nodes, children, &q, limit, under),
            Self::Instances { target } => {
                query::instances(nodes, components, component_sets, instances_of, &target)
            }
            Self::Components => query::components(nodes, components, component_sets),
            Self::Styles {
                style_type,
                values,
                resolve_vars,
            } => query::styles(
                nodes,
                styles,
                styled_by,
                variables,
                variable_collections,
                style_type,
                values,
                resolve_vars,
            ),
            Self::Uses { id } => query::uses(nodes, styled_by, bound_to, &id),
            Self::Vars { id, snapshot } => query::vars(
                nodes,
                variables,
                variable_collections,
                variable_snapshot,
                id,
                snapshot,
            ),
            Self::Stats => query::stats(
                nodes,
                components,
                component_sets,
                styles,
                variables,
                by_type,
            ),
            Self::Path { id } => query::path(nodes, id),
            Self::Text { page, under } => query::text(nodes, children, by_type, page, under),
            Self::Where {
                pointer,
                equals,
                page,
                under,
            } => query::where_(nodes, children, &pointer, equals, page, under),
            Self::At { x, y } => query::at(nodes, x, y),
            Self::Spec {
                id,
                tokens,
                tailwind,
                map,
                assets,
            } => query::spec(
                nodes,
                children,
                components,
                component_sets,
                variables,
                variable_collections,
                variable_snapshot,
                id,
                &tokens.unwrap_or_default(),
                tailwind.unwrap_or(false),
                map.as_ref(),
                assets.as_ref(),
            ),
        }
    }
}

/// Dispatch one of the 18 read-only `figmog_*` tools against an open
/// snapshot: [`ReadCommand::from_tool_args`] then
/// [`ReadCommand::execute`]. `upstream_status` is spliced into
/// `figmog_status`'s output (`"connected"` / `"unreachable"` /
/// `"disabled"`) without changing `query::status`'s own signature (spec
/// §12 point 4). `node_count` is passed through to
/// [`ReadCommand::execute_counted`].
///
/// Returns `None` for any name this function doesn't recognize — `figmog_*`
/// names it doesn't know (a genuine bug, since the registry gates
/// `tools/call` before this is reached) as well as `figmog_sync` and every
/// non-local name, both of which the caller handles itself.
pub(crate) fn dispatch_read_tool<R: Readable>(
    name: &str,
    args: &Value,
    upstream_status: &str,
    r: &RootReaders<'_, R>,
    node_count: Option<usize>,
) -> Option<Result<Value, String>> {
    let cmd = match ReadCommand::from_tool_args(name, args) {
        Ok(Some(cmd)) => cmd,
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    let is_status = cmd == ReadCommand::Status;
    Some(cmd.execute_counted(r, node_count).map(|mut v| {
        if is_status && let Some(obj) = v.as_object_mut() {
            obj.insert("upstream".to_string(), json!(upstream_status));
        }
        v
    }))
}

/// The optional `file` property every local tool's schema carries as of
/// v4 (spec §14): a Figma file URL or key, routed by `SessionManager`
/// (`sessions.rs`) to the mirror it names, auto-opening it (one Tier-1
/// pull) if it's new. Omitted, a tool targets the default mirrored file.
fn file_arg_property() -> Value {
    json!({
        "type": "string",
        "description": "Figma file URL or key; omit for the default mirrored file. First reference queues a background pull; data reads report not-ready until success. Poll figmog_status or figmog_files for progress."
    })
}

/// The 22 `figmog_*` MCP tools (spec §14, v4; `figmog_subtree` added by
/// v0.0.2 §2, `figmog_images` by v0.0.2 §5, `figmog_spec` by NWS-2968): 18
/// reads of the local mirror (`figmog_subtree` and `figmog_spec` among
/// them) plus `figmog_sync` and `figmog_images` — 20 total — every one of which gains the optional `file` routing
/// property below — plus the two v4 additions, `figmog_open` and
/// `figmog_files`, which don't (routing *to* a file, and listing every
/// file, aren't themselves per-file operations). Every tool but
/// `figmog_sync`/`figmog_open`/`figmog_images` reads the local mirror at
/// zero Figma API cost — `figmog_images` is deliberately never called
/// automatically by anything in figmog (spec §5: the images endpoints are
/// Figma's most rate-limited).
pub(crate) fn tool_registry() -> Vec<ToolDef> {
    let mut tools = vec![
        ToolDef {
            name: "figmog_status",
            description: "Committed file name/version/time/node count plus ready and pull lifecycle (job progress, last successful version/churn and errors). Local reads remain available during a pull; ready=false means no successful mirror yet. No Figma API cost.",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_pages",
            description: "List the file's pages (CANVAS nodes), in document order — reads the local mirror (no Figma API cost).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_tree",
            description: "Subtree outline (id, name, type, children) rooted at a node, defaulting to the whole document — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Root node id; defaults to the DOCUMENT node."},
                    "depth": {"type": "integer", "description": "Max depth to descend; omitted means unlimited."}
                }
            }),
        },
        ToolDef {
            name: "figmog_node",
            description: "Full raw JSON of one node by id, optionally with a one-level children summary and/or a resolved_variables annotation — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Node id (12:34 or 12-34 form)."},
                    "children": {"type": "boolean", "description": "Inline a one-level children summary."},
                    "resolve_vars": {"type": "boolean", "description": "Annotate boundVariables binding sites with variable names/values under a resolved_variables key."}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_subtree",
            description: "Full raw JSON of a node and its descendants, nested under `children` in child-index order — reads the local mirror (no Figma API cost). Use `depth` and `fields` to keep the response small; an unbounded dump of a large subtree can be huge.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Root node id (12:34 or 12-34 form)."},
                    "depth": {"type": "integer", "description": "Max depth to descend; omitted means unlimited."},
                    "fields": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Project every node to these raw fields (id/name/type/children always survive). Recommended for large subtrees."
                    },
                    "resolve_vars": {"type": "boolean", "description": "Annotate boundVariables binding sites with variable names/values under a resolved_variables key."}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_find",
            description: "Nodes by Figma node type, optionally scoped to one page and/or a subtree — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "type": {"type": "string", "description": "Figma node type, e.g. FRAME."},
                    "page": {"type": "string", "description": "Page (CANVAS) node id to scope to."},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                },
                "required": ["type"]
            }),
        },
        ToolDef {
            name: "figmog_search",
            description: "BM25 search over layer names and text content, optionally scoped to a subtree — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "description": "Max hits (default 10)."},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "figmog_instances",
            description: "Instances of a component, resolved by node id, global key, or component/component-set name — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Node id, key, or name of a component or component set."}
                },
                "required": ["target"]
            }),
        },
        ToolDef {
            name: "figmog_components",
            description: "Design-system inventory: component sets with their variant axes, plus standalone components — reads the local mirror (no Figma API cost).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_styles",
            description: "Styles with usage counts; `values` derives each style's definition from a consumer node — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "type": {"type": "string", "description": "Style type filter, e.g. FILL, TEXT."},
                    "values": {"type": "boolean", "description": "Derive each style's definition from a consumer node."},
                    "resolve_vars": {"type": "boolean", "description": "With values, annotate the definition's variable bindings under a resolved_variables key."}
                }
            }),
        },
        ToolDef {
            name: "figmog_uses",
            description: "Nodes using a style id or bound to a variable id — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "A style id or variable id."}},
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_vars",
            description: "Variables: the authoritative record (with default_mode, scopes, code_syntax, hidden_from_publishing, remote) if imported via figmog import-variables or an Enterprise pull, else inferred from bindings — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Variable id filter; omitted means all variables."},
                    "snapshot": {"type": "boolean", "description": "Return {snapshot, variables}: the variable snapshot's state (none, legacy, empty, current) and provenance next to the rows."}
                }
            }),
        },
        ToolDef {
            name: "figmog_sync",
            description: "Queue a background pull from Figma. Returns {accepted, key, job_id, state} immediately; poll figmog_status or figmog_files for completion, version/churn or errors. Duplicate active requests coalesce. Spends Tier-1 rate budget. Direct CLI sync without serve remains synchronous.",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_stats",
            description: "Node counts by type and by page, component/set/style/variable totals, text-node count, max tree depth — reads the local mirror (no Figma API cost).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_path",
            description: "Ancestor chain from the document root to a node, as [{id, name, type}] — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_text",
            description: "Every TEXT node's (id, characters, page_id), optionally scoped to one page and/or a subtree, sorted by id — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "page": {"type": "string"},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                }
            }),
        },
        ToolDef {
            name: "figmog_where",
            description: "Nodes whose raw JSON matches an RFC 6901 pointer, optionally filtered by value and/or scoped to a page/subtree — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pointer": {"type": "string", "description": "RFC 6901 pointer into the node's raw JSON, e.g. /layoutMode."},
                    "equals": {"description": "JSON value to match, including null; omitted means \"pointer exists\"."},
                    "page": {"type": "string"},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                },
                "required": ["pointer"]
            }),
        },
        ToolDef {
            name: "figmog_images",
            description: "Download node renders and/or fill images (v0.0.2 spec §5), cached by file version — spends Figma API budget (the images endpoints are the most rate-limited; never called automatically). Returns a JSON manifest plus, per item up to 1MB encoded: an image content block (base64) for raster formats, or the raw SVG markup as a text block for format=svg; larger items point at the CLI's `figmog images --out` instead.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Node ids (or Figma URLs carrying node-id=) to render, and/or to scan for fill imageRefs."
                    },
                    "format": {"type": "string", "description": "Render format: png or svg (default png)."},
                    "scale": {"type": "number", "description": "Render scale (default 1)."},
                    "renders": {"type": "boolean", "description": "Render the nodes (default true). false: only fetch the image fills their own `fills` reference."}
                },
                "required": ["ids"]
            }),
        },
        ToolDef {
            name: "figmog_at",
            description: "Nodes whose absolute bounds contain a point, sorted by area ascending (deepest/smallest first) — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": {"type": "number"},
                    "y": {"type": "number"}
                },
                "required": ["x", "y"]
            }),
        },
        ToolDef {
            name: "figmog_spec",
            description: "Code-ready spec of a subtree. Per node, two layers: `design` (the layout Figma recorded: auto layout, grid, sizing, constraints, min/max, clipping, text auto-resize, verbatim) and figmog's interpretation — `layout`/`values` (gap, padding, alignment, hug/fill/fixed, grid tracks, fill, stroke, radius, effects, opacity, typography), each value shown as its bound variable token (name plus the CSS name `figmog tokens export` emits) with the raw value, or flagged untokenized (a token of equal value is only ever named under `matches`); `css` (the declarations: flex or CSS grid, constraints as absolute offsets, tokens as var()); `content` (text, mixed runs, image/vector assets by node id and imageRef); `instance` (component, variant properties, and `code_component`: the code component the project's figmog.map.json maps it to — path, export, mapped props, a JSX rendering, and `coverage` \"full\" or \"partial\" with every unmapped property, override (text, swap, nested) and slot listed — else null); and, with `tailwind`, a Tailwind class hint. Whatever has no exact CSS is listed under `unsupported`, never approximated — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Root node id (12:34 or 12-34 form, or a Figma URL)."},
                    "tokens": {
                        "type": "object",
                        "description": "How token CSS names and root modes are derived, as figmog.toml's [tokens]: {format: \"tailwind\"|\"css\", modes: {\"<collection>/<mode>\": \"<selector>\"}}. The figmog CLI and stdio relay pass the project's settings; omit for the default mapping.",
                        "properties": {
                            "format": {"type": "string", "enum": ["tailwind", "css"]},
                            "modes": {"type": "object", "additionalProperties": {"type": "string"}}
                        }
                    },
                    "tailwind": {
                        "type": "boolean",
                        "description": "Add a Tailwind class hint per node (theme utilities for bound tokens, arbitrary values otherwise). The figmog CLI and stdio relay turn it on when the project's [stack] styling is tailwind; omit for none."
                    },
                    "map": {
                        "type": "object",
                        "description": "The Figma → code component map, as figmog.map.json: {version: 1, components: [{figma: {component|component_set: <key>}, code: {path, export}, props?: {\"<Figma property>\": {prop, values?}}}]}. The figmog CLI and stdio relay pass the project's figmog.map.json; omit for none (every code_component null)."
                    },
                    "assets": {
                        "type": "object",
                        "description": "The asset manifest `figmog assets export` writes ([assets] manifest, default figmog.assets.json): {icons: [{node_id, key, component, path, svg}], images: [{image_ref, path, url}]}. The figmog CLI and stdio relay pass the project's manifest; omit for none (assets by node id and imageRef only)."
                    }
                },
                "required": ["id"]
            }),
        },
    ];

    for t in tools.iter_mut() {
        if let Some(props) = t
            .input_schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            props.insert("file".to_string(), file_arg_property());
        }
    }

    tools.push(ToolDef {
        name: "figmog_open",
        description: "Queue a background mirror pull (Tier-1 API cost). Returns {accepted, key, job_id, state} immediately. Poll figmog_status or figmog_files for ready, job progress, version/churn or errors. Duplicate active requests coalesce; a geometry upgrade can queue one follow-up.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "file": {"type": "string", "description": "Figma file URL or key to mirror."},
                "geometry": {
                    "type": "boolean",
                    "description": "Request vector path data (fillGeometry/strokeGeometry) on this pull. Sticky (spec §4): once set, every later pull of this mirror keeps requesting it until a `pull --fresh`. Omitted preserves whatever this mirror already has stored; default false for a brand-new mirror."
                }
            },
            "required": ["file"]
        }),
    });
    tools.push(ToolDef {
        name: "figmog_files",
        description: "List every session: key, committed name/version/node count/time, default, ready, and pull lifecycle (job, queued, last_completed, last_success with version/churn). Failed jobs retain actionable errors and the previous successful mirror. Local state only (no Figma API cost).",
        input_schema: json!({"type": "object", "properties": {}}),
    });

    tools
}

/// Open/sync's server-side acknowledgement contract. Other local tools
/// retain their existing text-content protocol (including array results).
pub(crate) fn output_schema(name: &str) -> Option<Value> {
    matches!(name, "figmog_open" | "figmog_sync").then(|| {
        json!({
            "type": "object",
            "properties": {
                "accepted": {"const": true},
                "key": {"type": "string"},
                "job_id": {"type": "integer", "minimum": 1},
                "state": {"enum": ["queued", "running"]}
            },
            "required": ["accepted", "key", "job_id", "state"],
            "additionalProperties": false
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NWS-2952: `figmog_spec`'s `assets` (the manifest, normalized)
    /// round-trips; an unsafe manifest is the call's error.
    #[test]
    fn spec_assets_argument_round_trips_and_is_validated() {
        let manifest = json!({
            "icons": [{"node_id": "1:2", "key": "abc", "component": "IconStar",
                       "path": "src/icons/IconStar.tsx", "svg": "src/icons/svg/IconStar.svg"}],
            "images": [{"image_ref": "ff00", "path": "public/figma/x.png", "url": "/figma/x.png"}],
        });
        let read =
            ReadCommand::from_tool_args("figmog_spec", &json!({"id": "1:1", "assets": manifest}))
                .unwrap()
                .unwrap();
        let (name, args) = read.to_tool_call();
        assert_eq!(name, "figmog_spec");
        assert_eq!(args["assets"]["icons"][0]["component"], "IconStar");
        assert_eq!(ReadCommand::from_tool_args(name, &args), Ok(Some(read)));
        let err = ReadCommand::from_tool_args(
            "figmog_spec",
            &json!({"id": "1:1", "assets": {"images": [{"image_ref": "x", "path": "../up.png"}]}}),
        )
        .unwrap_err();
        assert!(err.starts_with("assets: images[0].path:"), "{err}");
    }

    #[test]
    fn require_str_reports_wrong_type_not_missing() {
        let err = require_str(&json!({"id": 12}), "id").unwrap_err();
        assert_eq!(err, "expected string for `id`, got number");
    }

    #[test]
    fn require_str_reports_missing_when_key_absent() {
        let err = require_str(&json!({}), "id").unwrap_err();
        assert_eq!(err, "missing required field: id");
    }

    #[test]
    fn require_f64_reports_wrong_type_not_missing() {
        let err = require_f64(&json!({"x": "12"}), "x").unwrap_err();
        assert_eq!(err, "expected number for `x`, got string");
    }

    #[test]
    fn require_f64_reports_missing_when_key_absent() {
        let err = require_f64(&json!({}), "x").unwrap_err();
        assert_eq!(err, "missing required field: x");
    }

    #[test]
    fn optional_args_absent_or_null_keep_defaults() {
        for args in [json!({}), json!({"k": null})] {
            assert_eq!(arg_str(&args, "k"), Ok(None));
            assert_eq!(arg_usize(&args, "k"), Ok(None));
            assert_eq!(arg_bool(&args, "k"), Ok(false));
            assert_eq!(arg_f64(&args, "k"), Ok(None));
            assert_eq!(arg_str_array(&args, "k"), Ok(None));
        }
    }

    #[test]
    fn optional_args_accept_their_own_type() {
        assert_eq!(arg_str(&json!({"k": "a"}), "k"), Ok(Some("a".into())));
        assert_eq!(arg_usize(&json!({"k": 0}), "k"), Ok(Some(0)));
        assert_eq!(arg_usize(&json!({"k": 7}), "k"), Ok(Some(7)));
        assert_eq!(arg_bool(&json!({"k": true}), "k"), Ok(true));
        assert_eq!(arg_f64(&json!({"k": 1.5}), "k"), Ok(Some(1.5)));
        assert_eq!(arg_f64(&json!({"k": 2}), "k"), Ok(Some(2.0)));
        assert_eq!(arg_str_array(&json!({"k": []}), "k"), Ok(Some(vec![])));
    }

    #[test]
    fn optional_args_reject_wrong_types() {
        let cases: [(Result<(), String>, &str); 10] = [
            (
                arg_usize(&json!({"depth": "2"}), "depth").map(drop),
                "expected integer for `depth`, got string",
            ),
            (
                arg_usize(&json!({"depth": 1.5}), "depth").map(drop),
                "expected integer for `depth`, got number",
            ),
            (
                arg_usize(&json!({"depth": -1}), "depth").map(drop),
                "expected non-negative integer for `depth`, got -1",
            ),
            (
                arg_usize(&json!({"limit": true}), "limit").map(drop),
                "expected integer for `limit`, got boolean",
            ),
            (
                arg_bool(&json!({"children": "true"}), "children").map(drop),
                "expected boolean for `children`, got string",
            ),
            (
                arg_bool(&json!({"children": 1}), "children").map(drop),
                "expected boolean for `children`, got number",
            ),
            (
                arg_f64(&json!({"scale": "2"}), "scale").map(drop),
                "expected number for `scale`, got string",
            ),
            (
                arg_str(&json!({"page": ["0:1"]}), "page").map(drop),
                "expected string for `page`, got array",
            ),
            (
                arg_str_array(&json!({"fields": "name"}), "fields").map(drop),
                "expected array for `fields`, got string",
            ),
            (
                arg_str_array(&json!({"fields": ["name", {}]}), "fields").map(drop),
                "expected string for `fields[1]`, got object",
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, Err(want.to_string()));
        }
    }

    #[test]
    fn infer_file_ignores_wrong_typed_node_refs() {
        assert_eq!(infer_file_from_node_ref(&json!({"id": 12})), None);
    }

    /// NWS-2902: only the 18 local reads become a `ReadCommand`; sync,
    /// images, management and desktop tools stay with their callers.
    #[test]
    fn read_command_covers_exactly_the_local_reads() {
        let reads: Vec<_> = tool_registry()
            .iter()
            .filter(|t| {
                ReadCommand::from_tool_args(
                    t.name,
                    &json!({"id": "1:1", "type": "T",
                    "query": "q", "target": "t", "pointer": "/a", "x": 0, "y": 0}),
                )
                .unwrap()
                .is_some()
            })
            .map(|t| t.name)
            .collect();
        assert_eq!(reads.len(), 18, "{reads:?}");
        for name in [
            "figmog_sync",
            "figmog_images",
            "figmog_open",
            "figmog_files",
            "get_code",
        ] {
            assert_eq!(
                ReadCommand::from_tool_args(name, &json!({})),
                Ok(None),
                "{name}"
            );
        }
        // Explicit-null `equals` is a value; an omitted one is not.
        let parse = |args| {
            ReadCommand::from_tool_args("figmog_where", &args)
                .unwrap()
                .unwrap()
        };
        assert!(matches!(
            parse(json!({"pointer": "/a", "equals": null})),
            ReadCommand::Where {
                equals: Some(Value::Null),
                ..
            }
        ));
        assert!(matches!(
            parse(json!({"pointer": "/a"})),
            ReadCommand::Where { equals: None, .. }
        ));
        assert!(matches!(
            ReadCommand::from_tool_args("figmog_search", &json!({"query": "q", "limit": null})),
            Ok(Some(ReadCommand::Search {
                limit: DEFAULT_SEARCH_LIMIT,
                ..
            }))
        ));
    }
}
