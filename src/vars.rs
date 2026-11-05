//! Variables: authoritative import parsing (this module also hosts the
//! free-plan inference in [`infer_from_nodes`]).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::model::{
    Id, NodeRec, Rec, VariableCodeRec, VariableCollectionRec, VariableRec, VariableSnapshotRec,
};

/// [`VariableSnapshotRec::source`] for `import-variables` (no export
/// format yet identifies a plugin, so every imported file is `"file"`).
pub const SOURCE_FILE: &str = "file";
/// [`VariableSnapshotRec::source`] for a pull's Enterprise
/// `variables/local` fetch.
pub const SOURCE_ENTERPRISE_REST: &str = "enterprise_rest";

/// Errors from [`parse_variables_export`].
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("unrecognized variables export shape: {0}")]
    Shape(String),
}

/// `fnv1a64:<16 hex>` digest of `bytes` (FNV-1a 64, the same
/// non-cryptographic hash `cache::cache_key` uses) — a change detector for
/// [`VariableSnapshotRec::digest`], not an integrity proof.
pub fn snapshot_digest(bytes: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("fnv1a64:{hash:016x}")
}

/// `import-variables`' stricter validation (NWS-2961): every variable and
/// collection entry must be a JSON object, else the whole file is
/// rejected. Deliberately not part of [`parse_variables_export`], which
/// Enterprise pulls share and which stays lenient per entry.
pub fn check_import_entries(v: &Value) -> Result<(), ImportError> {
    let root = v.get("meta").unwrap_or(v);
    for (kind, key) in [
        ("variable", "variables"),
        ("collection", "variableCollections"),
    ] {
        let Some(map) = root.get(key).and_then(Value::as_object) else {
            continue; // shape errors are parse_variables_export's to report
        };
        if let Some((id, _)) = map.iter().find(|(_, e)| !e.is_object()) {
            return Err(ImportError::Shape(format!(
                "{kind} `{id}` is not an object"
            )));
        }
    }
    Ok(())
}

/// A parsed export: the variable/collection records plus the provenance
/// row describing them (NWS-2961).
#[derive(Debug)]
pub struct ParsedSnapshot {
    pub recs: Vec<(Id, Rec)>,
    pub snapshot: VariableSnapshotRec,
}

/// Parse a variables export into records plus a [`VariableSnapshotRec`]
/// tagged with `source` and `digest`. `format` is the detected shape;
/// `format_version`/`exported_at` come from the export's top-level
/// `formatVersion`/`exportedAt` keys when present (no current exporter
/// emits them; they're read so a future plugin export can).
pub fn parse_variables_snapshot(
    v: &Value,
    source: &str,
    digest: String,
) -> Result<ParsedSnapshot, ImportError> {
    let recs = parse_variables_export(v)?;
    let root = v.get("meta").unwrap_or(v);
    let mut codes = Vec::new();
    if let Some(variables) = root.get("variables").and_then(Value::as_object) {
        let sorted: BTreeMap<_, _> = variables.iter().collect();
        for (id, var) in sorted {
            let mut code_syntax: Vec<(String, String)> = var
                .get("codeSyntax")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            code_syntax.sort();
            let flag = |k: &str| var.get(k).and_then(Value::as_bool).unwrap_or(false);
            codes.push(VariableCodeRec {
                id: id.clone(),
                code_syntax,
                hidden_from_publishing: flag("hiddenFromPublishing"),
                remote: flag("remote"),
            });
        }
    }
    let scalar = |k: &str| match v.get(k) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(n @ Value::Number(_)) => Some(n.to_string()),
        _ => None,
    };
    Ok(ParsedSnapshot {
        recs,
        snapshot: VariableSnapshotRec {
            source: source.to_string(),
            format: if v.get("meta").is_some() {
                "rest"
            } else {
                "bare"
            }
            .to_string(),
            format_version: scalar("formatVersion"),
            exported_at: scalar("exportedAt"),
            digest,
            variables: codes,
        },
    })
}

/// A pull's Enterprise `variables/local` response as records to fold into
/// that pull's sync: [`parse_variables_export`]'s records plus the
/// `enterprise_rest` provenance row (digest over the response's canonical
/// JSON — the raw bytes aren't kept).
pub fn enterprise_snapshot_recs(v: &Value) -> Result<Vec<(Id, Rec)>, ImportError> {
    let canonical = serde_json::to_string(v).expect("Value serializes");
    let parsed = parse_variables_snapshot(
        v,
        SOURCE_ENTERPRISE_REST,
        snapshot_digest(canonical.as_bytes()),
    )?;
    let mut recs = parsed.recs;
    recs.push((Id::VariableSnapshot, Rec::VariableSnapshot(parsed.snapshot)));
    Ok(recs)
}

/// Parse a variables export: either the Enterprise REST `variables/local`
/// response (`{meta: {variables, variableCollections}}`) or the bare
/// object a plugin-console export produces. Lenient about individual
/// entries (a non-object entry yields a record with empty fields), so one
/// malformed `variables/local` entry never fails a whole pull;
/// `import-variables` adds [`check_import_entries`] on top.
pub fn parse_variables_export(v: &Value) -> Result<Vec<(Id, Rec)>, ImportError> {
    let root = v.get("meta").unwrap_or(v);
    let variables = root
        .get("variables")
        .and_then(Value::as_object)
        .ok_or_else(|| ImportError::Shape("missing `variables` object".into()))?;
    let collections = root
        .get("variableCollections")
        .and_then(Value::as_object)
        .ok_or_else(|| ImportError::Shape("missing `variableCollections` object".into()))?;

    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let mut recs = Vec::new();
    let sorted: BTreeMap<_, _> = collections.iter().collect();
    for (id, c) in sorted {
        let mut modes: Vec<(String, String)> = c
            .get("modes")
            .and_then(Value::as_array)
            .map(|ms| ms.iter().map(|m| (s(m, "modeId"), s(m, "name"))).collect())
            .unwrap_or_default();
        modes.sort();
        recs.push((
            Id::VariableCollection(id.clone()),
            Rec::VariableCollection(VariableCollectionRec {
                id: id.clone(),
                name: s(c, "name"),
                modes,
                default_mode_id: s(c, "defaultModeId"),
            }),
        ));
    }
    let sorted: BTreeMap<_, _> = variables.iter().collect();
    for (id, var) in sorted {
        let mut values_by_mode: Vec<(String, String)> = var
            .get("valuesByMode")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(mode, val)| {
                        (
                            mode.clone(),
                            serde_json::to_string(val).expect("Value serializes"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        values_by_mode.sort();
        let scopes: Vec<String> = var
            .get("scopes")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        recs.push((
            Id::Variable(id.clone()),
            Rec::Variable(VariableRec {
                id: id.clone(),
                name: s(var, "name"),
                resolved_type: s(var, "resolvedType"),
                collection_id: s(var, "variableCollectionId"),
                values_by_mode,
                description: s(var, "description"),
                scopes,
            }),
        ));
    }
    Ok(recs)
}

/// Everything known about one variable from its usage sites alone.
#[derive(Debug, Serialize)]
pub struct VarUsage {
    pub variable_id: String,
    /// (node_id, json-pointer of the bound property), sorted.
    pub sites: Vec<(String, String)>,
    /// Distinct resolved values observed at those sites (canonical JSON),
    /// sorted. Usually one value; more indicates multi-mode usage.
    pub observed: Vec<String>,
}

/// Free-plan inference: fold every node's variable bindings into per-variable
/// usage + observed resolved values (the concrete values Figma bakes in
/// next to each binding — default-mode values unless a frame overrides its
/// mode).
pub fn infer_from_nodes<'a>(nodes: impl Iterator<Item = &'a NodeRec>) -> Vec<VarUsage> {
    type VarData = (Vec<(String, String)>, Vec<String>);
    let mut by_var: BTreeMap<String, VarData> = BTreeMap::new();
    for node in nodes {
        // An unbound node contributes nothing: skip parsing its raw JSON.
        if node.bound_variables.is_empty() {
            continue;
        }
        let raw: serde_json::Value = match serde_json::from_str(&node.raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (pointer, var_id) in &node.bound_variables {
            let entry = by_var.entry(var_id.clone()).or_default();
            entry.0.push((node.id.clone(), pointer.clone()));
            if let Some(v) = raw.pointer(pointer) {
                entry
                    .1
                    .push(serde_json::to_string(v).expect("Value serializes"));
            }
        }
    }
    by_var
        .into_iter()
        .map(|(variable_id, (mut sites, mut observed))| {
            sites.sort();
            observed.sort();
            observed.dedup();
            VarUsage {
                variable_id,
                sites,
                observed,
            }
        })
        .collect()
}

/// The raw-JSON pointer prefix a style type's definition lives under on a
/// consumer node, shared by [`style_value_from_consumer`] (the definition
/// itself) and `query::styles`'s `resolve_vars` path (which variable
/// bindings belong to that definition — a binding's own pointer, e.g.
/// `/fills/0/color`, starts with this prefix).
pub fn style_value_pointer(style_type: &str) -> Option<&'static str> {
    match style_type {
        "TEXT" => Some("/style"),
        "FILL" => Some("/fills"),
        "EFFECT" => Some("/effects"),
        "GRID" => Some("/layoutGrids"),
        _ => None,
    }
}

/// Derive a style's definition from one consumer node's raw JSON.
/// Style definitions are not in the file JSON; consumers carry the
/// resolved properties.
pub fn style_value_from_consumer(style_type: &str, consumer_raw: &str) -> Option<Value> {
    let raw: Value = serde_json::from_str(consumer_raw).ok()?;
    let pointer = style_value_pointer(style_type)?;
    raw.pointer(pointer).cloned()
}
