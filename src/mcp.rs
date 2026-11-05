//! MCP protocol core — pure JSON-RPC dispatch over stdio frames.
//!
//! This module owns no I/O and no store: [`dispatch_message`] takes one raw
//! JSON-RPC message, a registry of [`ToolDef`]s, and a [`ToolHandler`], and
//! returns the response frame to write (or `None` for notifications), or —
//! when the handler defers a `tools/call` — the [`PendingCall`] that frames
//! its response once the result arrives. The serve loop owns the I/O.

use serde_json::{Value, json};

/// Steering text carried verbatim in the `initialize` result. Local tools
/// answer from mirrors; native tools forward live to desktop, whose active
/// file/revision is independent of those mirrors. `file` routes local tools.
const INSTRUCTIONS: &str = "figmog is your Figma server: a local, instant mirror of one Figma file plus a proxy to Figma's native capabilities. Call figmog for everything Figma-related. figmog_* tools answer from the local mirror at zero API cost; native-named tools (get_*, …) go live to Figma desktop and are forwarded uncached. Pass the Figma file URL as the `file` argument to figmog_* tools when you have one; figmog queues a background pull on first reference. Open/sync return a job acknowledgement; poll figmog_status or figmog_files for ready, progress, version/churn and errors.";

/// The default MCP protocol version echoed when a client's `initialize`
/// request omits `protocolVersion`.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// This server's name, reported in `initialize`'s `serverInfo.name`.
pub const SERVER_NAME: &str = "figmog";

/// One registered tool: metadata for `tools/list`.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

/// What a `tools/call` handler produces on success (spec §11/§12: local
/// `figmog_*` tools own their own JSON shape and answer it as MCP text
/// content the way figmog always has; proxied tools' results are already a
/// complete, correctly-shaped MCP `CallToolResult` produced by the
/// upstream — re-wrapping that as ANOTHER text block would double-encode
/// it and, for a non-text content type such as `get_screenshot`'s image
/// block, make it unrenderable).
pub enum ToolOutput {
    /// figmog's own JSON, serialized into a single text content block —
    /// today's (v1/v2) behavior, still used for every local `figmog_*`
    /// tool.
    Json(Value),
    /// A complete MCP `tools/call` result, emitted verbatim as the
    /// JSON-RPC `result` member. Used for proxied calls, whose shape (and
    /// `isError`) is the upstream's to own.
    Raw(Value),
    /// The handler accepted the call as off-loop work identified by this
    /// token; its result arrives later and is turned into a response with
    /// [`PendingCall::finish`]. Only [`dispatch_message`] supports this.
    Deferred(u64),
}

/// Executes a `tools/call`. `Ok(output)` becomes the success result per
/// [`ToolOutput`]'s two shapes; `Err(msg)` becomes `isError` text content
/// on stdio or a JSON-RPC error on the socket control plane.
pub trait ToolHandler {
    fn call(&mut self, name: &str, args: &Value) -> Result<ToolOutput, String>;
}

/// Adapts a closure to [`ToolHandler`]. `figmog serve`'s store handle has
/// an unnameable type (the `open_store!` pipeline contains fn items), so
/// it can't be held in a named struct field generic over the store's
/// pipeline type; wrapping a closure that captures the store by unique
/// reference sidesteps that entirely — the closure's environment can hold
/// whatever concrete type it was defined against.
pub struct FnHandler<F>(pub F);

impl<F: FnMut(&str, &Value) -> Result<ToolOutput, String>> ToolHandler for FnHandler<F> {
    fn call(&mut self, name: &str, args: &Value) -> Result<ToolOutput, String> {
        (self.0)(name, args)
    }
}

/// Handle one incoming JSON-RPC message. Returns the response frame to
/// write, or `None` for notifications (a message with no `id`, or whose
/// method starts with `notifications/`). For handlers that never defer: a
/// [`ToolOutput::Deferred`] here has nowhere to be delivered and becomes a
/// tool error.
#[cfg(test)]
pub fn handle_message(
    raw: &str,
    tools: &[ToolDef],
    handler: &mut dyn ToolHandler,
) -> Option<Value> {
    immediate(dispatch_message(
        raw,
        tools,
        handler,
        ToolErrorMode::McpContent,
    ))
}

#[cfg(test)]
fn immediate(reply: Option<Reply>) -> Option<Value> {
    reply.map(|reply| match reply {
        Reply::Now(value) => value,
        Reply::Later(pending) => pending.finish(Err(
            "internal error: deferred call has no delivery path".into(),
        )),
    })
}

/// How a transport reports serve-generated tool failures: MCP `isError`
/// content (stdio) or a top-level JSON-RPC error (socket control plane), so
/// CLI clients can distinguish them from genuine upstream
/// `CallToolResult`s, whose tool-level `isError` belongs to the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolErrorMode {
    McpContent,
    JsonRpc,
}

/// One dispatched message's response: ready now, or owed later for a
/// `tools/call` whose handler returned [`ToolOutput::Deferred`].
pub(crate) enum Reply {
    Now(Value),
    Later(PendingCall),
}

/// A `tools/call` answered off the main loop. Keeps exactly what the
/// eventual response needs — the caller's JSON-RPC `id`, the tool name
/// (for `structuredContent`) and the transport's error mode — so a late
/// result is framed identically to an immediate one.
#[derive(Debug)]
pub(crate) struct PendingCall {
    id: Value,
    name: String,
    mode: ToolErrorMode,
    token: u64,
}

impl PendingCall {
    /// The handler's token from [`ToolOutput::Deferred`].
    pub(crate) fn token(&self) -> u64 {
        self.token
    }

    /// Frame the deferred handler result as this call's response.
    pub(crate) fn finish(self, output: Result<ToolOutput, String>) -> Value {
        let result = match output {
            Ok(ToolOutput::Deferred(_)) => {
                Err("internal error: deferred call deferred again".to_string())
            }
            Ok(output) => Ok(tool_output_result(&self.name, output)),
            Err(message) => Err(message),
        };
        call_response(self.id, self.mode, result)
    }
}

/// Handle one message, allowing the handler to defer a `tools/call`.
/// `None` for notifications.
pub(crate) fn dispatch_message(
    raw: &str,
    tools: &[ToolDef],
    handler: &mut dyn ToolHandler,
    tool_errors: ToolErrorMode,
) -> Option<Reply> {
    let msg: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            return Some(Reply::Now(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": "parse error"},
            })));
        }
    };

    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // Notifications: no `id`, or method under the `notifications/` namespace.
    if id.is_none() || method.starts_with("notifications/") {
        return None;
    }
    let id = id.unwrap();

    let result = match method {
        "initialize" => Some(initialize_result(&params)),
        "ping" => Some(json!({})),
        "tools/list" => Some(tools_list_result(tools)),
        "tools/call" => {
            return Some(match tools_call_result(&params, tools, handler) {
                Ok(Called::Now(result)) => Reply::Now(call_response(id, tool_errors, Ok(result))),
                Ok(Called::Later { name, token }) => Reply::Later(PendingCall {
                    id,
                    name,
                    mode: tool_errors,
                    token,
                }),
                Err(message) => Reply::Now(call_response(id, tool_errors, Err(message))),
            });
        }
        _ => None,
    };

    Some(Reply::Now(match result {
        Some(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not found"},
        }),
    }))
}

/// Frame a `tools/call` outcome per the transport's error mode.
fn call_response(id: Value, mode: ToolErrorMode, result: Result<Value, String>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => match mode {
            ToolErrorMode::McpContent => {
                json!({"jsonrpc": "2.0", "id": id, "result": error_content(&message)})
            }
            ToolErrorMode::JsonRpc => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": message},
            }),
        },
    }
}

fn initialize_result(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {}},
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn tools_list_result(tools: &[ToolDef]) -> Value {
    let list: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut tool = json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            });
            if let Some(schema) = crate::dispatch::output_schema(t.name) {
                tool["outputSchema"] = schema;
            }
            tool
        })
        .collect();
    json!({"tools": list})
}

enum Called {
    Now(Value),
    Later { name: String, token: u64 },
}

fn tools_call_result(
    params: &Value,
    tools: &[ToolDef],
    handler: &mut dyn ToolHandler,
) -> Result<Called, String> {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(name) => name,
        None => return Err("missing required field: name".to_string()),
    };

    if !tools.iter().any(|t| t.name == name) {
        return Err(format!("unknown tool: {name}"));
    }

    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    Ok(match handler.call(name, &args)? {
        ToolOutput::Deferred(token) => Called::Later {
            name: name.to_string(),
            token,
        },
        output => Called::Now(tool_output_result(name, output)),
    })
}

/// The `tools/call` result member for a completed tool output.
fn tool_output_result(name: &str, output: ToolOutput) -> Value {
    match output {
        ToolOutput::Json(v) => {
            let mut result = json!({
                "content": [{"type": "text", "text": serde_json::to_string(&v).unwrap()}],
                "isError": false,
            });
            if crate::dispatch::output_schema(name).is_some() {
                result["structuredContent"] = v;
            }
            result
        }
        ToolOutput::Raw(v) => v,
        ToolOutput::Deferred(_) => error_content("internal error: unexpected deferred output"),
    }
}

fn error_content(msg: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": msg}],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ToolHandler` test double: returns `Ok(Json({"ok":true}))` for a
    /// tool named `"ok"`, `Ok(Raw(...))` for `"raw"` (an already-complete
    /// MCP result, as a proxied call would produce), `Err("boom")` for
    /// `"err"`, and panics for any other name (the dispatch contract
    /// guarantees unknown names never reach the handler).
    struct FakeHandler;

    impl ToolHandler for FakeHandler {
        fn call(&mut self, name: &str, _args: &Value) -> Result<ToolOutput, String> {
            match name {
                "ok" => Ok(ToolOutput::Json(json!({"ok": true}))),
                "raw" => Ok(ToolOutput::Raw(json!({
                    "content": [{"type": "image", "data": "base64==", "mimeType": "image/png"}],
                    "isError": false,
                }))),
                "err" => Err("boom".to_string()),
                other => panic!("handler should not be called for {other}"),
            }
        }
    }

    fn fake_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "ok",
                description: "always succeeds",
                input_schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "raw",
                description: "returns a raw passthrough result",
                input_schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "err",
                description: "always fails",
                input_schema: json!({"type": "object"}),
            },
        ]
    }

    #[test]
    fn parse_failure_returns_dash_32700_with_null_id() {
        let resp = handle_message("not json", &[], &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": "parse error"},
            })
        );
    }

    #[test]
    fn initialize_echoes_client_protocol_version_and_carries_instructions_verbatim() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"},
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "figmog", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "figmog is your Figma server: a local, instant mirror of one Figma file plus a proxy to Figma's native capabilities. Call figmog for everything Figma-related. figmog_* tools answer from the local mirror at zero API cost; native-named tools (get_*, …) go live to Figma desktop and are forwarded uncached. Pass the Figma file URL as the `file` argument to figmog_* tools when you have one; figmog queues a background pull on first reference. Open/sync return a job acknowledgement; poll figmog_status or figmog_files for ready, progress, version/churn and errors.",
                },
            })
        );
    }

    #[test]
    fn initialize_defaults_protocol_version_when_absent() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {},
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], json!("2025-06-18"));
    }

    #[test]
    fn notifications_initialized_returns_none() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        })
        .to_string();
        assert_eq!(handle_message(&raw, &[], &mut FakeHandler), None);
    }

    #[test]
    fn any_notifications_namespaced_method_returns_none_even_with_id() {
        // Per the brief, method starting "notifications/" is a notification
        // regardless of whether an `id` happens to be present.
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "notifications/whatever",
        })
        .to_string();
        assert_eq!(handle_message(&raw, &[], &mut FakeHandler), None);
    }

    #[test]
    fn message_with_no_id_returns_none() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "ping",
        })
        .to_string();
        assert_eq!(handle_message(&raw, &[], &mut FakeHandler), None);
    }

    #[test]
    fn ping_returns_empty_result() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(resp, json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
    }

    #[test]
    fn tools_list_reflects_fake_slice_in_order() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "tools": [
                        {"name": "ok", "description": "always succeeds", "inputSchema": {"type": "object"}},
                        {"name": "raw", "description": "returns a raw passthrough result", "inputSchema": {"type": "object"}},
                        {"name": "err", "description": "always fails", "inputSchema": {"type": "object"}},
                    ],
                },
            })
        );
    }

    #[test]
    fn tools_call_success_wraps_handler_ok_as_text_content() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "ok", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "text", "text": "{\"ok\":true}"}],
                    "isError": false,
                },
            })
        );
    }

    #[test]
    fn tools_call_raw_output_is_emitted_verbatim_as_the_result_member() {
        // A proxied call's result is already a complete MCP `CallToolResult`
        // (e.g. an image content block for a screenshot tool) — `Raw` must
        // pass it through untouched, NOT re-wrap it in another text block
        // (which would double-encode it and make it unrenderable).
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "raw", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "image", "data": "base64==", "mimeType": "image/png"}],
                    "isError": false,
                },
            })
        );
    }

    #[test]
    fn tools_call_handler_error_becomes_is_error_content() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "err", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "text", "text": "boom"}],
                    "isError": true,
                },
            })
        );
    }

    #[test]
    fn tools_call_unknown_tool_is_error_without_calling_handler() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "nonexistent", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        // FakeHandler panics if called with an unregistered name, so a clean
        // result here proves the handler was never invoked.
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "text", "text": "unknown tool: nonexistent"}],
                    "isError": true,
                },
            })
        );
    }

    #[test]
    fn tools_call_missing_name_is_error_without_calling_handler() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(resp["result"]["isError"], json!(true));
        assert_eq!(resp["result"]["content"][0]["type"], json!("text"));
        // A clear message, not a panic or empty string.
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.is_empty());
    }

    #[test]
    fn unknown_method_with_id_returns_dash_32601() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "totally/bogus",
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {"code": -32601, "message": "method not found"},
            })
        );
    }

    #[test]
    fn id_echoed_as_string_when_client_sends_string_id() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": "abc-123",
            "method": "ping",
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(resp["id"], json!("abc-123"));
    }

    /// Defers every call with token 42.
    struct DeferringHandler;

    impl ToolHandler for DeferringHandler {
        fn call(&mut self, _: &str, _: &Value) -> Result<ToolOutput, String> {
            Ok(ToolOutput::Deferred(42))
        }
    }

    fn deferred(mode: ToolErrorMode) -> PendingCall {
        let raw = json!({"jsonrpc": "2.0", "id": "x", "method": "tools/call",
            "params": {"name": "ok"}})
        .to_string();
        match dispatch_message(&raw, &fake_tools(), &mut DeferringHandler, mode) {
            Some(Reply::Later(call)) => call,
            _ => panic!("expected a deferred call"),
        }
    }

    #[test]
    fn deferred_calls_are_framed_like_immediate_ones_when_finished() {
        let call = deferred(ToolErrorMode::McpContent);
        assert_eq!(call.token(), 42);
        assert_eq!(
            call.finish(Ok(ToolOutput::Json(json!({"ok": true})))),
            json!({"jsonrpc": "2.0", "id": "x", "result": {
                "content": [{"type": "text", "text": "{\"ok\":true}"}], "isError": false}})
        );
        let raw = json!({"content": [], "isError": true});
        assert_eq!(
            deferred(ToolErrorMode::JsonRpc).finish(Ok(ToolOutput::Raw(raw.clone()))),
            json!({"jsonrpc": "2.0", "id": "x", "result": raw})
        );
        assert_eq!(
            deferred(ToolErrorMode::McpContent).finish(Err("boom".into())),
            json!({"jsonrpc": "2.0", "id": "x", "result": error_content("boom")})
        );
        assert_eq!(
            deferred(ToolErrorMode::JsonRpc).finish(Err("boom".into())),
            json!({"jsonrpc": "2.0", "id": "x",
                "error": {"code": -32000, "message": "boom"}})
        );
    }

    #[test]
    fn handle_message_reports_a_deferral_it_cannot_deliver() {
        let raw = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "ok"}})
        .to_string();
        let resp = handle_message(&raw, &fake_tools(), &mut DeferringHandler).unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }
}
