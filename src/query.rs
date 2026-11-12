//! One source of truth for every read answer — shared by the CLI printers
//! and the MCP tools.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde_json::{Value, json};

use fold::pipeline::terminal::search::Bm25Reader;
use fold::pipeline::terminal::{InvertedIndexReader, MultimapReader, TableReader};
use fold::stream::Readable;

use crate::deep_json::OwnedValue;
use crate::ident::normalize_node_ref;
use crate::model::{
    ComponentRec, ComponentSetRec, FileMeta, NodeRec, StyleRec, VariableCollectionRec, VariableRec,
    VariableSnapshotRec,
};

#[cfg(test)]
mod tests;

/// Read handle for the pipeline's `text` BM25 sink (its tokenizer type
/// param makes the full type unwieldy at every call site).
pub type TextReader<'tx, R> = Bm25Reader<'tx, R, String, fn(&str, &mut Vec<u8>)>;

/// BFS the `children` index from `scope_root` (inclusive), collecting the
/// descendant id set — the shared engine behind `--under` scoping (spec
/// §3). Cycle-safe via the visited set, same discipline as `path`/
/// `depths`'s `parent_id` walks: a corrupted store with a `children`
/// cycle can't loop forever, it just stops re-queuing an id it's already
/// visited. Unknown `scope_root` is the caller's job to reject (via
/// `nodes.get` before calling this) so the error message names the right
/// argument.
fn scope_ids<R: Readable>(
    children: &MultimapReader<'_, R, String, (u32, String)>,
    scope_root: &str,
) -> BTreeSet<String> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    visited.insert(scope_root.to_string());
    queue.push_back(scope_root.to_string());
    while let Some(current) = queue.pop_front() {
        for (_, child_id) in children.get(&current) {
            if visited.insert(child_id.clone()) {
                queue.push_back(child_id);
            }
        }
    }
    visited
}

/// Resolve an optional `--under <id>` scope argument to the descendant id
/// set it names (root inclusive), or `None` when no scope was requested.
/// Unknown id ⇒ the standard "no node … in the mirror" error (spec §3).
fn resolve_under<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    under: Option<String>,
) -> Result<Option<BTreeSet<String>>, String> {
    match under {
        None => Ok(None),
        Some(raw) => {
            let id = normalize_node_ref(&raw);
            nodes
                .get(&id)
                .ok_or_else(|| format!("no node {id} in the mirror"))?;
            Ok(Some(scope_ids(children, &id)))
        }
    }
}

/// One `boundVariables` binding site resolved against the variables table
/// (spec §6): `{pointer, variable_id, variable_name, values_by_mode}` when
/// the variable is known (imported or Enterprise-synced), else
/// `{pointer, variable_id, source: "unresolved"}`. Never errors — an
/// unresolved binding is a normal, expected outcome on the free plan.
fn resolve_variable_binding<R: Readable>(
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    pointer: &str,
    variable_id: &str,
) -> Value {
    match variables.get(&variable_id.to_string()) {
        Some(var) => {
            let collection = variable_collections.get(&var.collection_id);
            let mut values_by_mode = OwnedValue::object();
            for (mode_id, val_str) in &var.values_by_mode {
                let mode_name = collection
                    .as_ref()
                    .and_then(|c| c.modes.iter().find(|(mid, _)| mid == mode_id))
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| mode_id.clone());
                let val = crate::deep_json::from_str(val_str).unwrap_or(OwnedValue(Value::Null));
                values_by_mode.insert(mode_name, val);
            }
            let mut row = OwnedValue(json!({
                "pointer": pointer,
                "variable_id": variable_id,
                "variable_name": var.name,
            }));
            row.insert("values_by_mode".to_string(), values_by_mode);
            row.into_inner()
        }
        None => json!({
            "pointer": pointer,
            "variable_id": variable_id,
            "source": "unresolved",
        }),
    }
}

/// `resolved_variables` array for a node's (or a style's) full set of
/// `boundVariables` binding sites, in the sorted order `bound_variables`
/// already carries (model.rs's determinism contract) — never re-sorted.
fn resolved_variables<R: Readable>(
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    bound_variables: &[(String, String)],
) -> Value {
    let mut arr = OwnedValue::array();
    for (pointer, vid) in bound_variables {
        arr.push(OwnedValue(resolve_variable_binding(
            variables,
            variable_collections,
            pointer,
            vid,
        )));
    }
    arr.into_inner()
}

/// File name, version, last modified, node count, and the variable
/// snapshot summary ([`variable_snapshot`]) under `variables`.
///
/// A store with no `meta` row (never pulled) but some variable state —
/// e.g. only ever `import-variables`'d into — still answers: the file
/// fields are null and `nodes` is the stored count (0), mirroring
/// `figmog serve`'s not-yet-mirrored status shape. Only a store with no
/// meta row *and* snapshot state `none` is the "no mirror here" error.
///
/// `node_count` is the snapshot's node count when the caller already knows
/// it (serve keeps one per published view, NWS-2942); `None` counts the
/// `nodes` table here, decoding every row — fine for a one-shot CLI read,
/// too slow for a status poll.
pub fn status<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    meta: &TableReader<'_, R, u8, FileMeta>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    snapshot: &TableReader<'_, R, u8, VariableSnapshotRec>,
    node_count: Option<usize>,
) -> Result<Value, String> {
    let vars = variable_snapshot(variables, variable_collections, snapshot);
    let m = meta.get(&0);
    if m.is_none() && vars["state"] == "none" {
        return Err("no mirror here — run `figmog pull <file-url>` first".to_string());
    }
    let count = node_count.unwrap_or_else(|| nodes.iter().count());
    Ok(json!({
        "name": m.as_ref().map(|m| m.name.clone()),
        "version": m.as_ref().map(|m| m.version.clone()),
        "last_modified": m.as_ref().map(|m| m.last_modified.clone()),
        "synced_at_unix_ms": m.as_ref().map(|m| m.synced_at_unix_ms),
        "nodes": count,
        "variables": vars,
    }))
}

/// The variable snapshot's state and provenance (NWS-2961). `state` is:
/// - `"none"`: no snapshot row and no variables/collections stored;
/// - `"legacy"`: variables or collections stored without a matching snapshot
///   row (including older-binary writes) — provenance fields are null;
/// - `"empty"`: a snapshot row whose export held no variables;
/// - `"current"`: a snapshot row with at least one variable.
///
/// A rejected (invalid) import never reaches the store, so it has no
/// state of its own: the previous snapshot's state stands.
pub fn variable_snapshot<R: Readable>(
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    snapshot: &TableReader<'_, R, u8, VariableSnapshotRec>,
) -> Value {
    let var_count = variables.iter().count();
    let collection_count = variable_collections.iter().count();
    let snap = matching_variable_snapshot(variables, snapshot);
    let state = match (&snap, var_count, collection_count) {
        (Some(_), 0, _) => "empty",
        (Some(_), _, _) => "current",
        (None, 0, 0) => "none",
        (None, _, _) => "legacy",
    };
    json!({
        "state": state,
        "source": snap.as_ref().map(|s| s.source.clone()),
        "format": snap.as_ref().map(|s| s.format.clone()),
        "format_version": snap.as_ref().and_then(|s| s.format_version.clone()),
        "exported_at": snap.as_ref().and_then(|s| s.exported_at.clone()),
        "digest": snap.as_ref().map(|s| s.digest.clone()),
        "variables": var_count,
        "collections": collection_count,
    })
}

/// Older binaries do not maintain provenance. An id mismatch proves the row
/// no longer describes these variables; same-id edits cannot be detected.
fn matching_variable_snapshot<R: Readable>(
    variables: &TableReader<'_, R, String, VariableRec>,
    snapshot: &TableReader<'_, R, u8, VariableSnapshotRec>,
) -> Option<VariableSnapshotRec> {
    snapshot.get(&0).filter(|snap| {
        // Table iteration follows postcard-encoded keys, which sort strings
        // by their encoded length before their contents. Compare decoded ids.
        let stored_ids: BTreeSet<_> = variables.iter().map(|(id, _)| id).collect();
        snap.variables
            .iter()
            .map(|code| &code.id)
            .eq(stored_ids.iter())
    })
}

/// List pages, ordered by document child index.
pub fn pages<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<Value, String> {
    let mut ids = by_type.search(&"CANVAS".to_string());
    ids.sort();

    let mut rows: Vec<(u32, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id).map(|n| (n.child_index, n.id, n.name)))
        .collect();
    rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    let arr: Vec<Value> = rows
        .iter()
        .map(|(_, id, name)| json!({"id": id, "name": name}))
        .collect();
    Ok(Value::Array(arr))
}

/// One level of a `tree` outline; JSON shape `{id, name, type, children}`.
pub struct TreeNode {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub children: Vec<TreeNode>,
}

pub fn build_tree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    node: &NodeRec,
    depth: Option<usize>,
) -> TreeNode {
    let make = |n: &NodeRec| TreeNode {
        id: n.id.clone(),
        name: n.name.clone(),
        node_type: n.node_type.clone(),
        children: Vec::new(),
    };
    let edges = |id: &String, depth| {
        let mut edges = if depth == Some(0) {
            Vec::new()
        } else {
            children.get(id)
        };
        edges.sort();
        edges.into_iter()
    };
    let mut stack = vec![(make(node), depth, edges(&node.id, depth))];
    let mut ancestors = BTreeSet::from([node.id.clone()]);
    loop {
        let (_, depth, edges_iter) = stack.last_mut().expect("root frame");
        if let Some((_, child_id)) = edges_iter.next() {
            if let Some(child) = nodes.get(&child_id)
                && ancestors.insert(child.id.clone())
            {
                let depth = depth.map(|d| d - 1);
                stack.push((make(&child), depth, edges(&child.id, depth)));
            }
        } else {
            let (finished, _, _) = stack.pop().expect("root frame");
            ancestors.remove(&finished.id);
            match stack.last_mut() {
                Some((parent, _, _)) => parent.children.push(finished),
                None => return finished,
            }
        }
    }
}

impl Drop for TreeNode {
    fn drop(&mut self) {
        let mut pending = std::mem::take(&mut self.children);
        while let Some(mut child) = pending.pop() {
            pending.append(&mut child.children);
        }
    }
}

fn tree_to_json(t: &TreeNode) -> Value {
    let mut pending = vec![(t, false)];
    let mut values = Vec::new();
    while let Some((node, visited)) = pending.pop() {
        if visited {
            let children = values.split_off(values.len() - node.children.len());
            // Move the child array. json! on a Value would clone it recursively.
            let mut value = json!({"id": node.id, "name": node.name, "type": node.node_type});
            value["children"] = Value::Array(children);
            values.push(value);
        } else {
            pending.push((node, true));
            pending.extend(node.children.iter().rev().map(|child| (child, false)));
        }
    }
    values.pop().expect("root value")
}

/// Resolve the root (default: the DOCUMENT node) and build its outline as a
/// [`TreeNode`], so callers that render for humans can walk the same
/// structure `tree`'s JSON is built from.
pub fn tree_nodes<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    id: Option<String>,
    depth: Option<usize>,
) -> Result<TreeNode, String> {
    let start = match id {
        Some(raw) => normalize_node_ref(&raw),
        None => {
            let mut docs = by_type.search(&"DOCUMENT".to_string());
            docs.sort();
            docs.into_iter()
                .next()
                .ok_or_else(|| "no DOCUMENT node in the mirror".to_string())?
        }
    };
    let root = nodes
        .get(&start)
        .ok_or_else(|| format!("no node {start} in the mirror"))?;
    Ok(build_tree(nodes, children, &root, depth))
}

/// Subtree outline (default: whole document).
pub fn tree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    id: Option<String>,
    depth: Option<usize>,
) -> Result<Value, String> {
    let t = tree_nodes(nodes, children, by_type, id, depth)?;
    Ok(tree_to_json(&t))
}

/// Full raw JSON of one node, optionally with a `children` summary array
/// and/or a `resolved_variables` annotation (spec §6) of every
/// `boundVariables` binding site on this node.
pub fn node<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: String,
    with_children: bool,
    resolve_vars: bool,
) -> Result<Value, String> {
    let id = normalize_node_ref(&id);
    let n = nodes
        .get(&id)
        .ok_or_else(|| format!("no node {id} in the mirror"))?;
    let mut value = crate::deep_json::from_str(&n.raw).map_err(|e| e.to_string())?;

    if with_children {
        let mut edges = children.get(&id);
        edges.sort();
        let kids: Vec<Value> = edges
            .into_iter()
            .filter_map(|(_, child_id)| {
                nodes
                    .get(&child_id)
                    .map(|n| json!({"id": n.id, "name": n.name, "type": n.node_type}))
            })
            .collect();
        if value.is_object() {
            value.insert("children".to_string(), OwnedValue(Value::Array(kids)));
        }
    }

    if resolve_vars && value.is_object() {
        value.insert(
            "resolved_variables".to_string(),
            OwnedValue(resolved_variables(
                variables,
                variable_collections,
                &n.bound_variables,
            )),
        );
    }

    Ok(value.into_inner())
}

/// Top-level raw fields a `fields` projection keeps regardless of whether
/// they were named (spec §2): `id`/`name`/`type` because a projected node
/// must stay identifiable. `children` isn't listed here — `raw` never
/// carries a `children` field (flatten strips it), and [`build_subtree`]
/// always adds it back after projection, so it survives unconditionally
/// without needing a name-check.
const SUBTREE_ALWAYS_FIELDS: [&str; 3] = ["id", "name", "type"];

/// Project a subtree node's raw JSON object down to `fields` plus the
/// always-kept set. Unknown field names are simply absent, not an error —
/// spec §2's "agents probe freely" contract. A non-object `value` (should
/// never happen for a real Figma node) passes through unchanged.
fn project_raw_fields(mut value: OwnedValue, fields: &[String]) -> OwnedValue {
    if let Some(map) = value.0.as_object_mut() {
        let discarded: Vec<_> = map
            .keys()
            .filter(|k| {
                !SUBTREE_ALWAYS_FIELDS.contains(&k.as_str()) && !fields.iter().any(|f| f == *k)
            })
            .cloned()
            .collect();
        for key in discarded {
            if let Some(value) = map.remove(&key) {
                drop(crate::deep_json::OwnedValue(value));
            }
        }
    }
    value
}

/// Iterative worker behind [`subtree`]: raw JSON of `n`, fields-projected,
/// with `children` nested in child-index order and (when requested)
/// `resolved_variables` — both inserted *after* projection so they always
/// survive a `fields` filter, matching `id`/`name`/`type`.
#[allow(clippy::too_many_arguments)]
fn build_subtree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    n: &NodeRec,
    depth: Option<usize>,
    fields: Option<&[String]>,
    resolve_vars: bool,
) -> Result<Value, String> {
    let prepare = |n: &NodeRec, depth| -> Result<_, String> {
        let mut value = crate::deep_json::from_str(&n.raw).map_err(|e| e.to_string())?;
        if let Some(fs) = fields {
            value = project_raw_fields(value, fs);
        }
        if resolve_vars && value.is_object() {
            value.insert(
                "resolved_variables".to_string(),
                OwnedValue(resolved_variables(
                    variables,
                    variable_collections,
                    &n.bound_variables,
                )),
            );
        }
        let mut edges = if depth == Some(0) {
            Vec::new()
        } else {
            children.get(&n.id)
        };
        edges.sort();
        Ok((
            n.id.clone(),
            value,
            depth,
            edges.into_iter(),
            OwnedValue::array(),
        ))
    };
    let mut stack = vec![prepare(n, depth)?];
    let mut ancestors = BTreeSet::from([n.id.clone()]);
    loop {
        let (_, _, depth, edges, _) = stack.last_mut().expect("root frame");
        if let Some((_, child_id)) = edges.next() {
            if let Some(child) = nodes.get(&child_id)
                && ancestors.insert(child.id.clone())
            {
                let depth = depth.map(|d| d - 1);
                stack.push(prepare(&child, depth)?);
            }
        } else {
            let (id, mut value, _, _, kids) = stack.pop().expect("root frame");
            ancestors.remove(&id);
            if value.is_object() {
                value.insert("children".to_string(), kids);
            }
            match stack.last_mut() {
                Some((_, _, _, _, kids)) => kids.push(value),
                None => return Ok(value.into_inner()),
            }
        }
    }
}

/// Subtree dump rooted at `id` (spec §2): the node's full raw JSON with
/// `children: [...]` nested recursively in child-index order, to `depth`
/// levels (default: unlimited). `fields` projects every node to the named
/// raw fields (`id`/`name`/`type`/`children` always survive); `None` skips
/// projection entirely. `resolve_vars` adds a `resolved_variables`
/// annotation (spec §6) to every node.
#[allow(clippy::too_many_arguments)]
pub fn subtree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: String,
    depth: Option<usize>,
    fields: Option<&[String]>,
    resolve_vars: bool,
) -> Result<Value, String> {
    let id = normalize_node_ref(&id);
    let root = nodes
        .get(&id)
        .ok_or_else(|| format!("no node {id} in the mirror"))?;
    build_subtree(
        nodes,
        children,
        variables,
        variable_collections,
        &root,
        depth,
        fields,
        resolve_vars,
    )
}

/// Binding-aware spec of the subtree rooted at `id` (NWS-2968): the
/// [`subtree`] answer and the [`vars`] `{snapshot, variables}` answer,
/// rendered by [`crate::spec::build`] with the project's token `tokens`
/// options (NWS-2948), Figma → code component `map` (NWS-2949) and
/// asset manifest `assets` (NWS-2952).
#[allow(clippy::too_many_arguments)]
pub fn spec<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    snapshot: &TableReader<'_, R, u8, VariableSnapshotRec>,
    id: String,
    tokens: &crate::tokens::ExportOptions,
    tailwind: bool,
    map: Option<&crate::map::CodeMap>,
    assets: Option<&crate::assets::AssetIndex>,
) -> Result<Value, String> {
    let tree = OwnedValue(subtree(
        nodes,
        children,
        variables,
        variable_collections,
        id,
        None,
        None,
        false,
    )?);
    let vars = OwnedValue(vars(
        nodes,
        variables,
        variable_collections,
        snapshot,
        None,
        true,
    )?);
    // The component records the subtree's instances name (NWS-2947), so
    // the spec stays a pure function of its inputs.
    let mut records = serde_json::Map::new();
    for cid in crate::spec::component_ids(&tree) {
        let Some(c) = components.get(&cid) else {
            continue;
        };
        let set = c.component_set_id.as_ref().map(|sid| {
            let s = component_sets.get(sid);
            json!({
                "id": sid,
                "name": s.as_ref().map(|s| s.name.clone()),
                "key": s.as_ref().map(|s| s.key.clone()),
            })
        });
        let mut record =
            json!({"name": c.name, "key": c.key, "remote": c.remote, "component_set": set});
        // With a map (NWS-2949): the main component's nested instances,
        // node id → component id, so a hand-swapped nested instance shows;
        // null when the main component isn't in this mirror.
        if map.is_some() {
            record["main"] = match nodes.get(&cid) {
                None => Value::Null,
                Some(_) => scope_ids(children, &cid)
                    .into_iter()
                    .filter(|id| *id != cid)
                    .filter_map(|id| {
                        let n = nodes.get(&id)?;
                        (n.node_type == "INSTANCE").then_some((id, json!(n.component_id?)))
                    })
                    .collect::<serde_json::Map<_, _>>()
                    .into(),
            };
        }
        records.insert(cid, record);
    }
    Ok(crate::spec::build(
        &tree,
        &vars,
        &Value::Object(records),
        tokens,
        tailwind,
        map,
        assets,
    ))
}

/// Nodes by type, optionally within one page and/or scoped to `under`'s
/// subtree (spec §3: intersects with the page filter). `page` and `under`
/// both accept a full Figma URL (spec §2b), same as any other node-id
/// argument.
pub fn find<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    node_type: String,
    page: Option<String>,
    under: Option<String>,
) -> Result<Value, String> {
    // Figma node types are stored uppercase; normalize so `--type frame`
    // matches the same as `--type FRAME`.
    let mut ids = by_type.search(&node_type.to_uppercase());
    ids.sort();
    let page = page.as_deref().map(normalize_node_ref);
    let scope = resolve_under(nodes, children, under)?;

    let mut rows: Vec<(String, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id))
        .filter(|n| page.as_deref().is_none_or(|p| n.page_id == p))
        .filter(|n| scope.as_ref().is_none_or(|s| s.contains(&n.id)))
        .map(|n| (n.id, n.name, n.page_id))
        .collect();
    rows.sort();

    let arr: Vec<Value> = rows
        .iter()
        .map(|(id, name, page_id)| json!({"id": id, "name": name, "page_id": page_id}))
        .collect();
    Ok(Value::Array(arr))
}

/// BM25 search over layer names and text content, optionally scoped to
/// `under`'s subtree (spec §3). Order all matches by score descending,
/// then node ID ascending, before limiting results. Scoped calls filter
/// before truncating so out-of-scope hits can't crowd out in-scope ones.
pub fn search<R: Readable>(
    text: &TextReader<'_, R>,
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    query: &str,
    limit: usize,
    under: Option<String>,
) -> Result<Value, String> {
    let scope = resolve_under(nodes, children, under)?;
    // Upstream score ties depend on reused thread-local scratch capacity.
    // Fetch every match so ties crossing the cutoff are resolved here.
    let mut hits = text.search(query, usize::MAX);
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.val.cmp(&b.val)));
    let rows: Vec<Value> = hits
        .iter()
        .filter(|hit| scope.as_ref().is_none_or(|s| s.contains(&hit.val)))
        .take(limit)
        .filter_map(|hit| {
            let node = nodes.get(&hit.val)?;
            let snippet = node
                .text
                .as_ref()
                .map(|t| t.chars().take(80).collect::<String>());
            Some(json!({
                "id": node.id,
                "score": hit.score,
                "type": node.node_type,
                "name": node.name,
                "page_id": node.page_id,
                "snippet": snippet,
            }))
        })
        .collect();
    Ok(Value::Array(rows))
}

/// Resolve a target (node id, component key, or component/set name) to the
/// component node ids it names, in priority order: exact node id, then key,
/// then set name (all variants), then component name (all matches).
fn resolve_component_ids<R: Readable>(
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    target: &str,
) -> Vec<String> {
    if components.contains(&target.to_string()) {
        return vec![target.to_string()];
    }

    let mut ids: Vec<String> = components
        .iter()
        .filter(|(_, c)| c.key == target)
        .map(|(id, _)| id)
        .collect();
    if !ids.is_empty() {
        return ids;
    }

    let set_ids: Vec<String> = component_sets
        .iter()
        .filter(|(_, s)| s.name == target)
        .map(|(id, _)| id)
        .collect();
    if !set_ids.is_empty() {
        ids = components
            .iter()
            .filter(|(_, c)| {
                c.component_set_id
                    .as_deref()
                    .is_some_and(|s| set_ids.iter().any(|sid| sid == s))
            })
            .map(|(id, _)| id)
            .collect();
        return ids;
    }

    components
        .iter()
        .filter(|(_, c)| c.name == target)
        .map(|(id, _)| id)
        .collect()
}

/// Instances of a component (by node id, key, or name).
pub fn instances<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    instances_of: &InvertedIndexReader<'_, R, String, String>,
    target: &str,
) -> Result<Value, String> {
    let target = normalize_node_ref(target);
    let component_ids = resolve_component_ids(components, component_sets, &target);

    let mut instance_ids: BTreeSet<String> = BTreeSet::new();
    for cid in &component_ids {
        instance_ids.extend(instances_of.search(cid));
    }

    let rows: Vec<Value> = instance_ids
        .iter()
        .filter_map(|id| nodes.get(id))
        .map(|n| json!({"id": n.id, "name": n.name, "page_id": n.page_id, "component_id": n.component_id}))
        .collect();
    Ok(Value::Array(rows))
}

/// Design-system inventory: sets, variant axes, standalone components.
pub fn components<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
) -> Result<Value, String> {
    let mut sets: Vec<(String, ComponentSetRec)> = component_sets.iter().collect();
    sets.sort_by(|a, b| a.0.cmp(&b.0));

    let mut all_components: Vec<(String, ComponentRec)> = components.iter().collect();
    all_components.sort_by(|a, b| a.0.cmp(&b.0));

    // Group variants by set once (NWS-2942: was a scan of every component
    // per set). Pushed in component-id order, so each group keeps it.
    let mut variants_by_set: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for (cid, c) in &all_components {
        if let Some(set_id) = c.component_set_id.as_deref() {
            variants_by_set
                .entry(set_id)
                .or_default()
                .push(json!({"node_id": cid, "name": c.name, "key": c.key}));
        }
    }

    let mut sets_json = OwnedValue::array();
    for (set_id, set) in &sets {
        let variants = variants_by_set.remove(set_id.as_str()).unwrap_or_default();
        let property_definitions = nodes
            .get(set_id)
            .and_then(|n| n.property_definitions)
            .and_then(|s| crate::deep_json::from_str(&s).ok())
            .unwrap_or(OwnedValue(Value::Null));
        let mut row = OwnedValue(
            json!({"node_id": set_id, "name": set.name, "key": set.key, "variants": variants}),
        );
        row.insert("property_definitions".to_string(), property_definitions);
        sets_json.push(row);
    }

    let standalone: Vec<Value> = all_components
        .iter()
        .filter(|(_, c)| c.component_set_id.is_none())
        .map(|(cid, c)| json!({"node_id": cid, "name": c.name, "key": c.key}))
        .collect();
    let mut inventory = OwnedValue::object();
    inventory.insert("sets".to_string(), sets_json);
    inventory.insert(
        "components".to_string(),
        OwnedValue(Value::Array(standalone)),
    );
    Ok(inventory.into_inner())
}

/// Styles with usage counts; `values` derives definitions from consumers.
/// `resolve_vars` (with `values`) adds a `resolved_variables` annotation
/// (spec §6) of the variable bindings under that definition's own raw-JSON
/// pointer prefix (e.g. a FILL style's bindings live under `/fills`) — a
/// no-op without `values`, since there's no definition to annotate.
#[allow(clippy::too_many_arguments)]
pub fn styles<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    style_type: Option<String>,
    values: bool,
    resolve_vars: bool,
) -> Result<Value, String> {
    let mut rows: Vec<(String, StyleRec)> = styles.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(t) = &style_type {
        rows.retain(|(_, s)| s.style_type.eq_ignore_ascii_case(t));
    }

    let mut out = OwnedValue::array();
    for (style_id, s) in &rows {
        let mut consumers = styled_by.search(style_id);
        consumers.sort();
        let mut obj = OwnedValue(json!({
            "style_id": style_id,
            "name": s.name,
            "key": s.key,
            "type": s.style_type,
            "uses": consumers.len(),
        }));
        if values {
            let consumer = consumers.first().and_then(|nid| nodes.get(nid));
            let value = consumer
                .as_ref()
                .and_then(|n| crate::vars::style_value_from_consumer(&s.style_type, &n.raw))
                .unwrap_or(Value::Null);
            obj.insert("value".to_string(), OwnedValue(value));

            if resolve_vars {
                let prefix = crate::vars::style_value_pointer(&s.style_type);
                let bound: Vec<(String, String)> = consumer
                    .map(|n| {
                        n.bound_variables
                            .iter()
                            .filter(|(p, _)| prefix.is_some_and(|pre| p.starts_with(pre)))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                obj.insert(
                    "resolved_variables".to_string(),
                    OwnedValue(resolved_variables(variables, variable_collections, &bound)),
                );
            }
        }
        out.push(obj);
    }
    Ok(out.into_inner())
}

/// Nodes using a style id or bound to a variable id.
pub fn uses<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    bound_to: &InvertedIndexReader<'_, R, String, String>,
    id: &str,
) -> Result<Value, String> {
    let id = id.to_string();
    let mut ids = styled_by.search(&id);
    if ids.is_empty() {
        ids = bound_to.search(&id);
    }
    ids.sort();

    let rows: Vec<Value> = ids
        .iter()
        .filter_map(|nid| nodes.get(nid))
        .map(|n| json!({"id": n.id, "name": n.name, "page_id": n.page_id}))
        .collect();
    Ok(Value::Array(rows))
}

/// Variables: authoritative if imported, else inferred from bindings.
/// Authoritative rows also carry `code_syntax`, `hidden_from_publishing`
/// and `remote` from the snapshot row (null for a legacy snapshot). With
/// `with_snapshot`, the result is `{snapshot, variables}` — the
/// [`variable_snapshot`] summary next to the usual row array.
pub fn vars<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    snapshot: &TableReader<'_, R, u8, VariableSnapshotRec>,
    id_filter: Option<String>,
    with_snapshot: bool,
) -> Result<Value, String> {
    let snap = matching_variable_snapshot(variables, snapshot);
    let code_of = |vid: &str| {
        snap.as_ref().and_then(|s| {
            s.variables
                .binary_search_by(|c| c.id.as_str().cmp(vid))
                .ok()
                .map(|i| &s.variables[i])
        })
    };
    // Stream the table rather than holding every node: only the bound ones
    // are kept, and `infer_from_nodes` never parses an unbound node's raw
    // JSON (NWS-2942).
    let bound_nodes: Vec<NodeRec> = nodes
        .iter()
        .map(|(_, n)| n)
        .filter(|n| !n.bound_variables.is_empty())
        .collect();
    let inferred = crate::vars::infer_from_nodes(bound_nodes.iter());
    let mut inferred_by_id: HashMap<String, crate::vars::VarUsage> = inferred
        .into_iter()
        .map(|u| (u.variable_id.clone(), u))
        .collect();

    let mut all_ids: BTreeSet<String> = inferred_by_id.keys().cloned().collect();
    all_ids.extend(variables.iter().map(|(k, _)| k));
    if let Some(target) = &id_filter {
        all_ids.retain(|v| v == target);
    }

    let mut rows = OwnedValue::array();
    for vid in &all_ids {
        let usage = inferred_by_id.remove(vid);
        let (sites, observed) = usage.map(|u| (u.sites, u.observed)).unwrap_or_default();

        let row = if let Some(var) = variables.get(vid) {
            let collection = variable_collections.get(&var.collection_id);
            let mode_names: BTreeMap<String, String> = collection
                .as_ref()
                .map(|c| {
                    c.modes
                        .iter()
                        .map(|(id, name)| {
                            (
                                id.clone(),
                                if name.is_empty() {
                                    id.clone()
                                } else {
                                    name.clone()
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            // The presentation map keeps its historical names only when
            // every key is unique, including modes absent from this variable.
            // Otherwise use ids for the entire collection so no value is lost.
            let mut labels: BTreeSet<&str> = BTreeSet::new();
            let unique_names = mode_names.values().all(|name| labels.insert(name));
            let mut values_by_mode = OwnedValue::object();
            let mut values_by_mode_id = OwnedValue::object();
            for (mode_id, val_str) in &var.values_by_mode {
                let mode_name = if unique_names {
                    mode_names.get(mode_id).unwrap_or(mode_id).clone()
                } else {
                    mode_id.clone()
                };
                // Parse each owned value separately: values may be too deep
                // for a recursive serde_json::Value clone.
                values_by_mode.insert(
                    mode_name,
                    crate::deep_json::from_str(val_str).unwrap_or(OwnedValue(Value::Null)),
                );
                values_by_mode_id.insert(
                    mode_id.clone(),
                    crate::deep_json::from_str(val_str).unwrap_or(OwnedValue(Value::Null)),
                );
            }
            let code = code_of(vid);
            let mut scopes = var.scopes.clone();
            scopes.sort();
            scopes.dedup();
            let mut row = OwnedValue(json!({
                "variable_id": vid,
                "source": "imported",
                "code_syntax": code.map(|c| {
                    c.code_syntax.iter().map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect::<serde_json::Map<_, _>>()
                }),
                "hidden_from_publishing": code.map(|c| c.hidden_from_publishing),
                "remote": code.map(|c| c.remote),
                "name": var.name,
                "resolved_type": var.resolved_type,
                "collection": collection.as_ref().map(|c| c.name.clone()),
                "collection_id": var.collection_id,
                // Names are presentation only; exporters select by id.
                "default_mode": collection.as_ref().map(|c| {
                    mode_names.get(&c.default_mode_id).unwrap_or(&c.default_mode_id).clone()
                }),
                "default_mode_id": collection.as_ref().map(|c| c.default_mode_id.clone()),
                "mode_names": mode_names,
                "scopes": scopes,
                "sites": sites,
                "observed": observed,
            }));
            row.insert("values_by_mode".to_string(), values_by_mode);
            row.insert("values_by_mode_id".to_string(), values_by_mode_id);
            row
        } else {
            OwnedValue(json!({
                "variable_id": vid,
                "source": "inferred",
                "sites": sites,
                "observed": observed,
            }))
        };
        rows.push(row);
    }
    if with_snapshot {
        let mut result = OwnedValue(json!({
            "snapshot": variable_snapshot(variables, variable_collections, snapshot),
        }));
        result.insert("variables".to_string(), rows);
        return Ok(result.into_inner());
    }
    Ok(rows.into_inner())
}

// ---- whole-file structural queries ----
//
// The local mirror's unfair advantage: full-file scans/joins no
// rate-limited API surface could offer, all answered from the local store.

/// Every node's depth (root = 0) along its `parent_id` chain, keyed by
/// node id, in one pass over `parents` (id → parent id, NWS-2942: was a
/// store walk per node, O(nodes × depth)). Iterative, so documents of any
/// depth are fine; each id's depth is memoized and reused by its
/// descendants.
///
/// Matches a per-node walk exactly, corrupted stores included: a parent
/// missing from the table still counts as one level; a `parent_id` cycle
/// stops at the first repeated id, so each cycle member's depth is the
/// cycle's length and a node leading into a cycle adds its distance to it.
/// A cycle must not hang: this runs inside `figmog serve` (on a read
/// worker), where a hang would pin a worker and its leased view forever.
fn depths<'a>(parents: &HashMap<&'a str, Option<&'a str>>) -> HashMap<&'a str, usize> {
    let mut depth: HashMap<&'a str, usize> = HashMap::with_capacity(parents.len());
    let mut path: Vec<&'a str> = Vec::new();
    let mut on_path: HashMap<&'a str, usize> = HashMap::new();
    for &start in parents.keys() {
        if depth.contains_key(start) {
            continue;
        }
        path.clear();
        on_path.clear();
        let mut current = start;
        // Depth of the node just above `path`'s last entry, and the index
        // `path[cycle..]` of a cycle when one closed the walk.
        let (above, cycle): (Option<usize>, Option<usize>) = loop {
            if let Some(&d) = depth.get(current) {
                break (Some(d), None);
            }
            let Some(&parent) = parents.get(current) else {
                // A missing parent: the edge to it still counts one level.
                break (Some(0), None);
            };
            if let Some(&i) = on_path.get(current) {
                break (None, Some(i));
            }
            on_path.insert(current, path.len());
            path.push(current);
            match parent {
                Some(parent) => current = parent,
                None => break (None, None),
            }
        };
        let len = path.len();
        match (above, cycle) {
            (_, Some(i)) => {
                let cycle_len = len - i;
                for (j, id) in path.iter().enumerate() {
                    depth.insert(id, cycle_len + i.saturating_sub(j));
                }
            }
            (Some(d), None) => {
                for (j, id) in path.iter().enumerate() {
                    depth.insert(id, d + (len - j));
                }
            }
            // Ended at a root: the last entry is depth 0.
            (None, None) => {
                for (j, id) in path.iter().enumerate() {
                    depth.insert(id, len - 1 - j);
                }
            }
        }
    }
    depth
}

/// Node counts by type and by page, table totals, text-node count, max tree
/// depth.
#[allow(clippy::too_many_arguments)]
pub fn stats<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<Value, String> {
    let all: Vec<NodeRec> = nodes.iter().map(|(_, n)| n).collect();

    let mut by_type_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_page_counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in &all {
        *by_type_counts.entry(n.node_type.clone()).or_insert(0) += 1;
        // DOCUMENT/CANVAS nodes are pages/roots, not page contents.
        if n.node_type != "DOCUMENT" && n.node_type != "CANVAS" {
            *by_page_counts.entry(n.page_id.clone()).or_insert(0) += 1;
        }
    }
    let parents: HashMap<&str, Option<&str>> = all
        .iter()
        .map(|n| (n.id.as_str(), n.parent_id.as_deref()))
        .collect();
    // Only the maximum leaves this function, never the map's order.
    let max_depth = depths(&parents).into_values().max().unwrap_or(0);

    let text_nodes = by_type.search(&"TEXT".to_string()).len();

    Ok(json!({
        "by_type": by_type_counts,
        "by_page": by_page_counts,
        "totals": {
            "components": components.iter().count(),
            "component_sets": component_sets.iter().count(),
            "styles": styles.iter().count(),
            "variables": variables.iter().count(),
        },
        "text_nodes": text_nodes,
        "max_depth": max_depth,
    }))
}

/// Ancestor chain root→node, as `[{id, name, type}]`. Unknown id → Err.
/// A `parent_id` cycle (a corrupted store) is also an `Err` rather than an
/// infinite loop — see `depths`'s doc comment for why that matters here.
pub fn path<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    id: String,
) -> Result<Value, String> {
    let id = normalize_node_ref(&id);
    let mut chain: Vec<NodeRec> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut current = id.clone();
    loop {
        if !visited.insert(current.clone()) {
            return Err(format!("parent cycle detected at {current}"));
        }
        let n = nodes
            .get(&current)
            .ok_or_else(|| format!("no node {current} in the mirror"))?;
        let parent = n.parent_id.clone();
        chain.push(n);
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }
    chain.reverse();

    let arr: Vec<Value> = chain
        .iter()
        .map(|n| json!({"id": n.id, "name": n.name, "type": n.node_type}))
        .collect();
    Ok(Value::Array(arr))
}

/// Every TEXT node's `(id, characters, page_id)`, optionally scoped to one
/// page and/or `under`'s subtree (spec §3: intersects with the page
/// filter), sorted by id. `page` and `under` both accept a full Figma URL
/// (spec §2b), same as any other node-id argument.
pub fn text<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    page: Option<String>,
    under: Option<String>,
) -> Result<Value, String> {
    let mut ids = by_type.search(&"TEXT".to_string());
    ids.sort();
    let page = page.as_deref().map(normalize_node_ref);
    let scope = resolve_under(nodes, children, under)?;

    let mut rows: Vec<(String, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id))
        .filter(|n| page.as_deref().is_none_or(|p| n.page_id == p))
        .filter(|n| scope.as_ref().is_none_or(|s| s.contains(&n.id)))
        .map(|n| (n.id, n.text.clone().unwrap_or_default(), n.page_id))
        .collect();
    rows.sort();

    let arr: Vec<Value> = rows
        .iter()
        .map(|(id, characters, page_id)| {
            json!({"id": id, "characters": characters, "page_id": page_id})
        })
        .collect();
    Ok(Value::Array(arr))
}

/// Nodes whose `raw` JSON matches an RFC 6901 pointer, optionally by value
/// and/or scoped to one page and/or `under`'s subtree (spec §3: intersects
/// with the page filter). Rows `[{id, name, type, page_id, value}]`,
/// sorted by id. `page` and `under` both accept a full Figma URL (spec
/// §2b), same as any other node-id argument.
pub fn where_<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    pointer: &str,
    equals: Option<&Value>,
    page: Option<String>,
    under: Option<String>,
) -> Result<Value, String> {
    if !pointer.starts_with('/') {
        return Err(format!(
            "pointer must be an RFC 6901 pointer starting with '/': {pointer}"
        ));
    }
    let page = page.as_deref().map(normalize_node_ref);
    let scope = resolve_under(nodes, children, under)?;

    // Keep completed matches guarded if a later stored record fails to parse.
    let mut rows: Vec<(String, OwnedValue)> = Vec::new();
    for (_, n) in nodes.iter() {
        if page.as_deref().is_some_and(|p| n.page_id != p) {
            continue;
        }
        if !scope.as_ref().is_none_or(|s| s.contains(&n.id)) {
            continue;
        }
        let mut raw = crate::deep_json::from_str(&n.raw).map_err(|e| e.to_string())?;
        let Some(value) = raw.0.pointer_mut(pointer) else {
            continue;
        };
        if equals.is_some_and(|want| !json_values_equal(value, want)) {
            continue;
        }
        let value = OwnedValue(std::mem::take(value));
        let mut row = OwnedValue(
            json!({"id": n.id, "name": n.name, "type": n.node_type, "page_id": n.page_id}),
        );
        row.insert("value".to_string(), value);
        rows.push((n.id, row));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut result = OwnedValue::array();
    for (_, row) in rows {
        result.push(row);
    }
    Ok(result.into_inner())
}

/// Value's scalar equality semantics, without recursive container comparisons.
fn json_values_equal(a: &Value, b: &Value) -> bool {
    let mut pending = vec![(a, b)];
    while let Some((a, b)) = pending.pop() {
        match (a, b) {
            (Value::Array(a), Value::Array(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                pending.extend(a.iter().zip(b));
            }
            (Value::Object(a), Value::Object(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for ((a_key, a_value), (b_key, b_value)) in a.iter().zip(b) {
                    if a_key != b_key {
                        return false;
                    }
                    pending.push((a_value, b_value));
                }
            }
            _ => {
                if a != b {
                    return false;
                }
            }
        }
    }
    true
}

/// Default output cap for point lookups, shared by the CLI and MCP.
pub const DEFAULT_AT_LIMIT: usize = 100;

/// Visible nodes on `page` whose `abs_bounds` contain `(x, y)`, optionally
/// intersected with `under`'s subtree. Hidden branches are pruned from the
/// page walk. Matches sort by area ascending then id before taking `limit`.
/// Rows `[{id, name, type, page_id, area}]`.
pub fn at<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    x: f64,
    y: f64,
    page: String,
    under: Option<String>,
    limit: usize,
) -> Result<Value, String> {
    let page = normalize_node_ref(&page);
    let page_node = nodes
        .get(&page)
        .ok_or_else(|| format!("no node {page} in the mirror"))?;
    if page_node.node_type != "CANVAS" {
        return Err(format!("node {page} is not a page (CANVAS)"));
    }
    let scope = resolve_under(nodes, children, under)?;
    let mut rows: Vec<(f64, String, String, String, String)> = Vec::new();
    let mut pending = vec![page.clone()];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        let Some(n) = nodes.get(&id) else {
            continue;
        };
        if n.page_id != page || !n.visible {
            continue;
        }
        pending.extend(children.get(&id).into_iter().map(|(_, child_id)| child_id));
        if scope.as_ref().is_some_and(|ids| !ids.contains(&id)) {
            continue;
        }
        let Some([bx, by, w, h]) = n.abs_bounds else {
            continue;
        };
        if bx <= x && x < bx + w && by <= y && y < by + h {
            rows.push((w * h, n.id, n.name, n.node_type, n.page_id));
        }
    }
    rows.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });

    let arr: Vec<Value> = rows
        .into_iter()
        .take(limit)
        .map(|(area, id, name, node_type, page_id)| {
            json!({"id": id, "name": name, "type": node_type, "page_id": page_id, "area": area})
        })
        .collect();
    Ok(Value::Array(arr))
}
