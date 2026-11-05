//! Desktop-proxy routing shared by `serve.rs` and `cli/`: namespace rules,
//! registry merge, and uncached forwarding. Desktop calls have no authoritative
//! file/revision context and must not reuse a mirror's response cache.

use serde_json::{Value, json};

use crate::mcp::ToolDef;
use crate::upstream::{UpstreamError, UpstreamMcp};

/// The namespace rule (spec §12, §11 point 3): `figmog_*` tools are always
/// local; every other name is proxied to the upstream (when attached).
pub(crate) fn is_local_tool(name: &str) -> bool {
    name.starts_with("figmog_")
}

/// Read-like tool names do not schedule a mirror poll after forwarding.
fn is_read_tool(name: &str) -> bool {
    name.starts_with("get_") || name.starts_with("list_")
}

/// `tools/list` = the local `figmog_*` registry followed by every upstream
/// tool verbatim (name/inputSchema passed through, description prefixed
/// `"[via Figma desktop] "`). Returns the merged list plus the names of any
/// upstream tools dropped for colliding with the local registry OR with an
/// upstream tool already accepted earlier in this same call — either the
/// `figmog_*` namespace (the namespace rule makes this impossible for
/// figmog's own registry, but a live desktop server's tool list is outside
/// figmog's control), a defensive exact match against a current local
/// tool's name even without the prefix, or two upstream entries sharing a
/// name (a malformed/duplicated upstream tool list must not produce two
/// `ToolDef`s of the same name in the merged registry) — the caller logs
/// the drops.
pub(crate) fn merge_registry(
    mut local: Vec<ToolDef>,
    upstream_tools: &[Value],
) -> (Vec<ToolDef>, Vec<String>) {
    let mut seen_names: std::collections::BTreeSet<&str> = local.iter().map(|t| t.name).collect();
    let mut dropped = Vec::new();
    for tool in upstream_tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if is_local_tool(name) || seen_names.contains(name) {
            dropped.push(name.to_string());
            continue;
        }
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let input_schema = tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        let name = intern(name.to_string());
        seen_names.insert(name);
        local.push(ToolDef {
            name,
            description: intern(format!("[via Figma desktop] {description}")),
            input_schema,
        });
    }
    (local, dropped)
}

/// A `&'static str` for `text`, as `ToolDef` needs. `figmog serve`
/// re-merges whenever the desktop app appears, disappears or changes its
/// tool list (NWS-2959), so each distinct string is leaked once and reused
/// by every later merge: memory stays bounded by the distinct upstream
/// names and descriptions seen, however often the desktop comes and goes.
fn intern(text: String) -> &'static str {
    static INTERNED: std::sync::Mutex<std::collections::BTreeSet<&'static str>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let mut interned = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = interned.get(text.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(text.into_boxed_str());
    interned.insert(leaked);
    leaked
}

/// Execute one desktop `tools/call` without consulting a mirror's cache.
/// The upstream interface supplies no authoritative file identity or revision:
/// neither a node id, a caller-supplied file argument, nor a mirror's version
/// proves which desktop content will answer this call. Always forward, including
/// when legacy `proxy_cache` rows exist, until that context can be established.
///
/// Returns the upstream result verbatim and whether to schedule an immediate
/// meta-poll after a successful likely write (a non-`get_*`/`list_*` tool).
pub(crate) fn proxy_call<U: UpstreamMcp>(
    upstream: &mut U,
    name: &str,
    args: &Value,
) -> Result<(Value, bool), UpstreamError> {
    let result = upstream.call(name, args)?;
    Ok((result, !is_read_tool(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{FakeUpstream, UpstreamError};

    fn local_tool(name: &'static str) -> ToolDef {
        ToolDef {
            name,
            description: "d",
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn is_local_tool_matches_only_figmog_prefix() {
        assert!(is_local_tool("figmog_status"));
        assert!(!is_local_tool("get_code"));
        assert!(!is_local_tool("list_something"));
    }

    #[test]
    fn merge_registry_appends_upstream_tools_with_prefixed_description() {
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![json!({
            "name": "get_code",
            "description": "Returns code for a node",
            "inputSchema": {"type": "object", "properties": {"nodeId": {"type": "string"}}},
        })];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert!(dropped.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "figmog_status");
        assert_eq!(merged[1].name, "get_code");
        assert_eq!(
            merged[1].description,
            "[via Figma desktop] Returns code for a node"
        );
        assert_eq!(
            merged[1].input_schema,
            json!({"type": "object", "properties": {"nodeId": {"type": "string"}}})
        );
    }

    #[test]
    fn merge_registry_drops_and_reports_upstream_tool_named_like_local() {
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![
            json!({"name": "figmog_evil", "description": "impersonating"}),
            json!({"name": "get_code", "description": "fine"}),
        ];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert_eq!(dropped, vec!["figmog_evil".to_string()]);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|t| t.name == "get_code"));
        assert!(!merged.iter().any(|t| t.name == "figmog_evil"));
    }

    #[test]
    fn merge_registry_drops_upstream_tool_matching_local_name_without_prefix() {
        // A local tool name without the `figmog_` prefix (hypothetical, but
        // the dedup must key off actual local names, not just the prefix
        // rule) still shadows an identically-named upstream tool.
        let local = vec![local_tool("status")];
        let upstream = vec![
            json!({"name": "status", "description": "impersonating"}),
            json!({"name": "get_code", "description": "fine"}),
        ];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert_eq!(dropped, vec!["status".to_string()]);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|t| t.name == "get_code"));
        assert_eq!(merged.iter().filter(|t| t.name == "status").count(), 1);
    }

    #[test]
    fn merge_registry_defaults_missing_description_and_schema() {
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![json!({"name": "get_screenshot"})];
        let (merged, _dropped) = merge_registry(local, &upstream);
        assert_eq!(merged[1].description, "[via Figma desktop] ");
        assert_eq!(merged[1].input_schema, json!({"type": "object"}));
    }

    #[test]
    fn merge_registry_drops_a_second_upstream_tool_with_a_duplicate_name() {
        // Two upstream entries sharing a name (a malformed/duplicated
        // upstream tool list) must still only produce one `ToolDef` in the
        // merged registry — the second is dropped, not appended as a
        // duplicate.
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![
            json!({"name": "get_code", "description": "first"}),
            json!({"name": "get_code", "description": "second"}),
        ];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert_eq!(dropped, vec!["get_code".to_string()]);
        assert_eq!(merged.iter().filter(|t| t.name == "get_code").count(), 1);
        assert_eq!(
            merged
                .iter()
                .find(|t| t.name == "get_code")
                .unwrap()
                .description,
            "[via Figma desktop] first"
        );
    }

    #[test]
    fn repeated_merges_reuse_interned_upstream_strings() {
        let upstream = vec![json!({"name": "get_code", "description": "d"})];
        let (first, _) = merge_registry(vec![], &upstream);
        let (second, _) = merge_registry(vec![], &upstream);
        assert!(std::ptr::eq(first[0].name, second[0].name));
        assert!(std::ptr::eq(first[0].description, second[0].description));
    }

    #[test]
    fn desktop_reads_always_reach_upstream_without_triggering_a_poll() {
        for (name, args) in [
            ("get_code", json!({"nodeId": "1:2"})),
            ("get_metadata", json!({"node_id": "1:2"})),
            ("list_variables", json!({"id": "1:2"})),
            ("get_code", json!({})),
        ] {
            let mut upstream = FakeUpstream::new(vec![]);
            for text in ["file B", "file C", "file C edited"] {
                let expected = json!({"content": [{"type": "text", "text": text}]});
                upstream.push_result(Ok(expected.clone()));
                let (value, poll) = proxy_call(&mut upstream, name, &args).unwrap();
                assert_eq!(value, expected);
                assert!(!poll);
            }
            assert_eq!(upstream.call_count, 3);
        }
    }

    #[test]
    fn proxy_call_write_triggers_meta_poll() {
        let mut upstream = FakeUpstream::new(vec![]);
        upstream.push_result(Ok(json!({"ok": true})));
        let (_value, poll) = proxy_call(&mut upstream, "add_code_connect_map", &json!({})).unwrap();
        assert!(poll, "a non-get/list write should trigger a meta poll");
    }

    #[test]
    fn proxy_call_tool_error_is_forwarded_and_next_call_retries() {
        let mut upstream = FakeUpstream::new(vec![]);
        for expected in [
            json!({"content": [{"type": "text", "text": "node not found"}], "isError": true}),
            json!({"content": [{"type": "text", "text": "fresh"}], "isError": false}),
        ] {
            upstream.push_result(Ok(expected.clone()));
            let (value, poll) =
                proxy_call(&mut upstream, "get_code", &json!({"nodeId": "1:2"})).unwrap();
            assert_eq!(value, expected);
            assert!(!poll);
        }
        assert_eq!(upstream.call_count, 2);
    }

    #[test]
    fn proxy_call_propagates_upstream_error() {
        let mut upstream = FakeUpstream::new(vec![]);
        upstream.push_result(Err(UpstreamError::Protocol("boom".into())));
        let err = proxy_call(&mut upstream, "get_code", &json!({})).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }
}
