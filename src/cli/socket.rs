//! Unix-socket control-plane client (spec §1): every read command, `tools`,
//! and `call` become clients of a running `figmog serve` process (the
//! repository's background owner) over its socket, instead of opening the
//! store directly —
//! always-fresh answers, no single-writer lock contention, and (for `call
//! figmog_sync`) a pull that reaches the *owning* process rather than
//! failing against a store `serve` already holds locked. The writers
//! `pull` and `import-variables` run as serve operations through
//! [`write_over`] (NWS-2963): one request, an operation-id notification,
//! then the answer once the write committed and was published.
//!
//! Socket routing only applies when the project is knowable at all: `--db`
//! bypasses it (an arbitrary store path has no owner to speak of), so
//! callers in `super::dispatch` only reach this module when
//! `cli.db.is_none()`. A command looks for the repository's background
//! owner first ([`crate::owner`], NWS-2953: discovery in the mirror root,
//! then the handshake) — the same process from every worktree — and then
//! for an in-process serve's `<local root>/serve.sock`.
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
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::owner::{self, OwnerPaths, Probe};
use crate::project::Layout;

/// Where a command looks for a running serve: the repository's background
/// owner (NWS-2953), found through its discovery record in the mirror
/// root, and then an in-process serve's `<local root>/serve.sock`
/// (`--figmog-root`, test setups).
#[derive(Clone, Debug)]
pub(super) struct Route {
    mirror_root: PathBuf,
    local_root: PathBuf,
}

impl Route {
    pub(super) fn new(layout: &Layout) -> Self {
        Self {
            mirror_root: layout.mirror_root.clone(),
            local_root: layout.local_root.clone(),
        }
    }
}

/// An open connection to a serve (handshaken, for the owner).
pub(crate) struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn new(stream: UnixStream) -> std::io::Result<Self> {
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        })
    }
}

/// Why a call over the socket failed.
enum Failure {
    /// The owner is shutting down and did not accept the request (nothing
    /// was started): reconnect and retry.
    NotAccepted,
    Error(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Error(message)
    }
}

/// How often a command reconnects after "not accepted" answers before it
/// gives up.
const MAX_RECONNECTS: u32 = 5;

/// `<root>/serve.sock` — must match [`crate::serve::socket_path`] exactly.
fn socket_path(root: &std::path::Path) -> PathBuf {
    root.join("serve.sock")
}

/// Connect to the running serve on `route`, if any. A background owner
/// that is shutting down is waited out (it may be followed by a new one);
/// `None` means none is running. An owner that refuses the handshake (a
/// different figmog build) is an error, never a silent direct open.
pub(super) fn connect(route: &Route) -> Option<Result<Client, String>> {
    if let Some(paths) = OwnerPaths::existing(&route.mirror_root) {
        match owner::probe_after_close(&paths, None, owner::START_TIMEOUT) {
            Ok(Probe::Ready(conn)) => {
                return Some(Ok(Client {
                    reader: conn.reader,
                    writer: conn.writer,
                }));
            }
            Ok(Probe::Absent | Probe::Closing) => {}
            Err(error) => return Some(Err(error)),
        }
    }
    let stream = UnixStream::connect(socket_path(&route.local_root)).ok()?;
    Some(Client::new(stream).map_err(|e| e.to_string()))
}

/// Run `call` on a connection, reconnecting for each "not accepted" answer.
/// `None`: no serve is running (any more).
fn with_client<T>(
    route: &Route,
    mut call: impl FnMut(Client) -> Result<T, Failure>,
) -> Option<Result<T, String>> {
    for _ in 0..MAX_RECONNECTS {
        let client = match connect(route)? {
            Ok(client) => client,
            Err(error) => return Some(Err(error)),
        };
        match call(client) {
            Ok(value) => return Some(Ok(value)),
            Err(Failure::NotAccepted) => continue,
            Err(Failure::Error(error)) => return Some(Err(error)),
        }
    }
    Some(Err(
        "the background figmog kept refusing new requests while shutting down; retry".into(),
    ))
}

/// A running serve answers a `tools/call` against its own already-open
/// store near-instantly; anything slower means something's wrong (a
/// wedged process, a huge query) and the CLI should time out rather than
/// hang indefinitely.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

/// Send one `tools/call` for `tool`/`args` to the running serve and
/// translate the response into the same `Result<Value, String>` shape
/// every direct-mode command already returns (spec §1: "same JSON on
/// stdout, same error shapes"). See this module's doc comment for the
/// `None` vs `Some` contract.
pub(super) fn try_call(route: &Route, tool: &str, args: Value) -> Option<Result<Value, String>> {
    with_client(route, |client| call_over(client, tool, args.clone()))
}

/// Like [`try_call`], but for `tools/list` — used by `figmog tools`'s own
/// socket routing to report the exact merged registry the running serve
/// would expose (its own upstream connection, not this invocation's
/// `--upstream`/`--no-upstream` flags — see this module's doc comment:
/// once the socket is reachable, the command is answered by serve's own
/// state).
pub(super) fn try_tools_list(route: &Route) -> Option<Result<Vec<Value>, String>> {
    with_client(route, tools_list_over)
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
pub(super) fn try_images_call(route: &Route, args: Value) -> Option<Result<Value, String>> {
    with_client(route, |client| {
        let resp = send_and_recv(
            client,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "figmog_images", "arguments": args.clone()},
            }),
        )?;
        if let Some(err) = resp.get("error") {
            return Err(Failure::Error(protocol_error_message(err)));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    })
}

/// How long a routed write may take to report: it may queue behind other
/// writes and pulls of its file, and a pull's fetch alone has a two-minute
/// HTTP budget. Past it, the outcome is unknown.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Run one write operation (`crate::serve::writes`) on the serve at the
/// other end of `client`, waiting on this connection for its answer —
/// `{"operation": id, "result": ...}`, sent once the write committed and
/// was published. A serve-side failure is that failure's error (nothing was
/// applied). If the connection ends first (serve killed or stopped), the
/// write is reported as **outcome unknown**, naming the operation id serve
/// announced when it accepted the write; it is never retried. Only a write
/// serve explicitly did not accept or start (a background owner shutting
/// down, NWS-2953, or any serve draining with the write still queued,
/// NWS-2963) is sent again, on a new connection from `route`.
pub(crate) fn write_over(
    route: &Route,
    client: Client,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    })
    .to_string();
    if frame.len() >= crate::serve::MAX_FRAME_BYTES {
        return Err(format!(
            "this write is too large to send to the running figmog serve ({} bytes; limit {} MiB) — stop it and run the command again to apply it directly",
            frame.len(),
            crate::serve::MAX_FRAME_BYTES / (1024 * 1024)
        ));
    }
    let mut client = client;
    for _ in 0..MAX_RECONNECTS {
        match write_once(client, tool, &frame) {
            Err(Failure::NotAccepted) => {}
            Err(Failure::Error(error)) => return Err(error),
            Ok(value) => return Ok(value),
        }
        client = match connect(route) {
            Some(Ok(client)) => client,
            Some(Err(error)) => return Err(error),
            None => {
                return Err(
                    "figmog serve shut down before accepting this write; nothing was applied — rerun the command".into(),
                );
            }
        };
    }
    Err("the background figmog kept refusing new requests while shutting down; nothing was applied — rerun the command".into())
}

/// [`write_over`] on an already-connected stream that is never
/// reconnected (tests drive a serve loop or a fake owner directly).
#[cfg(test)]
pub(crate) fn write_over_stream(
    stream: UnixStream,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let route = Route {
        mirror_root: PathBuf::from("/nonexistent/figmog-test-route"),
        local_root: PathBuf::from("/nonexistent/figmog-test-route"),
    };
    let client = Client::new(stream).map_err(|e| e.to_string())?;
    write_over(&route, client, tool, args)
}

fn write_once(client: Client, tool: &str, frame: &str) -> Result<Value, Failure> {
    let Client {
        mut reader,
        mut writer,
    } = client;
    writer
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    writer
        .set_read_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    // A request serve never fully received cannot have been applied.
    writeln!(writer, "{frame}").map_err(|e| format!("writing to serve socket: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("writing to serve socket: {e}"))?;

    let id = json!(1);
    let mut operation: Option<String> = None;
    loop {
        let unknown = |cause: String, operation: &Option<String>| match operation {
            Some(op) => format!(
                "outcome unknown: figmog serve did not report operation {op} ({cause}); it may or may not have been applied — inspect the mirror (figmog status) before retrying"
            ),
            None => format!(
                "outcome unknown: figmog serve did not acknowledge or answer this write ({cause}); it may or may not have been applied — inspect the mirror (figmog status) before retrying"
            ),
        };
        let line = match read_frame(&mut reader) {
            Ok(line) => line,
            Err(e) if e.starts_with("figmog serve did not respond") => {
                return Err(Failure::Error(unknown(
                    format!("no answer within {}s", WRITE_TIMEOUT.as_secs()),
                    &operation,
                )));
            }
            Err(e) => return Err(Failure::Error(unknown(e, &operation))),
        };
        let resp: Value = serde_json::from_str(&line)
            .map_err(|e| unknown(format!("invalid JSON: {e}"), &operation))?;
        if resp.get("method").and_then(Value::as_str)
            == Some(crate::serve::writes::OPERATION_NOTIFICATION)
        {
            operation = resp["params"]["id"].as_str().map(str::to_string);
            continue;
        }
        if is_response_to(&resp, &id) {
            // "Not accepted" means nothing was started, even after an
            // operation id was announced: a serve shutting down answers a
            // write still queued that way (NWS-2963).
            if not_accepted(&resp) {
                return Err(Failure::NotAccepted);
            }
            return Ok(interpret_call_response(tool, resp)?);
        }
    }
}

/// Serve answered "not accepted": it started nothing for this request.
fn not_accepted(resp: &Value) -> bool {
    resp["error"]["code"].as_i64() == Some(owner::NOT_ACCEPTED)
}

fn call_over(client: Client, tool: &str, args: Value) -> Result<Value, Failure> {
    let resp = send_and_recv(
        client,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        }),
    )?;
    Ok(interpret_call_response(tool, resp)?)
}

fn tools_list_over(client: Client) -> Result<Vec<Value>, Failure> {
    let resp = send_and_recv(
        client,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )?;
    if let Some(err) = resp.get("error") {
        return Err(Failure::Error(protocol_error_message(err)));
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
/// skipped until the response to `frame`'s own id arrives (NWS-2959). A
/// "not accepted" answer from a closing owner is [`Failure::NotAccepted`].
fn send_and_recv(client: Client, frame: &Value) -> Result<Value, Failure> {
    let Client {
        mut reader,
        mut writer,
    } = client;
    writer
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    writer
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    writeln!(writer, "{frame}").map_err(|e| format!("writing to serve socket: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("writing to serve socket: {e}"))?;

    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    loop {
        let line = read_frame(&mut reader)?;
        let resp: Value = serde_json::from_str(&line)
            .map_err(|e| format!("serve socket sent invalid JSON: {e}"))?;
        if is_response_to(&resp, &id) {
            if not_accepted(&resp) {
                return Err(Failure::NotAccepted);
            }
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
            Client::new(client).unwrap(),
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .unwrap_or_else(|_| panic!("send_and_recv failed"));
        assert_eq!(resp["result"], json!({"mine": true}));
        serve.join().unwrap();
    }

    /// A write the owner did not accept (it is shutting down) is never
    /// reported as applied or unknown: with no serve left to retry on, the
    /// CLI says nothing was applied (NWS-2953).
    #[test]
    fn a_write_the_owner_did_not_accept_is_retried_or_reported_unapplied() {
        let (client, server) = UnixStream::pair().unwrap();
        let owner = std::thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            let mut out = server;
            writeln!(
                out,
                "{}",
                json!({"jsonrpc": "2.0", "id": request["id"],
                       "error": {"code": owner::NOT_ACCEPTED, "message": "not accepted"}})
            )
            .unwrap();
        });
        let error =
            write_over_stream(client, "figmog_internal_pull", json!({"file": "k"})).unwrap_err();
        assert!(
            error.contains("before accepting this write; nothing was applied"),
            "{error}"
        );
        owner.join().unwrap();
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
