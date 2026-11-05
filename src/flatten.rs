//! Pure flattening of a Figma file response into deterministic records.
//!
//! No I/O, no clock, no randomness: two calls on equal JSON must produce
//! byte-identical records (postcard), because `KeyedStream::upsert` uses
//! byte equality as its change detector.

use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;

use crate::model::{ComponentRec, ComponentSetRec, Id, NodeRec, Rec, StyleRec};

/// File-level fields lifted from the response envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub name: String,
    pub version: String,
    pub last_modified: String,
}

/// Everything `flatten_file` extracts.
#[derive(Debug)]
pub struct Flattened {
    pub recs: Vec<(Id, Rec)>,
    pub file: FileInfo,
}

/// Errors from [`flatten_file`].
#[derive(Debug, thiserror::Error)]
pub enum FlattenError {
    #[error("missing field: {0}")]
    Missing(&'static str),
    #[error("invalid node at {path}: {reason}")]
    InvalidNode { path: String, reason: &'static str },
}

/// Flatten a full `GET /v1/files/:key` response, rejecting malformed trees
/// before any caller can apply a partial record set to a mirror.
pub fn flatten_file(resp: &Value) -> Result<Flattened, FlattenError> {
    let file = FileInfo {
        name: str_field(resp, "name").ok_or(FlattenError::Missing("name"))?,
        version: str_field(resp, "version").ok_or(FlattenError::Missing("version"))?,
        last_modified: str_field(resp, "lastModified")
            .ok_or(FlattenError::Missing("lastModified"))?,
    };
    let document = resp
        .get("document")
        .ok_or(FlattenError::Missing("document"))?;

    let mut recs = Vec::new();
    walk(document, &mut recs)?;

    // Borrowed, not cloned: every entry is read once here (a handful of
    // `str_field`/`get` lookups) and dropped, so there's no reason to clone
    // each envelope `Value` just to iterate it. `BTreeMap` keeps the same
    // deterministic (sorted-by-id) iteration order as before.
    let obj_map = |key: &str| -> BTreeMap<&str, &Value> {
        resp.get(key)
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect())
            .unwrap_or_default()
    };
    for (node_id, v) in obj_map("components") {
        recs.push((
            Id::Component(node_id.to_string()),
            Rec::Component(ComponentRec {
                node_id: node_id.to_string(),
                key: str_field(v, "key").unwrap_or_default(),
                name: str_field(v, "name").unwrap_or_default(),
                description: str_field(v, "description").unwrap_or_default(),
                component_set_id: str_field(v, "componentSetId"),
                remote: v.get("remote").and_then(Value::as_bool).unwrap_or(false),
            }),
        ));
    }
    for (node_id, v) in obj_map("componentSets") {
        recs.push((
            Id::ComponentSet(node_id.to_string()),
            Rec::ComponentSet(ComponentSetRec {
                node_id: node_id.to_string(),
                key: str_field(v, "key").unwrap_or_default(),
                name: str_field(v, "name").unwrap_or_default(),
                description: str_field(v, "description").unwrap_or_default(),
                remote: v.get("remote").and_then(Value::as_bool).unwrap_or(false),
            }),
        ));
    }
    for (style_id, v) in obj_map("styles") {
        recs.push((
            Id::Style(style_id.to_string()),
            Rec::Style(StyleRec {
                style_id: style_id.to_string(),
                key: str_field(v, "key").unwrap_or_default(),
                name: str_field(v, "name").unwrap_or_default(),
                style_type: str_field(v, "styleType").unwrap_or_default(),
                description: str_field(v, "description").unwrap_or_default(),
                remote: v.get("remote").and_then(Value::as_bool).unwrap_or(false),
            }),
        ));
    }

    Ok(Flattened { recs, file })
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(str::to_string)
}

/// Depth-first walk. `page_id` is the nearest CANVAS ancestor (None above
/// pages — the record then carries the node's own id).
fn walk(document: &Value, out: &mut Vec<(Id, Rec)>) -> Result<(), FlattenError> {
    let mut paths = Vec::new();
    let mut pending = vec![(document, None, None, 0, None)];
    while let Some((node, path, parent_id, child_index, page_id)) = pending.pop() {
        let invalid = |reason| FlattenError::InvalidNode {
            path: node_path(&paths, path),
            reason,
        };
        let object = node
            .as_object()
            .ok_or_else(|| invalid("expected an object"))?;
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid("expected a non-empty string id"))?;
        let node_type = node
            .get("type")
            .and_then(Value::as_str)
            .filter(|node_type| !node_type.is_empty())
            .ok_or_else(|| invalid("expected a non-empty string type"))?
            .to_string();
        if parent_id.is_none() && node_type != "DOCUMENT" {
            return Err(invalid("root type must be DOCUMENT"));
        }
        let own_page = matches!(node_type.as_str(), "DOCUMENT" | "CANVAS");
        let page = if own_page { id } else { page_id.unwrap_or(id) };

        // Serialize a borrowed map: neither cloning nor dropping a raw node
        // should recurse through arbitrary unknown properties (or descendants).
        let raw = NodeFields(object);
        let mut bound_variables = Vec::new();
        for (key, value) in object {
            if key == "boundVariables" {
                collect_aliases(value, "", &mut bound_variables);
            } else if key != "children" {
                let key = escape_json_pointer_component(key);
                scan_bv(value, &format!("/{key}"), &mut bound_variables);
            }
        }
        bound_variables.sort();
        bound_variables.dedup();

        let rec = NodeRec {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            child_index,
            page_id: page.to_string(),
            node_type,
            name: str_field(node, "name").unwrap_or_default(),
            visible: node.get("visible").and_then(Value::as_bool).unwrap_or(true),
            text: str_field(node, "characters"),
            component_id: str_field(node, "componentId"),
            component_properties: sorted_map(node.get("componentProperties"), |v| {
                v.get("value")
                    .map(|val| crate::deep_json::to_string(val).expect("Value serializes"))
            }),
            property_definitions: node
                .get("componentPropertyDefinitions")
                .map(|v| crate::deep_json::to_string(v).expect("Value serializes")),
            style_refs: sorted_map(node.get("styles"), |v| v.as_str().map(str::to_string)),
            bound_variables,
            abs_bounds: node.get("absoluteBoundingBox").and_then(|b| {
                Some([
                    b.get("x")?.as_f64()?,
                    b.get("y")?.as_f64()?,
                    b.get("width")?.as_f64()?,
                    b.get("height")?.as_f64()?,
                ])
            }),
            raw: crate::deep_json::to_string(&raw).expect("serde_json::Value serializes"),
        };
        out.push((Id::Node(id.to_string()), Rec::Node(rec)));

        if let Some(children) = node.get("children") {
            let children = children
                .as_array()
                .ok_or_else(|| invalid("children must be an array when present"))?;
            // Reverse the pending stack to retain document/child-index order.
            for (i, child) in children.iter().enumerate().rev() {
                let child_path = paths.len();
                paths.push(NodePath {
                    parent: path,
                    child_index: i,
                });
                pending.push((child, Some(child_path), Some(id), i as u32, Some(page)));
            }
        }
    }
    Ok(())
}

/// Arena links avoid copying ancestor paths and recursively dropping them.
struct NodePath {
    parent: Option<usize>,
    child_index: usize,
}

fn node_path(paths: &[NodePath], mut path: Option<usize>) -> String {
    let mut indices = Vec::new();
    while let Some(index) = path {
        let segment = &paths[index];
        indices.push(segment.child_index);
        path = segment.parent;
    }
    let mut path = "document".to_string();
    for index in indices.into_iter().rev() {
        write!(path, ".children[{index}]").expect("writing a String cannot fail");
    }
    path
}

/// Turn a JSON object into sorted (key, f(value)) pairs; absent/None entries drop.
fn sorted_map(obj: Option<&Value>, f: impl Fn(&Value) -> Option<String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = obj
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), f(v)?)))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Borrow only a node's non-children fields, preserving sorted map order.
struct NodeFields<'a>(&'a serde_json::Map<String, Value>);

impl Serialize for NodeFields<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in self.0 {
            if key != "children" {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

fn escape_json_pointer_component(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Find binding objects without recursing through unknown node properties.
fn scan_bv(v: &Value, path: &str, out: &mut Vec<(String, String)>) {
    let mut pending = vec![(v, path.to_string())];
    while let Some((v, path)) = pending.pop() {
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    if k == "boundVariables" {
                        collect_aliases(child, &path, out);
                    } else {
                        let key = escape_json_pointer_component(k);
                        pending.push((child, format!("{path}/{key}")));
                    }
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    pending.push((child, format!("{path}/{i}")));
                }
            }
            _ => {}
        }
    }
}

/// Walk the inside of a binding object, including nested objects/arrays.
fn collect_aliases(v: &Value, prop_path: &str, out: &mut Vec<(String, String)>) {
    let mut pending = vec![(v, prop_path.to_string())];
    while let Some((v, path)) = pending.pop() {
        match v {
            Value::Object(map) => {
                let alias = map.get("type").and_then(Value::as_str) == Some("VARIABLE_ALIAS");
                if alias && let Some(id) = map.get("id").and_then(Value::as_str) {
                    out.push((path, id.to_string()));
                    continue;
                }
                for (k, child) in map {
                    let key = escape_json_pointer_component(k);
                    pending.push((child, format!("{path}/{key}")));
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    pending.push((child, format!("{path}/{i}")));
                }
            }
            _ => {}
        }
    }
}
