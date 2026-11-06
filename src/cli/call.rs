//! Desktop-proxy CLI parity: `figmog tools`, `figmog call`, and
//! `figmog import-variables` — every tool `figmog serve` would expose,
//! reachable without an MCP client.

use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::dispatch;
use crate::model::Id;
use crate::proxy;
use crate::upstream::{HttpUpstream, UpstreamMcp};

use super::pull::do_pull;
use super::{Db, write_json};

/// `figmog import-variables <export.json>`: replace the stored variable
/// snapshot (NWS-2961). The file is read, parsed and validated before the
/// store is opened, so an unreadable or invalid file is rejected with the
/// previous snapshot untouched; a valid one replaces it atomically
/// (`store::replace_variable_snapshot`), removing variables and
/// collections absent from the new export.
pub(super) fn cmd_import_variables(
    db: &Db,
    path: PathBuf,
    allow_empty: bool,
    dry_run: bool,
) -> Result<(), String> {
    let (_, parsed) = load_import(&path)?;
    let invalid = |e: String| format!("invalid variables file {}: {e}", path.display());

    let imported = parsed
        .recs
        .iter()
        .filter(|(id, _)| matches!(id, Id::Variable(_)))
        .count();
    // A destination can exist without holding a store. Previewing a missing
    // or empty directory must not initialize a database to discover no variables.
    if dry_run {
        let reading = |e| format!("reading store directory {}: {e}", db.path.display());
        let empty = match std::fs::read_dir(&db.path) {
            Ok(mut entries) => entries.next().transpose().map_err(reading)?.is_none(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => return Err(reading(e)),
        };
        if empty {
            return write_json(&json!({"imported": imported, "removed": 0, "removed_ids": []}));
        }
    }
    let mut st = db.open(|path| crate::open_store!(path))?;
    let prior = st.rtx(|(_, _, _, _, variables, collections, ..)| {
        crate::store::collect_variable_ids(&variables, &collections)
    });
    let live: std::collections::BTreeSet<_> = parsed.recs.iter().map(|(id, _)| id).collect();
    let removed_ids: Vec<_> = prior
        .iter()
        .filter_map(|id| match id {
            Id::Variable(id_string) if !live.contains(id) => Some(id_string.clone()),
            _ => None,
        })
        .collect();
    if dry_run {
        return write_json(
            &json!({"imported": imported, "removed": removed_ids.len(), "removed_ids": removed_ids}),
        );
    }
    if imported == 0 && !allow_empty && prior.iter().any(|id| matches!(id, Id::Variable(_))) {
        return Err(invalid(
            "empty export would remove stored variables; pass --allow-empty to replace them"
                .to_string(),
        ));
    }
    let churn =
        crate::store::replace_variable_snapshot(&mut st, &prior, &parsed.recs, parsed.snapshot);
    let output = write_json(&json!({"imported": imported, "removed": removed_ids.len()}));
    if churn.wrote() {
        st.close_and_compact();
    }
    output
}

/// `import-variables` through the running serve that owns the store
/// (NWS-2963): the file is read and fully validated here first (the direct
/// path's errors, nothing sent for an invalid file), then its exact text is
/// sent as content — the snapshot digest is over those bytes. Serve
/// validates again, replaces the snapshot on the file's worker and answers
/// once it committed and was published; the output is the direct path's.
#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_import_variables_routed(
    db: &Db,
    route: &super::socket::Route,
    client: super::socket::Client,
    key: &str,
    path: PathBuf,
    allow_empty: bool,
    dry_run: bool,
) -> Result<(), String> {
    let (bytes, _) = load_import(&path)?;
    let content = String::from_utf8(bytes)
        .map_err(|e| format!("invalid variables file {}: {e}", path.display()))?;
    let args = json!({"file": key, "db_bytes": db.path.as_os_str().as_bytes(), "content": content, "name": path.display().to_string(), "allow_empty": allow_empty, "dry_run": dry_run});
    let done =
        super::socket::write_over(route, client, crate::serve::writes::IMPORT_VARIABLES, args)?;
    write_json(&done["result"])
}

/// Read, parse and validate a variables export before any store is opened,
/// so an unreadable or invalid file leaves the previous snapshot untouched.
fn load_import(path: &std::path::Path) -> Result<(Vec<u8>, crate::vars::ParsedSnapshot), String> {
    let invalid = |e: String| format!("invalid variables file {}: {e}", path.display());
    let bytes = std::fs::read(path).map_err(|e| invalid(e.to_string()))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
    crate::vars::check_import_entries(&v).map_err(|e| invalid(e.to_string()))?;
    let parsed = crate::vars::parse_variables_snapshot(
        &v,
        crate::vars::SOURCE_FILE,
        crate::vars::snapshot_digest(&bytes),
    )
    .map_err(|e| invalid(e.to_string()))?;
    Ok((bytes, parsed))
}

/// Probe `upstream_url` unless `no_upstream`, matching `figmog serve`'s own
/// startup behavior exactly: on failure, one stderr line and local-only
/// (never a hard error — see build design §12 "Startup").
fn attach_upstream(
    upstream_url: String,
    no_upstream: bool,
) -> (Option<HttpUpstream>, &'static str) {
    if no_upstream {
        return (None, "disabled");
    }
    let mut client = HttpUpstream::new(upstream_url);
    match client.initialize() {
        Ok(()) => (Some(client), "connected"),
        Err(e) => {
            eprintln!("figmog: upstream unreachable, serving local tools only: {e}");
            (None, "unreachable")
        }
    }
}

/// `figmog tools`: the merged registry `figmog serve` would expose for this
/// mirror — local tools always, upstream tools when reachable. Never opens
/// the store (spec §4 debt item M5): dispatched in `super::dispatch`
/// before `resolve_db`, so it works with no established mirror at all.
pub(super) fn cmd_tools(upstream_url: String, no_upstream: bool) -> Result<(), String> {
    let (upstream, status) = attach_upstream(upstream_url, no_upstream);
    let (tools, dropped) = match &upstream {
        Some(u) => proxy::merge_registry(dispatch::tool_registry(), u.tools()),
        None => (dispatch::tool_registry(), Vec::new()),
    };
    for name in &dropped {
        eprintln!("figmog: dropping upstream tool named like a local tool: {name}");
    }
    if status != "connected" {
        eprintln!("figmog: upstream {status} — showing local tools only");
    }

    let rows: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "source": if proxy::is_local_tool(t.name.as_ref()) { "local" } else { "upstream" },
                "cacheable": false,
            })
        })
        .collect();
    write_json(&Value::Array(rows))
}

/// `figmog call <tool> [--args json]`: invoke any tool by name through the
/// same routing `figmog serve` uses — local `figmog_*` tools (including
/// `figmog_sync`) and, when attached, the same uncached desktop proxy.
pub(super) fn cmd_call(
    db: &Db,
    tool: String,
    args: Option<String>,
    upstream_url: String,
    no_upstream: bool,
) -> Result<(), String> {
    let args = crate::deep_json::OwnedValue(parse_args(args.as_deref())?);
    // `file` is routing (already resolved into `db`), so neither dispatch
    // nor `do_pull` reads it; still reject a wrong-typed one as serve does
    // (NWS-2944) — before the `figmog_sync` early return and before any
    // store is opened.
    validate_local_args(&tool, &args)?;

    // `figmog_sync` delegates entirely to `do_pull`, which opens its own
    // `open_store!` handle at `db.path`. fjall allows only one open handle
    // per process for a given store, so this has to return *before* this
    // function opens its own `st` below — opening both in the same process
    // deadlocks/panics on the second open's file lock (this is why this
    // branch can't just join the `if tool == "figmog_sync"` chain further
    // down, the way `figmog serve`'s handler can: `run_serve` never opens a
    // second handle for `figmog_sync`, since it reuses its own long-lived
    // `st` instead of calling `do_pull`).
    if tool == "figmog_sync" {
        // `geometry: false` here means "no new override" — `do_pull`
        // internally unions this with whatever's already stored (spec §4
        // stickiness), so a plain `figmog_sync` still re-requests geometry
        // for a mirror that was ever pulled with `--geometry`.
        let (churn, _, _, compaction) =
            do_pull(db, None, None, false, false).map_err(|e| e.to_string())?;
        let output = print_call_result(serde_json::to_value(churn).map_err(|e| e.to_string()));
        if let Some(compaction) = compaction {
            compaction.run();
        }
        return output;
    }

    let (mut upstream, upstream_status) = attach_upstream(upstream_url, no_upstream);
    let st = db.open_existing(|path| crate::open_store!(path))?;

    let result: Result<Value, String> = if proxy::is_local_tool(&tool) {
        match st.rtx(|r| dispatch::dispatch_read_tool(&tool, &args, upstream_status, &r, None)) {
            Some(r) => r,
            None => Err(format!("unknown tool: {tool}")),
        }
    } else {
        let up = upstream
            .as_mut()
            .ok_or_else(|| format!("upstream not attached: {tool}"))?;
        proxy::proxy_call(up, &tool, &args).map_err(|e| e.to_string()).map(|(value, trigger_poll)| {
            if trigger_poll {
                eprintln!(
                    "figmog: {tool} may have changed the file — run `figmog pull` (or `figmog serve`, which polls automatically) to refresh the mirror"
                );
            }
            value
        })
    };

    print_call_result(result)
}

/// Reject malformed local calls before mirror selection, socket routing,
/// upstream attachment, or opening a store can have side effects.
pub(super) fn validate_local_args(tool: &str, args: &Value) -> Result<(), String> {
    if proxy::is_local_tool(tool) {
        dispatch::validate_object_args(args)?;
        dispatch::arg_str(args, "file")?;
        dispatch::ReadCommand::from_tool_args(tool, args)?;
    }
    Ok(())
}

/// Parse call arguments identically for mirror selection and dispatch.
pub(super) fn parse_args(raw: Option<&str>) -> Result<Value, String> {
    match raw {
        Some(raw) => crate::deep_json::from_str(raw)
            .map(crate::deep_json::OwnedValue::into_inner)
            .map_err(|e| format!("--args: invalid JSON: {e}")),
        None => Ok(json!({})),
    }
}

/// Shared `figmog call` output: pretty-printed JSON on success; on failure,
/// propagates the error so `run`'s top-level handler emits `{"error": ...}`
/// on stderr and exits 1, same as every other command.
fn print_call_result(result: Result<Value, String>) -> Result<(), String> {
    write_json(&crate::deep_json::OwnedValue(result?))
}
