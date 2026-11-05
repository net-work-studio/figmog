//! Unix-socket control-plane client (spec §1): every read command, `tools`,
//! and `call` become clients of a running `figmog serve` process reachable
//! at `<figmog-root>/serve.sock`, instead of opening the store directly —
//! always-fresh answers, no single-writer lock contention, and (for `call
//! figmog_sync`) a pull that reaches the *owning* process rather than
//! failing against a store `serve` already holds locked.
//!
//! Socket routing only applies when the root is knowable at all: `--db`
//! bypasses the whole multi-file root concept (an arbitrary store path has
//! no `<root>/serve.sock` to speak of), so callers in `super::dispatch`
//! only reach this module when `cli.db.is_none()`. [`DEFAULT_ROOT`] is the
//! CLI's own root — it has no `--figmog-root` override (that's `serve`'s
//! hidden testability knob only), so this constant must match
//! [`crate::serve`]'s own default figmog-root exactly for socket routing to
//! ever find the right process.
//!
//! Every function here returns `None` (not `Some(Err(..))`) for "no serve
//! reachable at this root" — a plain connection failure — so the caller
//! falls back to a direct store open exactly as if the socket didn't
//! exist. Once a connection succeeds, `Some(Ok(_))`/`Some(Err(_))` is
//! authoritative: the call reached serve and the caller must not also
//! attempt a direct open (spec §1: "Socket reachable ⇒ the command becomes
//! a client of serve").

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// The CLI's multi-file root when `--db` was not passed — always `.figmog`
/// relative to the current directory, matching `crate::serve::run_serve`'s
/// own `--figmog-root` default (see this module's doc comment).
pub(super) const DEFAULT_ROOT: &str = ".figmog";

/// `<root>/serve.sock` — must match [`crate::serve::socket_path`] exactly.
fn socket_path(root: &Path) -> PathBuf {
    root.join("serve.sock")
}

/// A running serve answers a `tools/call` against its own already-open
/// store near-instantly; anything slower means something's wrong (a
/// wedged process, a huge query) and the CLI should time out rather than
/// hang indefinitely.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

/// Send one `tools/call` for `tool`/`args` over `<root>/serve.sock` and
/// translate the response into the same `Result<Value, String>` shape
/// every direct-mode command already returns (spec §1: "same JSON on
/// stdout, same error shapes"). See this module's doc comment for the
/// `None` vs `Some` contract.
pub(super) fn try_call(root: &Path, tool: &str, args: Value) -> Option<Result<Value, String>> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    Some(call_over(stream, tool, args))
}

/// Like [`try_call`], but for `tools/list` — used by `figmog tools`'s own
/// socket routing to report the exact merged registry the running serve
/// would expose (its own upstream connection, not this invocation's
/// `--upstream`/`--no-upstream` flags — see this module's doc comment:
/// once the socket is reachable, the command is answered by serve's own
/// state).
pub(super) fn try_tools_list(root: &Path) -> Option<Result<Vec<Value>, String>> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    Some(tools_list_over(stream))
}

/// Build the `figmog tools` row shape (`{name, source, cacheable}`) from a
/// raw `tools/list` result — shared by the socket path above and, in
/// principle, any future caller that already has a tool-def array in hand.
pub(super) fn tools_rows(tools: &[Value]) -> Value {
    let rows: Vec<Value> = tools
        .iter()
        .map(|t| {
            let name = t["name"].as_str().unwrap_or_default();
            json!({
                "name": name,
                "source": if crate::proxy::is_local_tool(name) { "local" } else { "upstream" },
                "cacheable": false,
            })
        })
        .collect();
    Value::Array(rows)
}

/// Like [`try_call`], but for `figmog_images` specifically (v0.0.2 spec
/// §5): returns the raw MCP `result` object (`{"content": [...],
/// "isError": ...}`) rather than the unwrapped domain `Value` of other
/// local tools. `figmog_images`'s content mixes a text manifest block
/// with per-item payload blocks — `image` (base64, raster formats) or
/// `text` (raw markup, SVG — see `images::to_mcp_content`'s own doc
/// comment) — `interpret_call_response`'s "parse `content[0]`'s text as
/// figmog's own JSON" contract only fits the single-text-block shape
/// every other local tool returns, so `cli::images` (the only caller)
/// needs the whole array to decode the payload blocks and write files
/// itself.
pub(super) fn try_images_call(root: &Path, args: Value) -> Option<Result<Value, String>> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    let resp = send_and_recv(
        stream,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "figmog_images", "arguments": args},
        }),
    );
    Some(resp.and_then(|resp| {
        if let Some(err) = resp.get("error") {
            return Err(protocol_error_message(err));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }))
}

fn call_over(stream: UnixStream, tool: &str, args: Value) -> Result<Value, String> {
    let resp = send_and_recv(
        stream,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        }),
    )?;
    interpret_call_response(tool, resp)
}

fn tools_list_over(stream: UnixStream) -> Result<Vec<Value>, String> {
    let resp = send_and_recv(
        stream,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )?;
    if let Some(err) = resp.get("error") {
        return Err(protocol_error_message(err));
    }
    Ok(resp["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// One newline-delimited JSON-RPC frame out, its response back — the wire
/// mechanics every call in this module shares. `initialize` is
/// deliberately skipped: spec §1 makes it optional for socket clients, and
/// `mcp::dispatch_message` treats every frame independently regardless of
/// handshake state (verified by `serve.rs`'s socket-loop tests), so a bare
/// `tools/call`/`tools/list` is answered exactly as if a handshake had
/// happened first. Skipping it also means serve never sends this client
/// notifications (only connections that sent `initialize` get them), but
/// the reader still tolerates them: frames carrying a `method` (a
/// notification or a server request) and responses for another id are
/// skipped until the response to `frame`'s own id arrives (NWS-2959).
fn send_and_recv(stream: UnixStream, frame: &Value) -> Result<Value, String> {
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writeln!(writer, "{frame}").map_err(|e| format!("writing to serve socket: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("writing to serve socket: {e}"))?;

    let mut reader = BufReader::new(stream);
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    loop {
        let line = read_frame(&mut reader)?;
        let resp: Value = serde_json::from_str(&line)
            .map_err(|e| format!("serve socket sent invalid JSON: {e}"))?;
        if is_response_to(&resp, &id) {
            return Ok(resp);
        }
    }
}

/// Whether `resp` is the response to the request with `id`: a response
/// (no `method`) carrying that id, or an error with a null id (serve could
/// not read the request's id at all, e.g. a parse error).
fn is_response_to(resp: &Value, id: &Value) -> bool {
    if resp.get("method").is_some() {
        return false;
    }
    match resp.get("id") {
        Some(got) if got == id => true,
        Some(Value::Null) | None => resp.get("error").is_some(),
        Some(_) => false,
    }
}

/// One newline-terminated frame from serve, as text.
fn read_frame(reader: &mut BufReader<UnixStream>) -> Result<String, String> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(|e| {
        // Review m2: a timed-out read (the timeout set above) reads as a
        // generic OS error otherwise (`"Resource temporarily unavailable"`
        // or similar, depending on platform) — spell out what actually
        // happened and the escape hatch, rather than leaving the operator
        // to guess.
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            format!(
                "figmog serve did not respond within {}s (is it wedged? try --no-socket)",
                SOCKET_TIMEOUT.as_secs()
            )
        } else {
            format!("reading from serve socket: {e}")
        }
    })?;
    if n == 0 {
        return Err("serve socket closed before responding".to_string());
    }
    Ok(line)
}

fn protocol_error_message(err: &Value) -> String {
    err.get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_string()
}

/// Pure translation of one `tools/call` JSON-RPC response into the plain
/// `Result<Value, String>` shape every CLI command already produces —
/// split from [`call_over`]'s I/O so it's unit-testable directly against
/// hand-built response `Value`s. Only local single-JSON tools are
/// unwrapped; desktop results and `figmog_images` retain their MCP envelope.
fn interpret_call_response(tool: &str, resp: Value) -> Result<Value, String> {
    if let Some(err) = resp.get("error") {
        return Err(protocol_error_message(err));
    }
    let result = resp.get("result").cloned().unwrap_or(Value::Null);
    if !crate::proxy::is_local_tool(tool) {
        // Socket dispatch reports serve-generated failures as JSON-RPC
        // errors above. Only genuine desktop results reach this branch,
        // including the desktop's own tool-level `isError` responses.
        return Ok(result);
    }
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = result["content"][0]["text"].as_str().map(str::to_string);

    if is_error {
        return Err(text.unwrap_or_else(|| "unknown error from serve".to_string()));
    }
    if tool == "figmog_images" {
        return Ok(result);
    }

    match text {
        // Local single-JSON tools answer as `ToolOutput::Json`, i.e. one
        // text block holding figmog's own JSON (see `mcp::ToolOutput`).
        // Parse it back out for the CLI's established local read shape.
        Some(t) => serde_json::from_str(&t)
            .map_err(|e| format!("serve returned non-JSON tool content: {e}")),
        // Preserve the existing fallback for a local result without text.
        None => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_call_response_parses_json_tool_content() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "{\"ok\":true}"}], "isError": false},
        });
        assert_eq!(
            interpret_call_response("figmog_status", resp).unwrap(),
            json!({"ok": true})
        );
    }

    #[test]
    fn interpret_call_response_is_error_becomes_err_with_the_message_text() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "no node 99:99 in the mirror"}], "isError": true},
        });
        assert_eq!(
            interpret_call_response("figmog_get", resp).unwrap_err(),
            "no node 99:99 in the mirror"
        );
    }

    #[test]
    fn interpret_call_response_protocol_error_becomes_err() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "method not found"},
        });
        assert_eq!(
            interpret_call_response("get_code", resp).unwrap_err(),
            "method not found"
        );
    }

    #[test]
    fn interpret_call_response_raw_result_with_no_text_content_passes_through() {
        // A proxied (non-figmog_*) tool's result has no single text block
        // (e.g. an image content type) — passed through as-is rather than
        // failed as "non-JSON tool content".
        let raw_result = json!({
            "content": [{"type": "image", "data": "YQ==", "mimeType": "image/png"}],
            "isError": false,
        });
        let resp = json!({"jsonrpc": "2.0", "id": 1, "result": raw_result});
        assert_eq!(
            interpret_call_response("get_screenshot", resp).unwrap(),
            raw_result
        );
    }

    /// A notification (or another id's response) arriving before the reply
    /// is skipped, not printed as the answer (NWS-2959).
    #[test]
    fn send_and_recv_skips_notifications_and_matches_the_reply_by_id() {
        let (client, server) = UnixStream::pair().unwrap();
        let serve = std::thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            let mut out = server;
            for frame in [
                json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
                json!({"jsonrpc": "2.0", "id": 7, "result": {"other": true}}),
                json!({"jsonrpc": "2.0", "id": request["id"], "result": {"mine": true}}),
            ] {
                writeln!(out, "{frame}").unwrap();
            }
        });
        let resp = send_and_recv(
            client,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .unwrap();
        assert_eq!(resp["result"], json!({"mine": true}));
        serve.join().unwrap();
    }

    #[test]
    fn a_null_id_error_still_answers_the_request() {
        let error = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}});
        assert!(is_response_to(&error, &json!(1)));
        assert!(!is_response_to(
            &json!({"jsonrpc": "2.0", "id": null, "result": {}}),
            &json!(1)
        ));
        assert!(!is_response_to(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
            &json!(1)
        ));
    }

    #[test]
    fn tools_rows_classifies_local_vs_upstream_and_cacheable() {
        let tools = vec![
            json!({"name": "figmog_status"}),
            json!({"name": "get_code"}),
            json!({"name": "set_selection"}),
        ];
        let rows = tools_rows(&tools);
        let rows = rows.as_array().unwrap();
        assert_eq!(
            rows[0],
            json!({"name": "figmog_status", "source": "local", "cacheable": false})
        );
        assert_eq!(
            rows[1],
            json!({"name": "get_code", "source": "upstream", "cacheable": false})
        );
        assert_eq!(
            rows[2],
            json!({"name": "set_selection", "source": "upstream", "cacheable": false})
        );
    }
}
