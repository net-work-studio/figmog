#![recursion_limit = "256"]

//! End-to-end test of `figmog serve`: spawns the real compiled binary as a
//! child process, drives it over stdin/stdout exactly as an MCP client
//! would, and asserts on the JSON-RPC frames it writes back. Everything
//! else in this crate tests the pieces (`mcp::handle_message` unit tests,
//! CLI smoke tests over `query::*`); this is the one test proving the
//! pieces are wired together correctly in the real process, including the
//! stdin-EOF exit contract `--no-watch` mode relies on.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Generous but bounded: every wait in this test — for a response line or
/// for the child to exit — is capped at this, so a regression that makes
/// the server hang fails the test instead of the test run.
const TIMEOUT: Duration = Duration::from_secs(10);

fn deep_working_directory() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("a".repeat(90)).join("b".repeat(90));
    std::fs::create_dir_all(&work).unwrap();
    assert!(work.join(".figmog/serve.sock").as_os_str().len() > 128);
    (dir, work)
}

#[test]
fn deep_working_directory_keeps_the_default_socket_address_short() {
    let (_dir, work) = deep_working_directory();
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("figmog"));
    cmd.args([
        "serve",
        "--no-watch",
        "--no-upstream",
        "--figmog-root",
        ".figmog",
    ])
    .current_dir(&work);
    let (mut guard, mut stdin, rx) = spawn_child(cmd);
    handshake(&mut stdin, &rx);
    assert!(work.join(".figmog/serve.sock").exists());
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
    assert!(!work.join(".figmog/serve.sock").exists());
}

#[test]
fn deep_working_directory_preserves_an_existing_live_socket() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;

    let (_dir, work) = deep_working_directory();
    // Bind at a short address, then move the directory. The listening socket
    // stays live at its new pathname, just like an older server that bound
    // `.figmog/serve.sock` relative to the deep working directory.
    let staging = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(staging.path().join("serve.sock")).unwrap();
    std::fs::rename(staging.path(), work.join(".figmog")).unwrap();
    let socket = work.join(".figmog/serve.sock");
    let before = std::fs::metadata(&socket).unwrap();

    let output = common::command()
        .args([
            "serve",
            "--no-watch",
            "--no-upstream",
            "--figmog-root",
            ".figmog",
        ])
        .current_dir(&work)
        .output()
        .unwrap();

    assert!(socket.exists(), "live socket was unlinked: {output:?}");
    let after = std::fs::metadata(&socket).unwrap();
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("another figmog serve owns this root")
    );
    listener.set_nonblocking(true).unwrap();
    assert!(
        listener.accept().is_ok(),
        "probe never reached the live listener"
    );
}

/// Wait until the background owner of `mirror_root` (NWS-2953) has idled
/// out: it removes its discovery record only after closing its stores.
fn wait_for_owner_exit(mirror_root: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while mirror_root.join("owner.json").exists() {
        assert!(
            Instant::now() < deadline,
            "the background owner never exited"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The background owner's pid, from its discovery record.
fn owner_pid(mirror_root: &std::path::Path) -> u64 {
    let record: Value =
        serde_json::from_slice(&std::fs::read(mirror_root.join("owner.json")).unwrap()).unwrap();
    record["pid"].as_u64().unwrap()
}

/// Kills the child on drop so a failed assertion (which unwinds past the
/// rest of the test body, skipping the normal stdin-close/wait sequence)
/// never leaves an orphaned `figmog serve` process behind.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `figmog serve --no-watch --db <db>` with piped stdio. Returns the
/// kill-on-drop guard, a writer for stdin, and a channel of stdout lines
/// fed by a reader thread — driving the child through a channel (rather
/// than reading its stdout inline) means a hung child blocks only the
/// bounded `recv_timeout` in [`recv`], never the test thread itself.
fn spawn_serve(db: &std::path::Path) -> (ChildGuard, ChildStdin, Receiver<String>) {
    spawn_serve_with_args(db, &["--no-upstream"])
}

/// Like [`spawn_serve`], but with extra CLI args after `--db <db>` (e.g.
/// `--upstream <url>` for the proxy e2e test).
///
/// `current_dir` is pinned to `db`'s own tempdir (every caller's `db` is
/// `<tempdir>/db` — see `common::fixture_db`): `figmog serve` binds its
/// unix-socket control plane (spec §1) at `<figmog-root>/serve.sock`, and
/// `--figmog-root` defaults to the cwd-relative `.figmog` regardless of
/// `--db` — without this, every one of this file's `--db`-mode e2e tests
/// (which never sets its own `current_dir`) would default to the *shared*
/// `cargo test` process cwd, racing every other concurrently-running test
/// in this binary to bind the exact same socket path.
fn spawn_serve_with_args(
    db: &std::path::Path,
    extra_args: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--db"])
        .arg(db)
        .args(extra_args)
        .current_dir(db.parent().expect("db path has a parent tempdir"));
    spawn_child(cmd)
}

/// Spawn `figmog serve --no-watch --no-upstream --figmog-root <root>
/// <files...>` (spec §14's multi-file surface, with the hidden
/// `--figmog-root` testability flag pointed at a pre-built fixture root),
/// with `FIGMA_TOKEN` scrubbed from the child's environment — every
/// multi-file e2e that touches `figmog_open` needs the missing-token
/// isError, not whatever real token the test runner's own shell happens to
/// export.
fn spawn_serve_multifile(
    root: &std::path::Path,
    files: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--no-upstream", "--figmog-root"])
        .arg(root)
        .args(files)
        .env("FIGMA_TOKEN", "");
    spawn_child(cmd)
}

/// Shared plumbing behind every `spawn_serve*` helper: pipe stdio, spawn,
/// drain stderr for debugging visibility (never asserted on), and feed
/// stdout lines into a channel — driving the child through a channel
/// (rather than reading its stdout inline) means a hung child blocks only
/// the bounded `recv_timeout` in [`recv`], never the test thread itself.
fn spawn_child(mut cmd: Command) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let mut child = cmd
        .env("FIGMA_TOKEN", "")
        .env("FIGMOG_OWNER_IDLE_MS", "1000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn figmog serve");

    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let stderr = child.stderr.take().expect("child stderr");

    // Drain stderr on its own thread purely for debugging visibility
    // (`serve` logs there, e.g. "figmog serving ..."); never asserted on.
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[figmog serve stderr] {line}");
        }
    });

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    (ChildGuard(child), stdin, rx)
}

/// Pre-build one fixture store per `(key, fixture)` pair under a fresh temp
/// `--figmog-root` layout — `<root>/<key>/db`, exactly the path
/// [`sessions::open_session`](../src/sessions.rs) derives (spec §14) — each
/// via `figmog pull --from-file`, so the multi-file `serve` e2e can start
/// against pre-populated mirrors without ever touching the network. Returns
/// the tempdir; the caller must keep it alive for the duration of the test.
fn build_fixture_root(entries: &[(&str, Value)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (key, fixture) in entries {
        let resp = dir.path().join(format!("{key}.json"));
        std::fs::write(&resp, serde_json::to_string(fixture).unwrap()).unwrap();
        let db = dir.path().join(key).join("db");
        common::command()
            .args(["pull", "--from-file"])
            .arg(&resp)
            .arg("--db")
            .arg(&db)
            .assert()
            .success();
    }
    dir
}

/// Bare file keys (spec §14: 10+ alphanumeric chars — see `ident::parse_file_ref`),
/// deliberately readable rather than realistic, for the multi-file e2e
/// tests below. `KEY_A` mirrors [`common::fixture_v1`] and is always the
/// first startup file (so the default-routing rule picks it); `KEY_B`
/// mirrors [`common::fixture_other`].
const KEY_A: &str = "figmogkeyoneaaaa1111";
const KEY_B: &str = "figmogkeytwobbbb2222";

/// Write one JSON-RPC frame, newline-delimited (the protocol this crate's
/// `mcp`/`serve` modules speak).
fn send(stdin: &mut ChildStdin, msg: &Value) {
    writeln!(stdin, "{msg}").expect("write to child stdin");
    stdin.flush().expect("flush child stdin");
}

/// Read and parse the next response line, bounded by [`TIMEOUT`] so a
/// stuck server fails this assertion instead of hanging the test binary.
fn recv(rx: &Receiver<String>) -> Value {
    recv_within(rx, TIMEOUT)
}

/// Like [`recv`], but with a caller-supplied bound instead of [`TIMEOUT`] —
/// for the one scenario (a wedged socket client) whose fix is a *bounded*,
/// but deliberately longer-than-[`TIMEOUT`], delay rather than an instant
/// response, so that test needs its own wider window without loosening
/// every other test's.
fn recv_within(rx: &Receiver<String>, timeout: Duration) -> Value {
    let line = rx
        .recv_timeout(timeout)
        .expect("figmog serve did not respond within the timeout");
    serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("response line was not valid JSON: {e}\nline: {line}"))
}

/// Poll `try_wait` instead of a single blocking `wait()`, so a child that
/// never exits fails with a clear panic at `timeout` rather than hanging
/// the test run forever.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("figmog serve did not exit within {timeout:?} of stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn call(stdin: &mut ChildStdin, rx: &Receiver<String>, id: i64, name: &str, args: Value) -> Value {
    send(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": args},
        }),
    );
    recv(rx)
}

/// The tool result's text content, parsed as JSON (every `figmog_*` tool
/// returns `query::*` JSON serialized as the single text content block).
fn result_json(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in: {resp}"));
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("content text not JSON: {e}\ntext: {text}"))
}

#[test]
fn serve_e2e_initialize_tools_list_and_tool_calls() {
    let (_dir, db) = common::fixture_db();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);

    // -- initialize --
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    assert_eq!(resp["result"]["serverInfo"]["name"], json!("figmog"));
    let instructions = resp["result"]["instructions"]
        .as_str()
        .expect("instructions is a string");
    assert!(!instructions.is_empty());
    assert!(
        instructions.contains("forwarded uncached"),
        "instructions should describe live desktop forwarding: {instructions}"
    );

    // notifications/initialized: no `id`, so no response frame is expected
    // (mirrors a real MCP client's handshake; the server ignores it).
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // -- tools/list: exactly 21 figmog_* tools (spec §14: the 17 v3 tools
    // plus figmog_open/figmog_files, plus v0.0.2 §2's figmog_subtree and
    // §5's figmog_images) --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let resp = recv(&rx);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 21, "tools: {tools:#?}");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for name in &names {
        assert!(
            name.starts_with("figmog_"),
            "tool outside the figmog_ namespace: {name}"
        );
    }
    for expected in ["figmog_search", "figmog_where", "figmog_sync"] {
        assert!(names.contains(&expected), "missing tool: {expected}");
    }

    // -- figmog_search: first hit is 1:2 ("Title", text "...garden") --
    let resp = call(
        &mut stdin,
        &rx,
        3,
        "figmog_search",
        json!({"query": "garden"}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let hits = result_json(&resp);
    assert_eq!(hits[0]["id"], json!("1:2"));

    // -- figmog_node: id normalization (12-34 form) + raw JSON name --
    let resp = call(&mut stdin, &rx, 4, "figmog_node", json!({"id": "1-2"}));
    assert_eq!(resp["result"]["isError"], json!(false));
    let node = result_json(&resp);
    assert_eq!(node["name"], json!("Title"));

    // -- figmog_where: exactly one row, id 1:1 --
    let resp = call(
        &mut stdin,
        &rx,
        5,
        "figmog_where",
        json!({"pointer": "/layoutMode", "equals": "VERTICAL"}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let rows = result_json(&resp);
    let rows = rows.as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], json!("1:1"));

    // -- figmog_node on an unknown id: isError --
    let resp = call(&mut stdin, &rx, 6, "figmog_node", json!({"id": "99:99"}));
    assert_eq!(resp["result"]["isError"], json!(true));

    // -- unknown JSON-RPC method: -32601 --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 7, "method": "totally/bogus"}),
    );
    let resp = recv(&rx);
    assert_eq!(resp["error"]["code"], json!(-32601));

    // -- unknown tool name: isError, not a protocol-level error --
    let resp = call(&mut stdin, &rx, 8, "figmog_nonexistent", json!({}));
    assert_eq!(resp["result"]["isError"], json!(true));

    // -- figmog_node with a malformed percent-escape (`%` + one byte + a
    // multibyte UTF-8 char) in a node-id URL: isError, not a process death
    // (F1 regression — `ident::percent_decode` used to panic slicing a
    // multibyte char mid-byte, and this loop has no per-call unwind guard,
    // so one malformed frame used to abort the whole server). Answering
    // this call at all — rather than the client's `recv` timing out because
    // the process died — is the assertion; the exact error text isn't
    // pinned.
    let resp = call(
        &mut stdin,
        &rx,
        9,
        "figmog_node",
        json!({"id": "https://www.figma.com/file/abc/x?node-id=%aé"}),
    );
    assert_eq!(resp["result"]["isError"], json!(true));

    // -- NWS-2944: wrong-typed optional args are tool errors (isError, the
    // same shape as a missing required field), never silently "absent" --
    for (id, tool, args, want) in [
        (
            10,
            "figmog_subtree",
            json!({"id": "1:1", "depth": "2"}),
            "expected integer for `depth`, got string",
        ),
        (
            11,
            "figmog_node",
            json!({"id": "1:2", "file": 12}),
            "expected string for `file`, got number",
        ),
        (
            12,
            "figmog_images",
            json!({"ids": ["1:2"], "format": 1}),
            "expected string for `format`, got number",
        ),
    ] {
        let resp = call(&mut stdin, &rx, id, tool, args);
        assert_eq!(resp["result"]["isError"], json!(true), "{tool}: {resp}");
        assert_eq!(resp["result"]["content"][0]["text"], json!(want), "{tool}");
    }

    // Closing stdin is what makes the (`--no-watch`) serve loop exit: its
    // reader thread sees EOF and drops the sender, so the main loop's
    // blocking `rx.recv()` returns `Disconnected` and the process exits 0.
    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// I-1: while `figmog serve` holds the store's single-writer lock, a CLI
/// command opened against the same `--db` must still exit 1 with figmog's
/// friendly locked-store message on stderr — never fold's raw `unwrap()`
/// panic *exit code* (101). Reproduces the review's live repro (serve
/// holding a fixture store, `figmog status --db <same>` in a second
/// process) as an automated test.
///
/// `open_store_checked` (see its doc comment in `cli/mod.rs`) suppresses the
/// default panic hook for the call, so fold's raw `thread 'main'
/// panicked… ` banner must never reach stderr at all — this test proves
/// that directly by parsing the *entire* stderr buffer as one JSON object
/// (`serde_json::from_slice`, not a substring `contains` check): any
/// leaked banner text before or after the JSON line would make the whole
/// buffer fail to parse.
#[test]
fn cli_read_against_a_store_serve_holds_fails_clean_not_with_a_panic() {
    let (_dir, db) = common::fixture_db();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);

    // Complete the handshake before touching the store from a second
    // process: `run_serve` opens the store synchronously, before it can
    // ever respond to `initialize` (see serve.rs), so a response here
    // proves the lock is already held.
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["result"]["serverInfo"]["name"], json!("figmog"));

    let out = common::command()
        .args(["status", "--db"])
        .arg(&db)
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected a clean exit-1, not fold's raw panic exit (101); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr must be exactly one JSON object with no leaked panic banner \
             (parse error: {e}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let error = parsed["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an \"error\" string field, got: {parsed}"));
    assert!(
        error.contains("store is locked"),
        "expected the locked-store message, got: {error}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

#[test]
fn fresh_pull_preserves_a_store_owned_by_serve() {
    let (dir, db) = common::fixture_db();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);
    handshake(&mut stdin, &rx);
    let before = common::store_snapshot(&db);
    let input = dir.path().join("fresh.json");

    for (response, expected_error) in [
        (common::fixture_other(), "store is locked"),
        (
            json!({"name": "Synthetic", "version": "1", "document": {}}),
            "lastModified",
        ),
    ] {
        std::fs::write(&input, response.to_string()).unwrap();
        let out = common::command()
            .args(["pull", "--fresh", "--from-file"])
            .arg(&input)
            .arg("--db")
            .arg(&db)
            .assert()
            .failure()
            .code(1);
        let error: Value = serde_json::from_slice(&out.get_output().stderr).unwrap();
        assert!(
            error["error"].as_str().unwrap().contains(expected_error),
            "{error}"
        );
        assert_eq!(common::store_snapshot(&db), before);
    }

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 20, "method": "tools/call",
            "params": {"name": "figmog_status", "arguments": {}}
        }),
    );
    let response = recv(&rx);
    let status: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["name"], "Fixture");
    assert_eq!(status["nodes"], 12);

    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());

    // The permanent ownership anchor must release its OS lock when serve exits.
    std::fs::write(&input, common::fixture_other().to_string()).unwrap();
    common::command()
        .args(["pull", "--fresh", "--from-file"])
        .arg(&input)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();
}

// ---- desktop-proxy e2e: figmog against an in-process HTTP fake ----
//
// Scripted desktop responses model active-file switches and revisions without
// deriving fixtures from real files or exposing context to the proxy.

fn read_request(stream: &mut TcpStream) -> String {
    // macOS accepted sockets inherit a nonblocking listener's mode.
    stream.set_nonblocking(false).unwrap();
    let mut header_bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("read request byte");
        header_bytes.push(byte[0]);
        if header_bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_text = String::from_utf8_lossy(&header_bytes).to_string();
    let content_length: usize = header_text
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 {
        stream
            .read_exact(&mut body_bytes)
            .expect("read request body");
    }
    String::from_utf8_lossy(&body_bytes).to_string()
}

fn write_response(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut resp = format!("HTTP/1.1 {status}\r\n");
    resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
    resp.push_str("Connection: close\r\n");
    for (k, v) in headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str("\r\n");
    resp.push_str(body);
    stream.write_all(resp.as_bytes()).expect("write response");
    stream.flush().expect("flush response");
}

/// Serve scripted calls, either on one persistent serve connection or with
/// a fresh handshake for each direct CLI invocation. All accepts are bounded.
fn spawn_fake_upstream(
    results: Vec<Value>,
    handshake_each_call: bool,
) -> (String, std::thread::JoinHandle<()>) {
    spawn_fake_upstream_with_args(results, handshake_each_call, json!({"nodeId": "1:2"}))
}

fn spawn_fake_upstream_with_args(
    results: Vec<Value>,
    handshake_each_call: bool,
    expected_args: Value,
) -> (String, std::thread::JoinHandle<()>) {
    spawn_scripted_upstream(
        results.into_iter().map(DesktopReply::Result).collect(),
        handshake_each_call,
        expected_args,
    )
}

#[derive(Clone)]
enum DesktopReply {
    Result(Value),
    ProtocolError(&'static str),
    Disconnect,
}

fn spawn_scripted_upstream(
    results: Vec<DesktopReply>,
    handshake_each_call: bool,
    expected_args: Value,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = std::thread::spawn(move || {
        // This synthetic desktop accepts only the scripted argument keys;
        // the call assertion below also checks their exact values.
        let properties: serde_json::Map<String, Value> = expected_args
            .as_object()
            .unwrap()
            .keys()
            .map(|key| (key.clone(), json!({})))
            .collect();
        listener.set_nonblocking(true).unwrap();
        let mut results = results.into_iter().peekable();
        let mut initialized = false;
        let mut notified = false;
        let mut ready = false;
        let mut called_since_handshake = false;
        let mut request = 0;
        while results.peek().is_some() {
            let deadline = Instant::now() + TIMEOUT;
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "upstream request {request} never arrived"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            let body = read_request(&mut stream);
            let message: Value = serde_json::from_str(&body).unwrap();
            let id = message["id"].clone();
            // Desktop re-detection may ping, refresh tools, or re-handshake
            // between scripted calls. Only tools/call consumes a reply.
            match message["method"].as_str().unwrap() {
                "initialize" => {
                    initialized = true;
                    notified = false;
                    ready = false;
                    called_since_handshake = false;
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "fake-figma-desktop", "version": "1.0"},
                        },
                    })
                    .to_string();
                    write_response(
                        &mut stream,
                        "200 OK",
                        &[("Content-Type", "application/json")],
                        &resp,
                    );
                }
                "notifications/initialized" => {
                    assert!(initialized, "notification before initialize");
                    notified = true;
                    write_response(&mut stream, "202 Accepted", &[], "");
                }
                "tools/list" => {
                    assert!(notified, "tools/list before handshake completed");
                    ready = true;
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"tools": [
                            {
                                "name": "get_code",
                                "description": "Returns code for a node",
                                "inputSchema": {"type": "object", "properties": properties, "additionalProperties": false},
                            },
                        ]},
                    })
                    .to_string();
                    write_response(
                        &mut stream,
                        "200 OK",
                        &[("Content-Type", "application/json")],
                        &resp,
                    );
                }
                "ping" => {
                    assert!(ready, "ping before handshake completed");
                    write_response(
                        &mut stream,
                        "200 OK",
                        &[("Content-Type", "application/json")],
                        &json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string(),
                    );
                }
                "tools/call" => {
                    assert!(ready, "tools/call before handshake completed");
                    assert!(
                        !handshake_each_call || !called_since_handshake,
                        "direct CLI calls must each perform a fresh handshake"
                    );
                    called_since_handshake = true;
                    assert_eq!(message["params"]["name"], "get_code");
                    assert_eq!(message["params"]["arguments"], expected_args);
                    let mut resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                    });
                    match results.next().expect("scripted reply") {
                        DesktopReply::Result(result) => resp["result"] = result,
                        DesktopReply::ProtocolError(message) => {
                            resp["error"] = json!({"code": -32603, "message": message});
                        }
                        DesktopReply::Disconnect => continue,
                    }
                    write_response(
                        &mut stream,
                        "200 OK",
                        &[("Content-Type", "application/json")],
                        &resp.to_string(),
                    );
                }
                method => panic!("unexpected upstream method: {method}"),
            }
            request += 1;
        }
    });
    (format!("http://{addr}/mcp"), handle)
}

#[test]
fn scripted_desktop_maintenance_preserves_call_results() {
    for handshake_each_call in [false, true] {
        let args = json!({"nodeId": "1:2"});
        let results: Vec<Value> = (0..3)
            .map(|i| json!({"content": [{"type": "text", "text": format!("reply {i}")}]}))
            .collect();
        let (address, upstream) =
            spawn_fake_upstream_with_args(results.clone(), handshake_each_call, args.clone());
        let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
        let post = |method: &str, params: Value| {
            let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
            let text = agent
                .post(&address)
                .set("Content-Type", "application/json")
                .send_bytes(body.to_string().as_bytes())
                .unwrap()
                .into_string()
                .unwrap();
            if text.is_empty() {
                Value::Null
            } else {
                let response: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(response["id"], 1);
                response["result"].clone()
            }
        };
        let handshake = || {
            let initialized = post("initialize", json!({}));
            assert_eq!(initialized["protocolVersion"], "2025-06-18");
            assert_eq!(post("notifications/initialized", json!({})), Value::Null);
            assert_eq!(
                post("tools/list", json!({}))["tools"][0]["name"],
                "get_code"
            );
        };
        handshake();
        for (i, expected) in results.iter().enumerate() {
            // Exercise an ordinary live refresh, plus a recovery handshake.
            // Neither may consume the next scripted tools/call result.
            if i == 2 || (i > 0 && handshake_each_call) {
                handshake();
            }
            assert_eq!(post("ping", json!({})), json!({}));
            assert_eq!(
                post("tools/list", json!({}))["tools"][0]["name"],
                "get_code"
            );
            assert_eq!(
                post("tools/call", json!({"name": "get_code", "arguments": args})),
                *expected
            );
        }
        upstream.join().unwrap();
    }
}

#[test]
fn serve_desktop_proxy_observes_file_switches_and_edits_with_unchanged_mirror() {
    let (_dir, db) = common::fixture_db();
    let results = desktop_results();
    let (fake_addr, fake_handle) = spawn_fake_upstream(results.clone(), false);
    let (mut guard, mut stdin, rx) = spawn_serve_with_args(&db, &["--upstream", &fake_addr]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));

    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // -- tools/list: 21 local + 1 proxied, prefixed description --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let resp = recv(&rx);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 22, "tools: {tools:#?}");
    let proxied = tools
        .iter()
        .find(|t| t["name"] == json!("get_code"))
        .expect("get_code should be in the merged registry");
    assert_eq!(
        proxied["description"],
        json!("[via Figma desktop] Returns code for a node")
    );

    // -- figmog_status: upstream connected --
    let resp = call(&mut stdin, &rx, 3, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false));
    assert_eq!(result_json(&resp)["upstream"], json!("connected"));

    // Mirror A stays at version 100 throughout. Desktop switches B -> C,
    // then C is edited. Identical tool arguments must reach upstream each time.
    for (i, expected) in results.iter().enumerate() {
        let response = call(
            &mut stdin,
            &rx,
            4 + i as i64,
            "get_code",
            json!({"nodeId": "1:2"}),
        );
        assert_eq!(&response["result"], expected, "desktop context change {i}");
    }

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");

    fake_handle
        .join()
        .expect("every scripted desktop call should reach upstream");
    let st = figmog::open_store!(&db);
    st.rtx(|(_, _, _, _, _, _, meta, cache, _, _, _, _)| {
        assert_eq!(meta.get(&0).unwrap().version, "100");
        assert!(
            cache.iter().next().is_none(),
            "unknown desktop context must not populate the cache"
        );
    });
}

fn desktop_results() -> Vec<Value> {
    [
        "file B revision 1",
        "file C revision 1",
        "file C revision 2",
    ]
    .into_iter()
    .map(|text| json!({"content": [{"type": "text", "text": text}], "isError": false}))
    .collect()
}

#[test]
fn serve_forwards_deep_arguments_to_http_desktop_without_recursive_copies() {
    let (_dir, db) = common::fixture_db();
    let payload = format!(
        "{}null{}",
        r#"{"layer":["#.repeat(30_000),
        "]}".repeat(30_000)
    );
    let expected_payload = payload.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}/mcp", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        for method in [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call",
        ] {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "waiting for {method}");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            let body = read_request(&mut stream);
            // Check the entire payload byte-for-byte without constructing a
            // recursive Value in the test peer, then inspect the shallow frame.
            let body = if method == "tools/call" {
                assert_eq!(body.matches(&expected_payload).count(), 1);
                body.replacen(&expected_payload, "null", 1)
            } else {
                body
            };
            let request: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(request["method"], method);
            let result = match method {
                "initialize" => json!({
                    "protocolVersion":"2025-06-18", "capabilities":{},
                    "serverInfo":{"name":"synthetic-desktop", "version":"1"}
                }),
                "notifications/initialized" => {
                    write_response(&mut stream, "202 Accepted", &[], "");
                    continue;
                }
                "tools/list" => json!({"tools":[{
                    "name":"get_code", "description":"Synthetic deep argument probe",
                    "inputSchema":{"type":"object"}
                }]}),
                "tools/call" => {
                    assert_eq!(request["params"]["name"], "get_code");
                    assert_eq!(request["params"]["arguments"], json!({"payload":null}));
                    json!({"content":[{"type":"text", "text":"received deep payload"}], "isError":false})
                }
                _ => unreachable!(),
            };
            let response = json!({"jsonrpc":"2.0", "id":request["id"], "result":result});
            write_response(
                &mut stream,
                "200 OK",
                &[("Content-Type", "application/json")],
                &response.to_string(),
            );
        }
    });
    let (mut guard, mut stdin, rx) = spawn_serve_with_args(&db, &["--upstream", &address]);
    handshake(&mut stdin, &rx);
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"get_code","arguments":{{"payload":{payload}}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let response = recv(&rx);
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(
        response["result"]["content"][0]["text"],
        "received deep payload"
    );
    // The same serve process must remain available for its local mirror.
    let status = call(&mut stdin, &rx, 3, "figmog_status", json!({}));
    assert_eq!(result_json(&status)["nodes"], 12);
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
    peer.join().unwrap();
}

#[test]
fn direct_cli_desktop_proxy_ignores_legacy_cache_and_observes_context_changes() {
    let (_dir, db) = common::fixture_db();
    let legacy =
        json!({"content": [{"type": "text", "text": "legacy wrong file"}], "isError": false});
    let args = r#"{"nodeId":"1:2"}"#;
    {
        let mut st = figmog::open_store!(&db);
        figmog::cache::store(&mut st, "get_code", args, "100", &legacy).unwrap();
    }
    let results = desktop_results();
    let (fake_addr, fake_handle) = spawn_fake_upstream(results.clone(), true);
    let work = tempfile::tempdir().unwrap();
    for expected in results {
        let output = common::command()
            .current_dir(work.path())
            .args([
                "call",
                "get_code",
                "--args",
                args,
                "--db",
                db.to_str().unwrap(),
                "--upstream",
                &fake_addr,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            result, expected,
            "legacy or prior desktop responses must never be reused"
        );
    }
    fake_handle
        .join()
        .expect("every direct call should reach upstream");
    let st = figmog::open_store!(&db);
    st.rtx(|(_, _, _, _, _, _, meta, cache, _, _, _, _)| {
        assert_eq!(meta.get(&0).unwrap().version, "100");
        assert_eq!(
            figmog::cache::lookup(&cache, "get_code", args, "100"),
            Some(legacy)
        );
    });
}

#[test]
fn desktop_proxy_cli_results_are_verbatim_and_byte_identical_across_transports() {
    const KEY: &str = "figmogproxyresult001";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let args = json!({"nodeId": "1:2", "file": KEY});
    let results = vec![
        json!({
            "content": [
                {"type": "text", "text": "<Frame>synthetic code</Frame>"},
                {"type": "text", "text": "const label = 'Synthetic';\n"},
            ],
            "isError": false,
        }),
        json!({
            "content": [
                {"type": "text", "text": " { \"code\" : \"synthetic\" }\n"},
                {"type": "image", "data": "YQ==", "mimeType": "image/png"},
                {"type": "resource", "resource": {
                    "uri": "synthetic:///code", "mimeType": "text/plain", "text": "Synthetic resource",
                }},
            ],
            "structuredContent": {"code": "synthetic", "nodes": ["1:2"]},
            "isError": false,
            "_meta": {"synthetic": true},
        }),
        json!({
            "content": [
                {"type": "text", "text": "Synthetic tool failure"},
                {"type": "text", "text": "Additional error details"},
            ],
            "structuredContent": {"reason": "synthetic"},
            "isError": true,
        }),
        json!({
            "content": [
                {"type": "image", "data": "YQ==", "mimeType": "image/png"},
                {"type": "resource_link", "uri": "synthetic:///code", "name": "Code"},
            ],
        }),
        json!({"content": [], "structuredContent": {"code": "synthetic"}}),
    ];
    let run = |flags: &[&str]| {
        common::command()
            .current_dir(dir.path())
            .args(["call", "get_code", "--args", &args.to_string()])
            .args(flags)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    let (address, upstream) = spawn_fake_upstream_with_args(results.clone(), true, args.clone());
    let direct: Vec<Vec<u8>> = results
        .iter()
        .map(|expected| {
            let stdout = run(&["--no-socket", "--upstream", &address]);
            let result: Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(&result, expected);
            stdout
        })
        .collect();
    upstream.join().unwrap();

    let (address, upstream) = spawn_fake_upstream_with_args(results.clone(), false, args.clone());
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("figmog"));
    command
        .args(["serve", "--no-watch", "--upstream", &address, KEY])
        .current_dir(dir.path());
    let (mut guard, mut stdin, rx) = spawn_child(command);
    handshake(&mut stdin, &rx);
    for (expected, direct_stdout) in results.iter().zip(&direct) {
        let stdout = run(&[]);
        let result: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(&result, expected);
        assert_eq!(&stdout, direct_stdout);
    }
    upstream.join().unwrap();
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

fn assert_proxy_cli_forwarding_failure(reply: DesktopReply, message_prefix: &str) {
    const KEY: &str = "figmogproxyfailure01";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let args = json!({"nodeId": "1:2", "file": KEY});
    let (address, upstream) =
        spawn_scripted_upstream(vec![reply.clone(), reply], true, args.clone());
    let run = |flags: &[&str]| {
        let output = common::command()
            .current_dir(dir.path())
            .args(["call", "get_code", "--args", &args.to_string()])
            .args(flags)
            .assert()
            .failure()
            .code(1)
            .get_output()
            .clone();
        assert!(output.stdout.is_empty(), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(error["error"].as_str().unwrap().starts_with(message_prefix));
        output
    };
    let direct = run(&["--no-socket", "--upstream", &address]);

    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("figmog"));
    command
        .args(["serve", "--no-watch", "--upstream", &address, KEY])
        .current_dir(dir.path());
    let (mut guard, mut stdin, rx) = spawn_child(command);
    handshake(&mut stdin, &rx);
    let socket = run(&[]);
    assert_eq!(socket.stderr, direct.stderr);
    upstream.join().unwrap();
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn desktop_proxy_cli_protocol_errors_fail_on_both_transports() {
    assert_proxy_cli_forwarding_failure(
        DesktopReply::ProtocolError("Synthetic protocol failure"),
        "upstream protocol error: Synthetic protocol failure",
    );
}

#[test]
fn desktop_proxy_cli_transport_errors_fail_on_both_transports() {
    assert_proxy_cli_forwarding_failure(DesktopReply::Disconnect, "upstream unreachable:");
}

#[test]
fn desktop_proxy_cli_unknown_and_unavailable_tools_fail_on_both_transports() {
    const KEY: &str = "figmogproxyunknown01";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let run = |tool: &str, flags: &[&str]| {
        let output = common::command()
            .current_dir(dir.path())
            .args(["call", tool])
            .args(flags)
            .assert()
            .failure()
            .code(1)
            .get_output()
            .clone();
        assert!(output.stdout.is_empty(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stderr).unwrap()
    };
    for tool in ["nonexistent", "get_code"] {
        let error = run(tool, &["--no-socket", "--no-upstream"]);
        assert_eq!(error["error"], format!("upstream not attached: {tool}"));
    }

    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);
    for tool in ["nonexistent", "get_code"] {
        let error = run(tool, &[]);
        assert_eq!(error["error"], format!("unknown tool: {tool}"));
    }
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

/// `--no-upstream` disables desktop re-detection too (NWS-2959): neither
/// startup nor a call to a desktop tool name ever contacts the desktop
/// address, and status stays `disabled`.
#[test]
fn no_upstream_serve_never_probes_the_desktop() {
    let (_dir, db) = common::fixture_db();
    let desktop = TcpListener::bind("127.0.0.1:0").unwrap();
    desktop.set_nonblocking(true).unwrap();
    let url = format!("http://{}/mcp", desktop.local_addr().unwrap());
    let (mut guard, mut stdin, rx) =
        spawn_serve_with_args(&db, &["--no-upstream", "--upstream", &url]);
    handshake(&mut stdin, &rx);
    let resp = call(&mut stdin, &rx, 2, "get_code", json!({}));
    assert_eq!(
        resp["result"]["content"][0]["text"],
        "unknown tool: get_code"
    );
    let resp = call(&mut stdin, &rx, 3, "figmog_status", json!({}));
    assert_eq!(result_json(&resp)["upstream"], "disabled");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        desktop.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "--no-upstream must never connect to the desktop"
    );
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

// ---- multi-file serve e2e (spec §14) ----
//
// Two pre-built stores under a temp `--figmog-root` (built via `pull
// --from-file --db <root>/<key>/db`, never touching the network), started
// with BOTH keys as positional args, proves the whole v4 surface: 20 tools,
// every local tool's optional `file` schema property, `figmog_files`,
// `file`-argument routing to a *specific* mirror, default-file routing on
// an omitted `file`, and `figmog_open`'s isError on a missing token. A
// second spawn with zero startup files covers the omitted-`file`-with-no-
// default error text a single default file can never trigger.

#[test]
fn serve_e2e_multi_file_routes_by_file_arg_and_first_startup_key_is_default() {
    let root = build_fixture_root(&[
        (KEY_A, common::fixture_v1()),
        (KEY_B, common::fixture_other()),
    ]);
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[KEY_A, KEY_B]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // -- tools/list: 21 tools; every tool but figmog_open/figmog_files
    // carries an *optional* `file` property, figmog_open's `file` is
    // required, figmog_files takes none (spec §14). --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let resp = recv(&rx);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 21, "tools: {tools:#?}");
    for tool in tools {
        let name = tool["name"].as_str().expect("tool name");
        let schema = &tool["inputSchema"];
        let required: Vec<Value> = schema["required"].as_array().cloned().unwrap_or_default();
        match name {
            "figmog_open" => {
                assert!(
                    schema["properties"]["file"].is_object(),
                    "figmog_open should take a file property"
                );
                assert!(
                    required.contains(&json!("file")),
                    "figmog_open's file should be required"
                );
            }
            "figmog_files" => {
                assert!(
                    schema["properties"].get("file").is_none(),
                    "figmog_files should not take a file argument"
                );
            }
            _ => {
                assert!(
                    schema["properties"]["file"].is_object(),
                    "{name} is missing the optional file routing property"
                );
                assert!(
                    !required.contains(&json!("file")),
                    "{name}'s file property must be optional"
                );
            }
        }
    }

    // -- figmog_files: both mirrors, in open order, KEY_A (first startup
    // FILE) is the default. --
    let resp = call(&mut stdin, &rx, 3, "figmog_files", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false));
    let rows = result_json(&resp);
    let rows = rows.as_array().expect("files array");
    assert_eq!(rows.len(), 2, "files: {rows:#?}");
    assert_eq!(rows[0]["key"], json!(KEY_A));
    assert_eq!(rows[0]["name"], json!("Fixture"));
    assert_eq!(rows[0]["default"], json!(true));
    assert_eq!(rows[1]["key"], json!(KEY_B));
    assert_eq!(rows[1]["name"], json!("OtherFixture"));
    assert_eq!(rows[1]["default"], json!(false));

    // -- figmog_search {query: "zephyr", file: KEY_B} hits: "zephyr" only
    // appears in fixture_other's one TEXT node, so a hit here proves the
    // `file` argument actually reached the *other* mirror. --
    let resp = call(
        &mut stdin,
        &rx,
        4,
        "figmog_search",
        json!({"query": "zephyr", "file": KEY_B}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let hits = result_json(&resp);
    let hits = hits.as_array().expect("hits array");
    assert!(!hits.is_empty(), "expected a 'zephyr' hit in {KEY_B}");
    assert_eq!(hits[0]["id"], json!("1:1"));

    // -- same query with `file` omitted routes to the default (KEY_A /
    // fixture_v1, which never mentions "zephyr" anywhere) and misses —
    // proving omission really does route to the default session rather
    // than reusing whichever mirror answered the previous call. --
    let resp = call(
        &mut stdin,
        &rx,
        5,
        "figmog_search",
        json!({"query": "zephyr"}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let hits = result_json(&resp);
    assert!(
        hits.as_array().expect("hits array").is_empty(),
        "default file should have no 'zephyr' hits: {hits:?}"
    );

    // -- figmog_status {file: KEY_B}: the *other* file's own name. --
    let resp = call(&mut stdin, &rx, 6, "figmog_status", json!({"file": KEY_B}));
    assert_eq!(resp["result"]["isError"], json!(false));
    assert_eq!(result_json(&resp)["name"], json!("OtherFixture"));

    // -- figmog_open {file: "garbagekey1234567890"}: a brand-new key
    // opens a session and acknowledges a background job. Its eventual
    // credential failure is reported through status/files instead of
    // delaying this acknowledgement (spawn_serve_multifile is offline). --
    let resp = call(
        &mut stdin,
        &rx,
        7,
        "figmog_open",
        json!({"file": "garbagekey1234567890"}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    assert_eq!(result_json(&resp)["accepted"], true);

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Zero startup files (spec §14: valid, token-free, idle startup) — the
/// only shape in which the omitted-`file`-with-no-default error is
/// triggerable at all (a single startup file, or an established default,
/// always resolves the omitted case; two auto-opened mirrors are covered
/// by `sessions.rs`'s own unit tests). Proves the error names both new
/// tools, per spec §14's resolution rule.
#[test]
fn serve_e2e_multi_file_zero_startup_omitted_file_errors_naming_figmog_open() {
    let root = tempfile::tempdir().unwrap();
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // figmog_files: nothing mirrored yet.
    let resp = call(&mut stdin, &rx, 2, "figmog_files", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false));
    assert_eq!(result_json(&resp), json!([]));

    // A tool call with `file` omitted and no default mirrored file:
    // isError naming figmog_open/figmog_files, not a silent empty answer.
    let resp = call(&mut stdin, &rx, 3, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("figmog_open"),
        "error should name figmog_open: {text}"
    );
    assert!(
        text.contains("figmog_files"),
        "error should name figmog_files: {text}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// spec §2b: a full Figma frame URL works directly as `id` on
/// `figmog_node`/`figmog_subtree`, and — with no explicit `file` argument
/// and no startup default (zero startup files here) — the URL's own file
/// key auto-opens the right mirror (spec §14's existing auto-open
/// semantics), exactly as if `file: KEY_A` had been passed explicitly.
/// The mirror's store already exists on disk (pre-built by
/// `build_fixture_root`), so this never touches the network.
#[test]
fn serve_e2e_zero_startup_url_id_infers_the_file_and_resolves_the_node() {
    let root = build_fixture_root(&[(KEY_A, common::fixture_v1())]);
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // Nothing mirrored yet — the store on disk hasn't been opened as a
    // session until a tool call names it.
    let resp = call(&mut stdin, &rx, 2, "figmog_files", json!({}));
    assert_eq!(result_json(&resp), json!([]));

    let url = format!("https://www.figma.com/design/{KEY_A}/Fixture?node-id=1-1&t=abc-1");

    // figmog_node: `id` alone (no `file`) resolves the Hero frame from
    // KEY_A's mirror.
    let resp = call(&mut stdin, &rx, 3, "figmog_node", json!({"id": url}));
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");
    let node = result_json(&resp);
    assert_eq!(node["id"], json!("1:1"));
    assert_eq!(node["name"], json!("Hero"));

    // figmog_subtree: same URL, still no `file` — proves the auto-open
    // isn't a one-shot fluke of the first call above.
    let resp = call(
        &mut stdin,
        &rx,
        4,
        "figmog_subtree",
        json!({"id": url, "depth": 0}),
    );
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");
    let subtree = result_json(&resp);
    assert_eq!(subtree["id"], json!("1:1"));
    assert_eq!(subtree["children"], json!([]));

    // The URL's file key really did auto-open a session, not answer from
    // thin air.
    let resp = call(&mut stdin, &rx, 5, "figmog_files", json!({}));
    let rows = result_json(&resp);
    let rows = rows.as_array().expect("files array");
    assert_eq!(rows.len(), 1, "files: {rows:#?}");
    assert_eq!(rows[0]["key"], json!(KEY_A));

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Review fix I3: the explicit-`file`/URL-file mismatch note (spec §2b)
/// must compare *normalized* file keys, not raw argument text — an
/// explicit `file` that's itself a full Figma URL naming the very same
/// file the `id` URL names is not a disagreement, even though the two
/// strings look nothing alike. A genuinely different explicit `file` still
/// gets the note.
#[test]
fn serve_e2e_mismatch_note_compares_normalized_file_keys() {
    let root = build_fixture_root(&[
        (KEY_A, common::fixture_v1()),
        (KEY_B, common::fixture_other()),
    ]);
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // Same file, different spelling: explicit `file` is a full URL naming
    // KEY_A, `id` is a *different* full URL also naming KEY_A (with an
    // unknown node id, so the call still fails) — no mismatch, no note.
    let same_file_url = format!("https://www.figma.com/design/{KEY_A}/Alt-Name");
    let id_url_a = format!("https://www.figma.com/file/{KEY_A}/Fixture?node-id=99-99");
    let resp = call(
        &mut stdin,
        &rx,
        2,
        "figmog_node",
        json!({"id": id_url_a, "file": same_file_url}),
    );
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("99:99"), "text: {text}");
    assert!(
        !text.contains("note:"),
        "same file, different spelling: no mismatch note expected — text: {text}"
    );

    // Genuinely different files: explicit `file` names KEY_A, `id` is a
    // URL naming KEY_B — the note must appear and name both keys.
    let id_url_b = format!("https://www.figma.com/design/{KEY_B}/Other?node-id=99-99");
    let resp = call(
        &mut stdin,
        &rx,
        3,
        "figmog_node",
        json!({"id": id_url_b, "file": KEY_A}),
    );
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("note:"), "text: {text}");
    assert!(text.contains(KEY_A), "text: {text}");
    assert!(text.contains(KEY_B), "text: {text}");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// `--db <path>` with no FILE positional predates multi-file serve (spec
/// §14 non-goal: CLI multi-file addressing is out of scope) — the single
/// session it opens has no real Figma key to pull with (see
/// `sessions::open_session_at`'s `network_key: None` case). Any tool that
/// forces a pull must fail with the same clean pre-v4 message, never panic
/// or attempt a network call against a filesystem-path-shaped "key".
#[test]
fn serve_e2e_db_override_with_no_file_figmog_sync_errors_no_file_key() {
    let (_dir, db) = common::fixture_db();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    let resp = call(&mut stdin, &rx, 2, "figmog_sync", json!({}));
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(result_json(&resp)["accepted"], true);
    let status = wait_for_pull(&mut stdin, &rx, json!({}), "failed");
    let text = status["pull"]["job"]["error"].as_str().unwrap();
    assert!(text.contains("no file key"), "{status}");
    assert!(!text.to_lowercase().contains("panic"), "{status}");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

#[test]
fn serve_e2e_db_images_preserve_cached_renders_and_fills_among_misses() {
    use figmog::model::{Id, ImageBlobRec, ImagePinRec, Rec};

    let (dir, db) = common::fixture_db();
    {
        let mut st = figmog::open_store!(&db);
        let Some(Rec::Node(mut node)) = st.get(&Id::Node("1:2".into())) else {
            panic!("fixture node missing");
        };
        let mut raw: Value = serde_json::from_str(&node.raw).unwrap();
        raw["fills"] = json!([
            {"type": "IMAGE", "imageRef": "missing-fill"},
            {"type": "IMAGE", "imageRef": "cached-fill"},
        ]);
        node.raw = raw.to_string();
        st.wtx(|tx| {
            tx.upsert(&Id::Node(node.id.clone()), &Rec::Node(node.clone()));
            for (kind, subject, version, scale, bytes) in [
                ("render", "1:2", "100", 2000, b"render".as_slice()),
                ("render", "1:3", "99", 2000, b"stale".as_slice()),
                ("fill", "cached-fill", "100", 1000, b"fill".as_slice()),
            ] {
                let format = if kind == "render" { "png" } else { "" };
                let key = figmog::images::image_key(kind, subject, format, scale);
                if kind == "render" {
                    tx.upsert(
                        &Id::ImagePin(key.clone()),
                        &Rec::ImagePin(ImagePinRec {
                            generation: figmog::images::RENDER_PIN_GENERATION,
                            file_version: version.into(),
                        }),
                    );
                }
                tx.upsert(
                    &Id::ImageBlob(key.clone()),
                    &Rec::ImageBlob(ImageBlobRec {
                        key_hash: key,
                        kind: kind.into(),
                        subject: subject.into(),
                        format: "png".into(),
                        scale_milli: scale,
                        file_version: version.into(),
                        bytes: bytes.to_vec(),
                    }),
                );
            }
        });
    }
    // A current-file hint and a node URL must not supply network identity
    // to an explicit store. The child also has no token (spawn_child).
    std::fs::create_dir_all(dir.path().join(".figmog")).unwrap();
    std::fs::write(dir.path().join(".figmog/current"), KEY_A).unwrap();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);
    handshake(&mut stdin, &rx);
    let resp = call(
        &mut stdin,
        &rx,
        2,
        "figmog_images",
        json!({
            "ids": ["1:3", format!("https://www.figma.com/design/{KEY_A}/Fixture?node-id=1-2"), "1:2"],
            "scale": 2,
        }),
    );
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let manifest = result_json(&resp);
    let rows = manifest.as_array().unwrap();
    assert_eq!(rows.len(), 4, "{manifest}");
    assert_eq!(rows[0]["id"], "1:3");
    assert_eq!(rows[3]["ref"], "missing-fill");
    for row in [&rows[0], &rows[3]] {
        assert_eq!(row["cached"], false);
        assert_eq!(row["bytes"], 0);
        let error = row["error"].as_str().unwrap();
        assert!(error.contains("without a file key"), "{error}");
        assert!(error.contains("figmog pull"), "{error}");
        assert!(!error.contains("FIGMA_TOKEN"), "{error}");
    }
    assert_eq!(
        rows[1],
        json!({"id": "1:2", "kind": "render", "format": "png", "bytes": 6, "cached": true})
    );
    assert_eq!(
        rows[2],
        json!({"ref": "cached-fill", "kind": "fill", "format": "png", "bytes": 4, "cached": true})
    );
    assert_eq!(
        &resp["result"]["content"].as_array().unwrap()[1..],
        &[
            json!({"type": "image", "id": "1:2", "mimeType": "image/png", "data": "cmVuZGVy"}),
            json!({"type": "image", "ref": "cached-fill", "mimeType": "image/png", "data": "ZmlsbA=="}),
        ],
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Documents an accepted divergence (see this crate's README, "Multiple
/// files" section, and `serve.rs::build_sessions`'s doc comment):
/// `.figmog/current` is only refreshed by a startup pull that actually
/// *ran* (`!no_watch && !session.mirrored`). `--no-watch` never pulls at
/// startup — not even against a pre-built store under the default
/// `.figmog` root — so `figmog serve <key> --no-watch` (no `--db`) must
/// NOT write `.figmog/current`, even though pre-v4 `figmog serve <key>`
/// (which always watched) did. A real pull that writes it (an initial
/// watch-mode pull against an empty store, or a later watch-tick pull)
/// needs a live network + token and is deliberately not exercised here —
/// this test only pins the `--no-watch` half, which is fully offline.
#[test]
fn serve_e2e_no_watch_default_root_startup_does_not_write_figmog_current() {
    let cwd = tempfile::tempdir().unwrap();
    let response = cwd.path().join("resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_v1()).unwrap(),
    )
    .unwrap();
    let db = cwd.path().join(".figmog").join(KEY_A).join("db");
    common::command()
        .args(["pull", "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();

    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--no-upstream", KEY_A])
        .current_dir(cwd.path())
        .env("FIGMA_TOKEN", "");
    let (mut guard, mut stdin, rx) = spawn_child(cmd);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");

    assert!(
        !cwd.path().join(".figmog").join("current").exists(),
        "--no-watch startup against a pre-built store must not write .figmog/current"
    );
}

// ---- unix-socket control plane e2e (v0.0.2 spec §1) ----
//
// The CLI's own socket routing (`cli::socket`) only ever tries a root
// derived exactly like `figmog serve`'s default: `.figmog` relative to the
// current directory (it has no `--figmog-root` override — that's serve's
// hidden testability knob). Every test below builds a root-derived fixture
// store at `<cwd>/.figmog/<key>/db` (the same layout a real `figmog pull`
// would leave) plus `<cwd>/.figmog/current`, then runs both `figmog serve`
// and every CLI probe with `current_dir(cwd)` so the two sides agree on
// the root.

/// Build a root-derived fixture store at `<dir>/.figmog/<key>/db` — the
/// exact path `sessions::open_session` derives for a startup FILE with no
/// `--figmog-root` override — and establish `<dir>/.figmog/current`
/// pointing at it (which `pull --from-file --db <path>` itself never
/// writes, since `--db` short-circuits key tracking — see
/// `cli::resolve_db`), matching what a real `figmog pull <url>` run from
/// `dir` would leave behind. Returns the tempdir; the caller must keep it
/// alive for the test's duration.
fn default_root_fixture(key: &str, fixture: Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(&response, serde_json::to_string(&fixture).unwrap()).unwrap();
    let figmog_dir = dir.path().join(".figmog");
    let db = figmog_dir.join(key).join("db");
    common::command()
        .args(["pull", "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();
    std::fs::write(figmog_dir.join("current"), key).unwrap();
    dir
}

/// Like [`default_root_fixture`], but for several mirrored files under one
/// root with `.figmog/current` pointed at a caller-chosen key — used by the
/// C1 regression test proving socket-routed calls target *that* key, not
/// whichever file `figmog serve` itself would pick as its own default.
fn default_root_fixture_multi(entries: &[(&str, Value)], current: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let figmog_dir = dir.path().join(".figmog");
    for (key, fixture) in entries {
        let response = dir.path().join(format!("{key}.json"));
        std::fs::write(&response, serde_json::to_string(fixture).unwrap()).unwrap();
        let db = figmog_dir.join(key).join("db");
        common::command()
            .args(["pull", "--from-file"])
            .arg(&response)
            .arg("--db")
            .arg(&db)
            .assert()
            .success();
    }
    std::fs::write(figmog_dir.join("current"), current).unwrap();
    dir
}

const ROUTING_A: &str = "figmogroutingaaa11";
const ROUTING_B: &str = "figmogroutingbbb22";

fn routing_fixture() -> tempfile::TempDir {
    default_root_fixture_multi(
        &[
            (ROUTING_A, common::fixture_v1()),
            (ROUTING_B, common::fixture_other()),
        ],
        ROUTING_A,
    )
}

fn assert_proxy_cli_preserves_args_and_current_mirror(args: Value, socket: bool) {
    let dir = routing_fixture();
    let result =
        json!({"content": [{"type": "text", "text": r#"{"code":"CODE_HERE"}"#}], "isError": false});
    let (address, upstream) =
        spawn_fake_upstream_with_args(vec![result.clone()], false, args.clone());
    let server = if socket {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("figmog"));
        command
            .args([
                "serve",
                "--no-watch",
                "--upstream",
                &address,
                ROUTING_A,
                ROUTING_B,
            ])
            .current_dir(dir.path())
            .env("FIGMA_TOKEN", "");
        let (guard, mut stdin, rx) = spawn_child(command);
        handshake(&mut stdin, &rx);
        Some((guard, stdin))
    } else {
        None
    };
    // Holding B's store makes an accidental direct-mode selection of B
    // fail on its writer lock. A desktop argument must not select B.
    let other_db = dir.path().join(".figmog").join(ROUTING_B).join("db");
    let other_store = (!socket).then(|| figmog::open_store!(&other_db));
    let mut command = common::command();
    command
        .args(["call", "get_code", "--args", &args.to_string()])
        .current_dir(dir.path());
    if !socket {
        command.args(["--no-socket", "--upstream", &address]);
    }
    let output = command.assert().success();
    let value: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(value, result);
    upstream.join().unwrap();
    if let Some((mut guard, stdin)) = server {
        drop(stdin);
        assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
        // The background owner (NWS-2953) holds the stores until it idles out.
        wait_for_owner_exit(&dir.path().join(".figmog"));
    }
    drop(other_store);

    // Desktop calls remain uncached, regardless of their file arguments.
    for key in [ROUTING_A, ROUTING_B] {
        let db = dir.path().join(".figmog").join(key).join("db");
        let store = figmog::open_store!(&db);
        store.rtx(|(_, _, _, _, _, _, _, cache, _, _, _, _)| {
            assert!(cache.iter().next().is_none(), "desktop cache for {key}");
        });
    }
}

#[test]
fn proxy_cli_preserves_desktop_arguments_across_direct_and_socket() {
    for socket in [false, true] {
        for args in [
            json!({"nodeId": "1:2"}),
            json!({"nodeId": "1:2", "file": null}),
            json!({"nodeId": "1:2", "file": ROUTING_B}),
            json!({"nodeId": "1:2", "file": "not-a-figma-ref"}),
            json!({"nodeId": "1:2", "file": 42}),
            json!({"nodeId": "1:2", "file": {"path": "synthetic-file"}}),
        ] {
            assert_proxy_cli_preserves_args_and_current_mirror(args, socket);
        }
    }
}

#[test]
fn proxy_cli_does_not_infer_mirrors_from_desktop_node_urls() {
    let url = format!("https://www.figma.com/design/{ROUTING_B}/Fixture?node-id=1-1");
    for socket in [false, true] {
        for key in ["id", "under", "target"] {
            assert_proxy_cli_preserves_args_and_current_mirror(json!({(key): url}), socket);
        }
    }
}

/// Both files contain 1:1, so a successful read must also prove which
/// file answered. Run the same expectations against each CLI transport.
fn assert_cli_file_routing(cwd: &std::path::Path, flags: &[&str]) {
    let url = format!("https://www.figma.com/design/{ROUTING_B}/Fixture?node-id=1-1");
    let node_args = json!({"id": url}).to_string();
    let null_file_args = json!({"id": url, "file": null}).to_string();
    let explicit_args = json!({"file": ROUTING_B}).to_string();
    let explicit_url_args = json!({"file": url}).to_string();
    let conflicting_args = json!({"file": ROUTING_A, "id": url}).to_string();
    let current_url = format!("https://www.figma.com/design/{ROUTING_A}/Fixture?node-id=1-1");
    let explicit_b_args = json!({"file": ROUTING_B, "id": current_url}).to_string();
    let scoped_args = json!({"under": url, "type": "TEXT"}).to_string();
    let target_url = format!("https://www.figma.com/file/{ROUTING_B}/Fixture?node-id=2-2");
    let target_args = json!({"target": target_url}).to_string();
    let cases: &[(&[&str], &str)] = &[
        (&["get", &url], "Banner"),
        (&["tree", &url], "Banner"),
        (&["dump", &url, "--depth", "0"], "Banner"),
        (&["call", "figmog_node", "--args", &node_args], "Banner"),
        (
            &["call", "figmog_node", "--args", &null_file_args],
            "Banner",
        ),
        (
            &["call", "figmog_status", "--args", &explicit_args],
            "OtherFixture",
        ),
        (
            &["call", "figmog_status", "--args", &explicit_url_args],
            "OtherFixture",
        ),
        (
            &["call", "figmog_node", "--args", &conflicting_args],
            "Hero",
        ),
        (
            &["call", "figmog_node", "--args", &explicit_b_args],
            "Banner",
        ),
        (&["get", "1:1"], "Hero"),
        (&["status"], "Fixture"),
        (&["call", "figmog_status"], "Fixture"),
    ];
    let mut failures = Vec::new();
    for (args, expected) in cases {
        let mut command = common::command();
        command.args(flags).args(*args).current_dir(cwd);
        if args[0] == "call" {
            command.arg("--no-upstream");
        }
        let out = command.output().unwrap();
        let value = serde_json::from_slice::<Value>(&out.stdout).unwrap_or(Value::Null);
        if !out.status.success() || value["name"] != *expected {
            failures.push(format!(
                "{flags:?} {args:?}: expected {expected}, got {value}; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    for args in [
        vec!["find", "--type", "TEXT", "--under", &url],
        vec![
            "call",
            "figmog_find",
            "--args",
            &scoped_args,
            "--no-upstream",
        ],
    ] {
        let out = common::command()
            .args(flags)
            .args(&args)
            .current_dir(cwd)
            .assert()
            .success();
        let value: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
        assert_eq!(value[0]["name"], "Banner", "{flags:?} {args:?}: {value}");
    }
    for args in [
        vec!["instances", &target_url],
        vec![
            "call",
            "figmog_instances",
            "--args",
            &target_args,
            "--no-upstream",
        ],
    ] {
        let out = common::command()
            .args(flags)
            .args(&args)
            .current_dir(cwd)
            .assert()
            .success();
        let value: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
        assert_eq!(value, json!([]), "{flags:?} {args:?}: {value}");
    }
    assert_eq!(
        std::fs::read_to_string(cwd.join(".figmog/current")).unwrap(),
        ROUTING_A
    );
}

#[test]
fn direct_cli_file_routing_prefers_explicit_file_then_url_then_current() {
    let dir = routing_fixture();
    assert_cli_file_routing(dir.path(), &["--no-socket"]);
    assert_cli_file_routing(dir.path(), &[]);
}

#[test]
fn direct_cli_file_routing_honors_db_over_explicit_file_and_url() {
    let dir = routing_fixture();
    let db = dir.path().join(".figmog").join(ROUTING_A).join("db");
    let url = format!("https://www.figma.com/design/{ROUTING_B}/Fixture?node-id=1-1");
    let args = json!({"file": ROUTING_B, "id": url}).to_string();
    // A corrupt current must not interfere with an explicit store path.
    std::fs::write(dir.path().join(".figmog/current"), "invalid").unwrap();
    for (args, expected) in [
        (vec!["get", &url], "Hero"),
        (
            vec!["call", "figmog_node", "--args", &args, "--no-upstream"],
            "Hero",
        ),
        (
            vec!["call", "figmog_status", "--args", &args, "--no-upstream"],
            "Fixture",
        ),
    ] {
        let out = common::command()
            .args(&args)
            .arg("--db")
            .arg(&db)
            .current_dir(dir.path())
            .assert()
            .success();
        let value: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
        assert_eq!(value["name"], expected, "{args:?}: {value}");
    }
}

#[test]
fn socket_cli_file_routing_prefers_explicit_file_then_url_then_current() {
    let dir = routing_fixture();
    let (mut guard, mut stdin, rx) =
        spawn_default_root_serve_keys(dir.path(), &[ROUTING_A, ROUTING_B], &[]);
    handshake(&mut stdin, &rx);
    assert_cli_file_routing(dir.path(), &[]);
    for (tool, expected) in [
        ("figmog_open", "missing required field: file"),
        ("figmog_images", "missing required field: ids"),
    ] {
        let out = common::command()
            .args(["call", tool])
            .current_dir(dir.path())
            .assert()
            .failure()
            .code(1);
        let error: Value = serde_json::from_slice(&out.get_output().stderr).unwrap();
        assert_eq!(error["error"], expected);
    }
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn socket_management_calls_do_not_require_a_valid_current_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let (mut guard, mut stdin, rx) = spawn_default_root_serve_zero_files(dir.path(), &[]);
    handshake(&mut stdin, &rx);
    std::fs::write(dir.path().join(".figmog/current"), "invalid").unwrap();

    let out = common::command()
        .args(["call", "figmog_files"])
        .current_dir(dir.path())
        .assert()
        .success();
    let files: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(files, json!([]));

    let out = common::command()
        .args(["call", "figmog_open"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1);
    let error: Value = serde_json::from_slice(&out.get_output().stderr).unwrap();
    assert_eq!(error["error"], "missing required field: file");

    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

/// Spawn `figmog serve --no-watch --no-upstream <keys...>` against the
/// default (cwd-relative) `.figmog` root, with `FIGMA_TOKEN` scrubbed —
/// every socket-plane e2e test below runs the CLI half from the same
/// `cwd`, so both sides derive the identical root/db paths a real
/// deployment would. `keys` may be empty (spec §14's zero-startup-file
/// shape — the real quick-start invocation, `claude mcp add figmog --
/// figmog serve`, always starts this way).
fn spawn_default_root_serve_keys(
    cwd: &std::path::Path,
    keys: &[&str],
    extra_args: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--no-upstream"])
        .args(keys)
        .args(extra_args)
        .current_dir(cwd)
        .env("FIGMA_TOKEN", "");
    spawn_child(cmd)
}

fn spawn_default_root_serve(
    cwd: &std::path::Path,
    key: &str,
    extra_args: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    spawn_default_root_serve_keys(cwd, &[key], extra_args)
}

/// [`spawn_default_root_serve`] served in this process rather than relayed
/// to a background owner (NWS-2953): `--figmog-root .figmog` keeps the
/// stores where the default root has them and binds the in-process socket
/// at `.figmog/serve.sock`, which the CLI still finds. For the tests of
/// that socket's own mechanics.
fn spawn_in_process_serve(
    cwd: &std::path::Path,
    key: &str,
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    spawn_default_root_serve_keys(cwd, &[key], &["--figmog-root", ".figmog"])
}

/// Zero-startup-files variant of [`spawn_default_root_serve`] (spec §14:
/// the server starts empty and mirrors files as agents/commands reference
/// them).
fn spawn_default_root_serve_zero_files(
    cwd: &std::path::Path,
    extra_args: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    spawn_default_root_serve_keys(cwd, &[], extra_args)
}

/// The `initialize` / `notifications/initialized` handshake every stdio
/// e2e test in this file performs before its first real request — shared
/// here since the socket-plane tests below still drive `figmog serve` over
/// stdio (to prove it's alive and to close it down cleanly) alongside the
/// socket-routed CLI calls under test.
fn handshake(stdin: &mut ChildStdin, rx: &Receiver<String>) {
    send(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(rx);
    assert_eq!(resp["id"], json!(1));
    send(
        stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
}

/// (a) `figmog status` while `figmog serve` owns the store: reachable over
/// the socket ⇒ fresh data, exit 0, no lock error at all (spec §1's
/// headline fix — this is the exact friction the whole feature exists to
/// remove).
#[test]
fn socket_status_returns_fresh_data_with_no_lock_error() {
    const KEY: &str = "figmogsocketkeyaaa111";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let out = common::command()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success();
    let stdout = out.get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", String::from_utf8_lossy(&stdout)));
    assert!(v.get("error").is_none(), "unexpected error shape: {v}");
    assert_eq!(v["name"], json!("Fixture"));
    assert!(
        v["nodes"].as_u64().unwrap_or(0) > 0,
        "expected a nonzero node count: {v}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (b) `figmog call figmog_sync` reaches the *owning* process over the
/// socket rather than failing against the local lock. `FIGMA_TOKEN` is
/// scrubbed from `serve`'s own environment (`spawn_default_root_serve`),
/// so its pull attempt fails with a specific, recognizable error — proving
/// the error genuinely came from serve's own sync attempt (not a local
/// store-lock panic, which this CLI invocation could never hit anyway,
/// since it never opens the store directly when the socket is reachable)
/// — and serve must stay alive and answering afterward.
#[test]
fn socket_call_figmog_sync_reaches_the_owning_process_not_a_local_lock_error() {
    const KEY: &str = "figmogsocketkeybbb222";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let out = common::command()
        .args(["call", "figmog_sync"])
        .current_dir(dir.path())
        .assert()
        .success();
    let accepted: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(accepted["accepted"], true);
    let status = wait_for_pull(&mut stdin, &rx, json!({}), "failed");
    let error = status["pull"]["job"]["error"].as_str().unwrap();
    assert!(
        error.contains("FIGMA_TOKEN is empty")
            && error.contains("unset FIGMA_TOKEN to use saved credentials"),
        "{status}"
    );
    assert!(!error.to_lowercase().contains("locked"), "{status}");
    assert_eq!(status["ready"], true, "the prior mirror remains available");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (c) `--no-socket` forces the old direct-open path even with a reachable
/// serve — reproducing the pre-v0.0.2 lock error exactly, on both the
/// `serve` side (never listens) and the CLI side (never probes).
#[test]
fn no_socket_flag_forces_the_old_lock_error() {
    const KEY: &str = "figmogsocketkeyccc333";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let out = common::command()
        .args(["status", "--no-socket"])
        .current_dir(dir.path())
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(output.status.code(), Some(1));
    let parsed: Value = serde_json::from_slice(&output.stderr).unwrap();
    let err = parsed["error"].as_str().unwrap();
    assert!(
        err.contains("store is locked"),
        "expected the direct-mode lock error, got: {err}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// An explicit store must bypass the running server. Its cache misses
/// report missing file identity; routing to the server's known file would
/// instead report missing credentials. Without --db, the current mirror
/// still routes over the socket and avoids the server-held store lock.
#[test]
fn images_with_db_flag_bypasses_the_socket_even_when_serve_is_reachable() {
    const KEY: &str = "figmogsocketkeyfff666";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    // A second, unrelated store `serve` never opened at all — proves a
    // wrong-file answer (had the bug still routed this through the
    // socket) would have come from `KEY`'s mirror, not this one.
    let other_db = dir.path().join("other-db");
    let response = dir.path().join("other-resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_other()).unwrap(),
    )
    .unwrap();
    common::command()
        .args(["pull", "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(&other_db)
        .assert()
        .success();

    let out = common::command()
        .current_dir(dir.path())
        .env("FIGMA_TOKEN", "")
        .args(["images", "1:1", "--db"])
        .arg(&other_db)
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 1);
    let err = parsed[0]["error"].as_str().unwrap();
    assert!(err.contains("without a file key"), "{err}");
    assert!(!err.contains("FIGMA_TOKEN"), "{err}");

    let routed = common::command()
        .current_dir(dir.path())
        .args(["images", "1:1"])
        .assert()
        .failure()
        .code(1);
    assert!(routed.get_output().stderr.is_empty());
    let parsed: Value = serde_json::from_slice(&routed.get_output().stdout).unwrap();
    let err = parsed[0]["error"].as_str().unwrap();
    assert!(err.contains("FIGMA_TOKEN"), "{err}");

    // serve is still alive and untouched by this call.
    let resp = call(&mut stdin, &rx, 2, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (d) A clean stdin-EOF exit removes `serve.sock` — the socket file must
/// not outlive the process that created it.
#[test]
fn clean_exit_removes_serve_sock() {
    const KEY: &str = "figmogsocketkeyddd444";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_in_process_serve(dir.path(), KEY);
    handshake(&mut stdin, &rx);

    let sock_path = dir.path().join(".figmog").join("serve.sock");
    assert!(
        sock_path.exists(),
        "socket file should exist while serve is running"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");

    assert!(
        !sock_path.exists(),
        "socket file should be removed on clean exit"
    );
}

/// (e) A second `figmog serve` against the same root, while the first is
/// still alive, exits with the clean JSON "another figmog serve owns this
/// root" error — never stealing the socket. Started with zero startup
/// files so its own session-store open (a separate concern, spec §1's I-1)
/// can never race the socket check and mask it with a store-lock error
/// instead.
#[test]
fn second_serve_on_same_root_exits_with_clean_error() {
    const KEY: &str = "figmogsocketkeyeee555";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_in_process_serve(dir.path(), KEY);
    handshake(&mut stdin, &rx);

    let out = common::command()
        .args([
            "serve",
            "--no-watch",
            "--no-upstream",
            "--figmog-root",
            ".figmog",
        ])
        .current_dir(dir.path())
        .env("FIGMA_TOKEN", "")
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(output.status.code(), Some(1));
    let parsed: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr must be exactly one JSON object (parse error: {e}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let err = parsed["error"].as_str().unwrap();
    assert!(
        err.contains("another figmog serve owns this root"),
        "expected the clean multi-owner error, got: {err}"
    );

    // The first serve is untouched by the second's failed startup.
    let resp = call(&mut stdin, &rx, 2, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (f) A stale socket file (nothing listening behind it — the simplest
/// reproduction of a leftover from an unclean exit) is unlinked and
/// rebound at startup, proven by a real client successfully connecting to
/// the *new* socket afterward.
#[test]
fn stale_socket_file_is_unlinked_and_rebound() {
    let dir = tempfile::tempdir().unwrap();
    let figmog_dir = dir.path().join(".figmog");
    std::fs::create_dir_all(&figmog_dir).unwrap();
    let sock_path = figmog_dir.join("serve.sock");
    std::fs::write(&sock_path, b"not a socket, nothing listening here").unwrap();

    let (mut guard, mut stdin, rx) = spawn_in_process_serve(dir.path(), "figmogstalekeyfff666");
    handshake(&mut stdin, &rx);

    assert!(sock_path.exists(), "a real socket should now be bound");
    assert!(
        std::os::unix::net::UnixStream::connect(&sock_path).is_ok(),
        "the rebound socket should accept a real connection"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

// ---- review fix round: C1 (routed reads target `.figmog/current`,
// not serve's own default), I1 (a wedged socket client can't freeze
// stdio), m6 (direct vs. socket parity) ----

/// C1 (reproduced by review, now fixed): two mirrored files, both given at
/// startup — serve's own default is `FIRST` (the first startup FILE) — but
/// `.figmog/current` (the fixture builder's last write) names `SECOND`.
/// Before the fix, a socket-routed `figmog status` carried no `file`
/// argument at all and silently answered from serve's own default
/// (`FIRST`); the fix injects `.figmog/current`'s key so the routed call
/// targets the *same* mirror direct mode would.
#[test]
fn socket_status_targets_current_mirror_not_servers_own_default() {
    const FIRST: &str = "figmogsockparityaaa11";
    const SECOND: &str = "figmogsockparitybbb22";
    let dir = default_root_fixture_multi(
        &[
            (FIRST, common::fixture_v1()),
            (SECOND, common::fixture_other()),
        ],
        SECOND,
    );
    let (mut guard, mut stdin, rx) =
        spawn_default_root_serve_keys(dir.path(), &[FIRST, SECOND], &[]);
    handshake(&mut stdin, &rx);

    let out = common::command()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        v["name"],
        json!("OtherFixture"),
        "expected the .figmog/current mirror (SECOND), not serve's own \
         startup default (FIRST): {v}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// C1 (reproduced by review, now fixed): the real quick-start shape — zero
/// startup files (`figmog serve` with no file argument, exactly what
/// `claude mcp add figmog -- figmog serve` runs), relying entirely on
/// auto-open. Before the fix, a socket-routed `figmog status` with no
/// `file` argument hit `SessionManager::resolve(None, ..)` against a
/// server with *zero* sessions and no `default_key` — a hard "no default
/// mirrored file" error, even though a mirror was already pulled and
/// sitting on disk. The fix routes the injected `.figmog/current` key
/// through the explicit-`file` path, which auto-opens it — the store is
/// already mirrored (`meta_present`), so this costs no Tier-1 pull.
#[test]
fn socket_status_targets_the_current_mirror_with_zero_startup_files() {
    const KEY: &str = "figmogsockzerofilesg1";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve_zero_files(dir.path(), &[]);
    handshake(&mut stdin, &rx);

    // Nothing mirrored yet from serve's own point of view.
    let resp = call(&mut stdin, &rx, 2, "figmog_files", json!({}));
    assert_eq!(result_json(&resp), json!([]));

    let out = common::command()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}"));
    assert!(
        v.get("error").is_none(),
        "expected real data, not the no-default-mirrored-file error: {v}"
    );
    assert_eq!(v["name"], json!("Fixture"));

    // Proves it really auto-opened (spent no network, since the store was
    // already mirrored) rather than answering from nowhere.
    let resp = call(&mut stdin, &rx, 3, "figmog_files", json!({}));
    let rows = result_json(&resp);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "rows: {rows:#?}");
    assert_eq!(rows[0]["key"], json!(KEY));

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// I1 (reproduced by review, now fixed): a socket client that connects,
/// sends a flood of requests, and never reads a single response back must
/// not be able to freeze the single-threaded main loop — and therefore
/// stdio — forever. Before the fix, `write_socket_response` held the
/// `ConnRegistry` mutex across an unbounded blocking write; a client that
/// never drains its receive buffer would eventually wedge that write (and
/// every other connection's registry bookkeeping) indefinitely. After the
/// fix, the write is bounded by `SOCKET_WRITE_TIMEOUT` (5s) and happens on
/// a handle cloned out from behind the lock — this test proves stdio still
/// gets answered within a bounded window (well under this test's own
/// deadline) even while the wedged connection is being flooded.
#[test]
fn wedged_socket_client_does_not_freeze_stdio() {
    const KEY: &str = "figmogsockwedgedkey01";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_in_process_serve(dir.path(), KEY);
    handshake(&mut stdin, &rx);

    let sock_path = dir.path().join(".figmog").join("serve.sock");
    let mut wedged =
        std::os::unix::net::UnixStream::connect(&sock_path).expect("connect the wedged client");

    // Flood a single connection, never reading a single response, with
    // enough total response *bytes* to exceed whatever the OS's send-buffer
    // size for this connection happens to be (varies by platform — this
    // doesn't assume a specific number, just "enough"). `figmog_subtree` on
    // the whole document (full raw JSON, no depth/field limit) gives a much
    // bigger response per call than e.g. `figmog_status` would, so this
    // reaches the same total bytes with far fewer round trips — each
    // round trip carries its own real per-call store-transaction cost on
    // the server, which dominates this loop's own wall time far more than
    // the actual socket write ever would.
    for i in 0..300u32 {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": i,
            "method": "tools/call",
            "params": {"name": "figmog_subtree", "arguments": {"id": "0:0"}},
        });
        if writeln!(wedged, "{frame}").is_err() {
            break;
        }
    }
    let _ = wedged.flush();

    // Drive the SAME server over stdio while the wedged connection is
    // still sitting there unread. `recv_within`'s bound here is
    // deliberately much wider than `TIMEOUT`: this response sits queued
    // behind every already-enqueued socket message (including whichever
    // one's write blocks for up to `SOCKET_WRITE_TIMEOUT` before the fix's
    // registry cleanup unwedges the loop) plus real processing time for
    // the rest of that backlog — bounded, but not "instant". Before the
    // fix (no write timeout at all) this would hang far past this bound
    // instead of resolving.
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "figmog_status", "arguments": {}},
        }),
    );
    let resp = recv_within(&rx, Duration::from_secs(30));
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");

    drop(wedged);
    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, Duration::from_secs(30));
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Explicit null is equality; an omitted equality only tests existence.
/// Exercise both CLI routes and raw MCP against the same synthetic rows.
#[test]
fn where_null_equality_is_preserved_across_direct_socket_and_mcp() {
    const KEY: &str = "figmogwherenullkey01";
    let mut fixture = common::fixture_v1();
    let hero = &mut fixture["document"]["children"][0]["children"][0];
    hero["queryValue"] = Value::Null;
    hero["children"][0]["queryValue"] = json!("VERTICAL");
    hero["children"][1]["queryValue"] = json!(32);
    // All other nodes omit queryValue, so existence must exclude them.
    let dir = default_root_fixture(KEY, fixture);
    let cases = [
        (Some("null"), vec!["1:1"]),
        (None, vec!["1:1", "1:2", "1:3"]),
        (Some("VERTICAL"), vec!["1:2"]),
        (Some("32"), vec!["1:3"]),
    ];
    let run = |equals: Option<&str>, direct: bool| {
        let mut cmd = common::command();
        cmd.args(["where", "--pointer", "/queryValue"])
            .current_dir(dir.path());
        if let Some(equals) = equals {
            cmd.args(["--equals", equals]);
        }
        if direct {
            cmd.arg("--no-socket");
        }
        cmd.assert().success().get_output().stdout.clone()
    };
    // Capture direct results before serve locks this same mirror.
    let direct: Vec<_> = cases.iter().map(|(equals, _)| run(*equals, true)).collect();
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    for ((equals, expected_ids), direct) in cases.iter().zip(&direct) {
        let direct_json: Value = serde_json::from_slice(direct).unwrap();
        let ids: Vec<_> = direct_json
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect();
        assert_eq!(&ids, expected_ids, "direct --equals {equals:?}");

        let socket = run(*equals, false);
        let mut args = json!({"pointer": "/queryValue"});
        if let Some(equals) = equals {
            args["equals"] = serde_json::from_str(equals).unwrap_or_else(|_| json!(equals));
        }
        let resp = call(&mut stdin, &rx, 2, "figmog_where", args);
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");
        let socket_json: Value = serde_json::from_slice(&socket).unwrap();
        let mcp_json = result_json(&resp);
        assert_eq!(
            (&socket_json, &mcp_json),
            (&direct_json, &direct_json),
            "socket and MCP must match direct --equals {equals:?}"
        );
        assert_eq!(&socket, direct, "CLI output must be byte-identical");
    }

    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn search_ties_are_deterministic_across_direct_socket_and_mcp_after_broad_queries() {
    const KEY: &str = "figmogsearchtieskey01";
    let mut nodes: Vec<Value> = (0..300)
        .map(|i| {
            json!({
                "id": format!("1:{i}"), "type": "TEXT",
                "name": if i < 12 { "target" } else { "padding" },
            })
        })
        .collect();
    let scoped: Vec<Value> = nodes.drain(4..12).collect();
    nodes.push(json!({"id": "9:0", "type": "FRAME", "name": "Scope", "children": scoped}));
    // A higher-scoring hit must precede the ties even though its ID sorts last.
    nodes.push(json!({"id": "9:1", "type": "TEXT", "name": "target target target"}));
    let fixture = json!({
        "name": "Search ties", "version": "1", "lastModified": "2026-11-04T00:00:00Z",
        "document": {
            "id": "0:0", "type": "DOCUMENT", "name": "Document",
            "children": [{"id": "0:1", "type": "CANVAS", "name": "Page", "children": nodes}],
        },
    });
    let dir = default_root_fixture(KEY, fixture);
    let cases = [
        (0, None, vec![]),
        (3, None, vec!["9:1", "1:0", "1:1"]),
        (
            20,
            None,
            vec![
                "9:1", "1:0", "1:1", "1:10", "1:11", "1:2", "1:3", "1:4", "1:5", "1:6", "1:7",
                "1:8", "1:9",
            ],
        ),
        (3, Some("9:0"), vec!["1:10", "1:11", "1:4"]),
        (
            20,
            Some("9:0"),
            vec!["1:10", "1:11", "1:4", "1:5", "1:6", "1:7", "1:8", "1:9"],
        ),
        (3, Some("1:299"), vec![]),
    ];
    let run = |limit: usize, under: Option<&str>, direct: bool| {
        let mut cmd = common::command();
        cmd.args(["search", "target", "-n", &limit.to_string()])
            .current_dir(dir.path());
        if let Some(under) = under {
            cmd.args(["--under", under]);
        }
        if direct {
            cmd.arg("--no-socket");
        }
        cmd.assert().success().get_output().stdout.clone()
    };
    // Capture direct results before serve locks this same mirror.
    let direct: Vec<_> = cases
        .iter()
        .map(|(limit, under, expected_ids)| {
            let bytes = run(*limit, *under, true);
            let rows: Value = serde_json::from_slice(&bytes).unwrap();
            let ids: Vec<_> = rows
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect();
            assert_eq!(&ids, expected_ids, "limit {limit}, under {under:?}");
            bytes
        })
        .collect();
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    for after_broad_query in [false, true] {
        if after_broad_query {
            let resp = call(
                &mut stdin,
                &rx,
                2,
                "figmog_search",
                json!({"query": "target padding", "limit": 1000}),
            );
            assert_eq!(resp["result"]["isError"], false, "{resp}");
            assert_eq!(result_json(&resp).as_array().unwrap().len(), 301);
        }
        for ((limit, under, _), direct) in cases.iter().zip(&direct) {
            let socket = run(*limit, *under, false);
            assert_eq!(
                &socket, direct,
                "socket output: limit {limit}, under {under:?}, after broad query {after_broad_query}"
            );
            let mut args = json!({"query": "target", "limit": limit});
            if let Some(under) = under {
                args["under"] = json!(under);
            }
            let resp = call(&mut stdin, &rx, 3, "figmog_search", args);
            assert_eq!(resp["result"]["isError"], false, "{resp}");
            let direct_json: Value = serde_json::from_slice(direct).unwrap();
            assert_eq!(
                result_json(&resp),
                direct_json,
                "MCP output: limit {limit}, under {under:?}, after broad query {after_broad_query}"
            );
        }
    }

    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn at_is_scoped_and_bounded_across_direct_socket_and_mcp_requests() {
    const KEY_A: &str = "figmogpointaaa001";
    const KEY_B: &str = "figmogpointbbb002";
    let mut other = common::fixture_other();
    other["document"]["children"][0]["children"][0]["absoluteBoundingBox"] =
        json!({"x": 0, "y": 0, "width": 20, "height": 20});
    let dir = default_root_fixture_multi(&[(KEY_A, common::fixture_at()), (KEY_B, other)], KEY_A);
    let page_url = format!("https://www.figma.com/design/{KEY_B}/Fixture?node-id=0-1");
    let run = |args: &[&str]| {
        let out = common::command()
            .args(["at", "--x", "0", "--y", "0"])
            .args(args)
            .current_dir(dir.path())
            .assert()
            .success();
        out.get_output().stdout.clone()
    };
    // A page URL must select its own file, even when current names another.
    let other_direct = run(&["--page", &page_url]);
    let rows: Value = serde_json::from_slice(&other_direct).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["name"], "Banner");
    let scope_args = ["--page", "0:1", "--under", "1:1", "--limit", "3"];
    let scoped_direct = run(&scope_args);
    let rows: Value = serde_json::from_slice(&scoped_direct).unwrap();
    let ids: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["1:2", "1:3", "1:1"]);

    let (mut guard, mut stdin, rx) =
        spawn_default_root_serve_keys(dir.path(), &[KEY_A, KEY_B], &[]);
    handshake(&mut stdin, &rx);
    assert_eq!(run(&["--page", &page_url]), other_direct);
    assert_eq!(run(&scope_args), scoped_direct);
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let response = recv(&rx);
    let schema = &response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "figmog_at")
        .unwrap()["inputSchema"];
    for required in ["x", "y", "page"] {
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(required))
        );
    }
    assert_eq!(schema["properties"]["under"]["type"], "string");
    assert_eq!(schema["properties"]["limit"]["type"], "integer");
    assert_eq!(schema["properties"]["limit"]["minimum"], 0);
    assert_eq!(schema["properties"]["limit"]["default"], 100);

    let resp = call(
        &mut stdin,
        &rx,
        3,
        "figmog_at",
        json!({"page": page_url, "x": 0, "y": 0}),
    );
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(
        result_json(&resp),
        serde_json::from_slice::<Value>(&other_direct).unwrap()
    );
    let resp = call(
        &mut stdin,
        &rx,
        4,
        "figmog_at",
        json!({"page": "0:1", "x": 0, "y": 0, "under": "1:1", "limit": 3}),
    );
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(
        result_json(&resp),
        serde_json::from_slice::<Value>(&scoped_direct).unwrap()
    );
    let resp = call(
        &mut stdin,
        &rx,
        5,
        "figmog_at",
        json!({"page": "0:1", "x": 0, "y": 0}),
    );
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(result_json(&resp).as_array().unwrap().len(), 100);
    for extra in [
        json!({"limit": 0}),
        json!({"page": "0:2", "under": "1:1"}),
        json!({"under": "1:10"}),
    ] {
        let mut args = json!({"page": "0:1", "x": 0, "y": 0});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let resp = call(&mut stdin, &rx, 6, "figmog_at", args);
        assert_eq!(resp["result"]["isError"], false);
        assert_eq!(result_json(&resp), json!([]));
    }
    for args in [
        json!({"x": 0, "y": 0}),
        json!({"page": 1, "x": 0, "y": 0}),
        json!({"page": "1:1", "x": 0, "y": 0}),
        json!({"page": "99:99", "x": 0, "y": 0}),
        json!({"page": "0:1", "x": 0, "y": 0, "under": "99:99"}),
        json!({"page": "0:1", "x": 0, "y": 0, "under": 123}),
        json!({"page": "0:1", "x": 0, "y": 0, "limit": -1}),
        json!({"page": "0:1", "x": 0, "y": 0, "limit": 1.5}),
        json!({"page": "0:1", "x": 0, "y": 0, "limit": "1"}),
        json!({"page": "0:1", "x": 0, "y": 0, "limit": null}),
    ] {
        let resp = call(&mut stdin, &rx, 7, "figmog_at", args.clone());
        assert_eq!(resp["result"]["isError"], true, "{args}: {resp}");
    }
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn at_accepts_integral_json_limit_representations_within_usize() {
    const KEY: &str = "figmogpointnumbers01";
    let dir = default_root_fixture(KEY, common::fixture_at());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);
    let max = usize::MAX.to_string();
    let overflow = (usize::MAX as u128 + 1).to_string();
    let float_overflow = format!("{overflow}.0");
    for (limit, expected_len) in [
        ("0", Some(0)),
        ("1", Some(1)),
        ("0.0", Some(0)),
        ("1.0", Some(1)),
        ("1e2", Some(100)),
        ("-0.0", Some(0)),
        (max.as_str(), Some(108)),
        ("1.5", None),
        ("-1.0", None),
        ("1e100", None),
        (overflow.as_str(), None),
        (float_overflow.as_str(), None),
    ] {
        // Send the original numeric spelling over the wire, including exponents.
        let args = format!(r#"{{"page":"0:1","x":0,"y":0,"limit":{limit}}}"#);
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"figmog_at","arguments":{args}}}}}"#
        );
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let response = recv(&rx);
        if let Some(expected_len) = expected_len {
            assert_eq!(
                response["result"]["isError"], false,
                "limit {limit}: {response}"
            );
            assert_eq!(
                result_json(&response).as_array().unwrap().len(),
                expected_len,
                "limit {limit}"
            );
        } else {
            assert_eq!(
                response["result"]["isError"], true,
                "limit {limit}: {response}"
            );
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("expected non-negative integer for `limit`"),
                "limit {limit}: {response}"
            );
        }
    }
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

/// m6: a handful of read commands (deliberately including `where
/// --pointer` with no `--equals` — C2's exact regression shape — and
/// `status` — m1's) run once direct (before serve ever starts, so the
/// store is never locked) and once socket-routed (after serve starts
/// against the very same on-disk data), asserting byte-identical JSON
/// output either way.
#[test]
fn socket_routed_reads_are_byte_identical_to_direct_mode() {
    const KEY: &str = "figmogsockparitykey01";
    let mut fixture = common::fixture_v1();
    fixture["document"]["children"][0]["children"][0]["ready"] = json!(false);
    fixture["document"]["children"][0]["children"][0]["pull"] = json!({"custom": true});
    let dir = default_root_fixture(KEY, fixture);

    let commands: &[&[&str]] = &[
        &["status"],
        &["get", "1:1"],
        &["pages"],
        &["tree"],
        &["find", "--type", "TEXT"],
        &["search", "garden"],
        &["stats"],
        &["where", "--pointer", "/layoutMode", "--equals", "VERTICAL"],
        // The exact shape C2 regressed on: no `--equals` at all.
        &["where", "--pointer", "/layoutMode"],
    ];

    let run = |args: &[&str]| -> Value {
        let out = common::command()
            .args(args)
            .current_dir(dir.path())
            .assert()
            .success();
        serde_json::from_slice(&out.get_output().stdout)
            .unwrap_or_else(|e| panic!("{args:?} stdout not JSON: {e}"))
    };

    // Direct-mode baseline: captured before serve ever starts, so the
    // store is never locked (no `--no-socket` needed — there's nothing to
    // connect to yet either way).
    let direct: Vec<Value> = commands.iter().map(|args| run(args)).collect();

    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let socket: Vec<Value> = commands.iter().map(|args| run(args)).collect();

    for (args, (d, s)) in commands.iter().zip(direct.iter().zip(socket.iter())) {
        assert_eq!(d, s, "direct vs socket mismatch for {args:?}");
    }

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Observe completion through the same MCP interface clients use.
fn wait_for_pull(stdin: &mut ChildStdin, rx: &Receiver<String>, args: Value, state: &str) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let response = call(stdin, rx, 99, "figmog_status", args.clone());
        assert_eq!(response["result"]["isError"], false, "{response}");
        let status = result_json(&response);
        if status["pull"]["job"]["state"] == state {
            return status;
        }
        assert!(Instant::now() < deadline, "waiting for {state}: {status}");
        std::thread::yield_now();
    }
}

/// NWS-2961: a store only ever `import-variables`'d into (no `meta` row)
/// reports its stored snapshot through a real `figmog serve` — over stdio
/// MCP and via socket-routed plain `figmog status` — while the file itself
/// stays not ready. The routed CLI output is byte-identical to direct
/// `figmog status --db` on the same store.
#[test]
fn serve_e2e_import_only_store_reports_variable_snapshot_before_first_pull() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let export = dir.path().join("vars.json");
    std::fs::write(
        &export,
        include_bytes!("fixtures/variables-export.json").as_slice(),
    )
    .unwrap();
    common::command()
        .args(["import-variables", export.to_str().unwrap(), "--db"])
        .arg(&db)
        .assert()
        .success();
    let direct = common::command()
        .args(["status", "--db"])
        .arg(&db)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let direct_json: Value = serde_json::from_slice(&direct).unwrap();
    assert_eq!(direct_json["variables"]["state"], "current");
    let direct_vars = common::command()
        .args(["vars", "--snapshot", "--db"])
        .arg(&db)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let direct_vars_json: Value = serde_json::from_slice(&direct_vars).unwrap();

    // `spawn_serve` runs with cwd = `dir`, so serve's socket lands at
    // `<dir>/.figmog/serve.sock`, which plain CLI reads from `dir` route to.
    let (mut guard, mut stdin, rx) = spawn_serve(&db);
    handshake(&mut stdin, &rx);

    let resp = call(&mut stdin, &rx, 2, "figmog_status", json!({}));
    // One request in flight at a time, so the next line is its answer.
    assert_eq!(resp["id"], 2, "{resp}");
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let status = result_json(&resp);
    assert_eq!(status["ready"], false, "{status}");
    assert_eq!(status["name"], Value::Null);
    assert_eq!(status["version"], Value::Null);
    assert_eq!(status["nodes"], 0);
    assert_eq!(status["variables"], direct_json["variables"], "{status}");

    let resp = call(&mut stdin, &rx, 3, "figmog_vars", json!({"snapshot": true}));
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    assert_eq!(result_json(&resp), direct_vars_json);
    let resp = call(&mut stdin, &rx, 4, "figmog_vars", json!({}));
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    assert_eq!(result_json(&resp), direct_vars_json["variables"]);

    let routed_vars = common::command()
        .args(["vars", "--snapshot"])
        .current_dir(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(routed_vars, direct_vars);

    // Serve holds the writer lock, so success here proves socket routing.
    let routed = common::command()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&routed),
        String::from_utf8_lossy(&direct)
    );
    let called: Value = serde_json::from_slice(
        &common::command()
            .args(["call", "figmog_status"])
            .current_dir(dir.path())
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert_eq!(called["ready"], false, "{called}");
    assert_eq!(called["variables"], direct_json["variables"]);

    // A failed first pull must not look like a healthy import-only store
    // after plain status strips the server's lifecycle fields.
    let resp = call(&mut stdin, &rx, 5, "figmog_sync", json!({}));
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let failed = wait_for_pull(&mut stdin, &rx, json!({}), "failed");
    let pull_error = failed["pull"]["last_completed"]["error"].as_str().unwrap();
    assert!(pull_error.contains("no file key"), "{failed}");
    let routed = common::command()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1);
    assert!(routed.get_output().stdout.is_empty());
    let error: Value = serde_json::from_slice(&routed.get_output().stderr).unwrap();
    let error = error["error"].as_str().unwrap();
    assert!(error.contains("pull failed"), "{error}");
    assert!(error.contains(pull_error), "{error}");

    // Data reads keep the not-ready error: a snapshot is not a mirror.
    let resp = call(&mut stdin, &rx, 6, "figmog_pages", json!({}));
    assert_eq!(resp["result"]["isError"], true, "{resp}");

    drop(stdin);
    let exit = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(exit.success(), "figmog serve exited with {exit:?}");
}

/// NWS-2902 adapter parity over the server transports: every local read
/// in [`common::read_parity_cases`] runs direct (before serve locks the
/// store), then socket-routed through the same CLI command, then as a raw
/// MCP `tools/call`. Socket CLI output is byte-identical to direct; the MCP
/// result is the same JSON. `figmog_status` keeps its documented transport
/// metadata: the socket CLI strips `ready`/`pull`/`upstream` back to the
/// direct shape, while MCP reports them.
#[test]
fn every_local_read_matches_across_direct_socket_and_mcp() {
    const KEY: &str = "figmogreadparitykey01";
    let dir = default_root_fixture(KEY, common::parity_fixture());
    let cases = common::read_parity_cases();
    let run = |args: &[&str]| -> Vec<u8> {
        common::command()
            .args(args)
            .current_dir(dir.path())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let direct: Vec<Vec<u8>> = cases.iter().map(|(args, _, _)| run(args)).collect();

    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    for ((cli_args, tool, tool_args), direct) in cases.iter().zip(&direct) {
        let socket = run(cli_args);
        assert_eq!(
            String::from_utf8_lossy(&socket),
            String::from_utf8_lossy(direct),
            "socket vs direct for {cli_args:?}"
        );
        let resp = call(&mut stdin, &rx, 2, tool, tool_args.clone());
        assert_eq!(
            resp["result"]["isError"], false,
            "{tool} {tool_args}: {resp}"
        );
        let mut mcp = result_json(&resp);
        let direct: Value = serde_json::from_slice(direct).unwrap();
        if *tool == "figmog_status" {
            assert_eq!(mcp["ready"], true);
            assert_eq!(mcp["upstream"], "disabled");
            assert!(mcp["pull"].is_object(), "{mcp}");
            let obj = mcp.as_object_mut().unwrap();
            for key in ["ready", "pull", "upstream"] {
                obj.remove(key);
            }
        }
        assert_eq!(mcp, direct, "MCP vs direct for {tool} {tool_args}");
    }

    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

// ---- NWS-2963: writers through a running serve ----

/// One writer invocation's observable outcome: exit code, stdout, stderr.
fn run_cli(cwd: &std::path::Path, args: &[&str]) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let output = common::command()
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    (output.status.code(), output.stdout, output.stderr)
}

/// With serve owning the store, every writer goes through its socket and
/// prints exactly what the direct path prints on an identical store (run
/// in a second, serve-less project): `pull --from-file` (plain, with
/// `--geometry`, `--fresh`), a network pull (failing identically without
/// credentials), and `import-variables`. Invalid inputs fail identically
/// and leave both mirrors as they were.
#[test]
fn routed_writers_match_the_direct_path_byte_for_byte() {
    const KEY: &str = "figmogwriterkey12345";
    let served = default_root_fixture(KEY, common::fixture_v1());
    let direct = default_root_fixture(KEY, common::fixture_v1());
    let inputs = tempfile::tempdir().unwrap();
    let input = |name: &str, content: &[u8]| {
        let path = inputs.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_string()
    };
    let v1 = input("v1.json", common::fixture_v1().to_string().as_bytes());
    let v2 = input("v2.json", common::fixture_v2().to_string().as_bytes());
    let other = input("other.json", common::fixture_other().to_string().as_bytes());
    let shapeless = input(
        "shapeless.json",
        json!({"name": "Synthetic", "version": "9", "document": {}})
            .to_string()
            .as_bytes(),
    );
    let not_json = input("not.json", b"{ not json");
    let null_json = input("null.json", b"null");
    let vars = input(
        "vars.json",
        include_bytes!("fixtures/variables-export.json").as_slice(),
    );
    let empty_vars = input(
        "empty-vars.json",
        br#"{"variables":{},"variableCollections":{}}"#,
    );
    let bad_vars = input(
        "bad-vars.json",
        json!({"meta": {"variables": {"VariableID:1": 7}, "variableCollections": {}}})
            .to_string()
            .as_bytes(),
    );

    let (mut guard, mut stdin, rx) = spawn_default_root_serve(served.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let steps: Vec<(Vec<&str>, bool)> = vec![
        (vec!["pull", "--from-file", &v2], true),
        (vec!["pull", "--from-file", &other, "--geometry"], true),
        (vec!["pull", "--from-file", &other], true),
        (vec!["import-variables", &vars], true),
        (vec!["import-variables", &empty_vars], false),
        (vec!["import-variables", &empty_vars, "--dry-run"], true),
        (vec!["import-variables", &empty_vars, "--allow-empty"], true),
        (vec!["import-variables", &vars], true),
        (vec!["import-variables", &bad_vars], false),
        (vec!["import-variables", &not_json], false),
        (vec!["pull", "--from-file", &null_json], false),
        (vec!["pull", "--fresh", "--from-file", &null_json], false),
        (vec!["pull", "--from-file", &shapeless], false),
        (vec!["pull", "--fresh", "--from-file", &shapeless], false),
        (vec!["pull", "--fresh", "--from-file", &not_json], false),
        (vec!["pull"], false),
        (vec!["pull", "--fresh"], false),
        (vec!["pull", "--fresh", "--from-file", &v1], true),
        (vec!["import-variables", &vars], true),
        (vec!["pull", "--from-file", &v1, "--geometry"], true),
    ];
    let reads: [&[&str]; 4] = [&["tree"], &["vars", "--snapshot"], &["stats"], &["pages"]];
    for (args, ok) in &steps {
        let routed = run_cli(served.path(), args);
        let direct_out = run_cli(direct.path(), args);
        if args.as_slice() == ["pull"] || args.as_slice() == ["pull", "--fresh"] {
            assert_eq!(routed.0, direct_out.0);
            assert_eq!(routed.1, direct_out.1);
            let routed_error: Value = serde_json::from_slice(&routed.2).unwrap();
            assert!(
                routed_error["error"]
                    .as_str()
                    .unwrap()
                    .contains("credentials read by the running figmog serve at startup")
            );
        } else {
            assert_eq!(
                routed,
                direct_out,
                "{args:?}: routed {} / {}",
                String::from_utf8_lossy(&routed.1),
                String::from_utf8_lossy(&routed.2)
            );
        }
        assert_eq!(routed.0 == Some(0), *ok, "{args:?}: {routed:?}");
        if !ok {
            let error: Value = serde_json::from_slice(&routed.2).unwrap();
            assert!(error["error"].is_string(), "{error}");
        }
        // The mirrors agree after every step (a failed one changed neither).
        for read in reads {
            assert_eq!(
                run_cli(served.path(), read),
                run_cli(direct.path(), read),
                "{read:?} after {args:?}"
            );
        }
    }

    // Every write above went through serve: it still owns the store and
    // reports the last routed pull as a success with its operation id.
    let status = result_json(&call(&mut stdin, &rx, 50, "figmog_status", json!({})));
    assert_eq!(status["name"], "Fixture");
    let operation = status["pull"]["last_success"]["operation"]
        .as_str()
        .unwrap();
    assert!(operation.contains('-'), "{status}");
    // Its receipt is readable from another connection, but the write
    // operations are not agent tools.
    let tools = call(
        &mut stdin,
        &rx,
        51,
        "figmog_internal_pull",
        json!({"file": KEY}),
    );
    assert_eq!(tools["result"]["isError"], true, "{tools}");
    let receipt = run_cli(
        served.path(),
        &[
            "call",
            "figmog_internal_operation",
            "--args",
            &json!({"id": operation}).to_string(),
        ],
    );
    assert_eq!(receipt.0, Some(0), "{receipt:?}");
    let receipt: Value = serde_json::from_slice(&receipt.1).unwrap();
    assert_eq!(receipt["state"], "succeeded");
    assert_eq!(receipt["id"], operation);
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn serve_e2e_empty_store_status_includes_empty_variable_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    // Create only the store's empty schema before the bounded handshake;
    // no pull or import has written any records.
    drop(figmog::open_store!(&db));
    let (mut guard, mut stdin, rx) = spawn_serve(&db);
    handshake(&mut stdin, &rx);

    let resp = call(&mut stdin, &rx, 2, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let status = result_json(&resp);
    assert_eq!(status["ready"], false, "{status}");
    assert_eq!(status["name"], Value::Null);
    assert_eq!(status["nodes"], 0);
    assert_eq!(
        status["variables"],
        json!({
            "state": "none", "source": null, "format": null,
            "format_version": null, "exported_at": null, "digest": null,
            "variables": 0, "collections": 0,
        })
    );
    for (name, args) in [
        ("figmog_vars", json!({"snapshot": true})),
        ("figmog_pages", json!({})),
    ] {
        let resp = call(&mut stdin, &rx, 3, name, args);
        assert_eq!(resp["result"]["isError"], true, "{resp}");
    }
    common::command()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1);

    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

/// `--db` at a store a running serve owns fails with a message naming that
/// serve (pid and socket) instead of a bare lock error, and changes nothing.
#[test]
fn db_flag_at_a_serve_owned_store_names_the_owner() {
    const KEY: &str = "figmogownerkey123456";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);
    let db = dir.path().join(".figmog").join(KEY).join("db");
    let response = dir.path().join("other.json");
    std::fs::write(&response, common::fixture_other().to_string()).unwrap();
    // The store is held by the background owner the relay started.
    let pid = owner_pid(&dir.path().join(".figmog")).to_string();
    let vars = dir.path().join("vars.json");
    std::fs::write(
        &vars,
        include_bytes!("fixtures/variables-export.json").as_slice(),
    )
    .unwrap();

    for args in [
        vec!["status", "--db", db.to_str().unwrap()],
        vec![
            "pull",
            "--fresh",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ],
        vec!["import-variables", vars.to_str().unwrap(), "--no-socket"],
    ] {
        let (code, _, stderr) = run_cli(dir.path(), &args);
        assert_eq!(code, Some(1), "{args:?}");
        let error: Value = serde_json::from_slice(&stderr).unwrap();
        let error = error["error"].as_str().unwrap();
        assert!(
            error.contains("store is locked by figmog serve")
                && error.contains(&format!("pid {pid}"))
                && error.contains(".sock"),
            "{args:?}: {error}"
        );
    }
    let (code, stdout, _) = run_cli(dir.path(), &["status"]);
    assert_eq!(code, Some(0));
    let status: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(status["name"], "Fixture");

    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

/// If the owner disappears after accepting a write, `figmog pull` reports
/// "outcome unknown" with the announced operation id and exits nonzero —
/// it neither retries through the socket nor falls back to opening the
/// store directly. (A stand-in owner on the project's socket plays a serve
/// killed mid-operation.)
#[test]
fn owner_vanishing_mid_write_reports_outcome_unknown_without_retry() {
    use std::os::unix::net::UnixListener;

    const KEY: &str = "figmogvanishkey12345";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let response = dir.path().join("v2.json");
    std::fs::write(&response, common::fixture_v2().to_string()).unwrap();
    let db = dir.path().join(".figmog").join(KEY).join("db");
    let before = common::store_snapshot(&db);
    let listener = UnixListener::bind(dir.path().join(".figmog/serve.sock")).unwrap();

    let cli = {
        let cwd = dir.path().to_path_buf();
        let response = response.clone();
        std::thread::spawn(move || {
            common::command()
                .args(["pull", "--from-file"])
                .arg(&response)
                .current_dir(cwd)
                .output()
                .unwrap()
        })
    };
    let (stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    let request: Value = serde_json::from_str(&request).unwrap();
    assert_eq!(request["params"]["name"], "figmog_internal_pull");
    assert_eq!(request["params"]["arguments"]["file"], KEY);
    assert_eq!(
        request["params"]["arguments"]["from_file"],
        common::fixture_v2()
    );
    let mut out = stream;
    writeln!(
        out,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/figmog/operation",
            "params": {"id": "c0ffee-1"}})
    )
    .unwrap();
    drop(out);
    drop(reader);

    let output = cli.join().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    let error = error["error"].as_str().unwrap();
    assert!(
        error.starts_with("outcome unknown") && error.contains("operation c0ffee-1"),
        "{error}"
    );
    assert!(output.stdout.is_empty());
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err(), "the CLI connected again");
    assert_eq!(
        common::store_snapshot(&db),
        before,
        "the CLI wrote directly"
    );
}

#[test]
fn deep_tree_reads_over_socket_match_direct_cli() {
    #[path = "fixtures/deep_file.rs"]
    mod deep_file;

    const KEY: &str = "figmogdeepfixture01";
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("deep.json");
    let payload = format!("{}null{}", r#"{"layer":["#.repeat(500), "]}".repeat(500));
    let file = deep_file::file(500).replacen(
        r#""type":"DOCUMENT","#,
        &format!(r#""type":"DOCUMENT","payload":{payload},"#),
        1,
    );
    std::fs::write(&response, file).unwrap();
    common::command()
        .current_dir(dir.path())
        .args(["pull", KEY, "--from-file"])
        .arg(&response)
        .assert()
        .success();
    let mut direct = Vec::new();
    for args in [
        vec!["tree"],
        vec!["dump", "0:0"],
        vec!["where", "--pointer", "/id"],
        vec!["where", "--pointer", "/missing"],
        vec!["where", "--pointer", "/payload"],
        vec!["where", "--pointer", "/payload", "--equals", &payload],
    ] {
        let output = common::command()
            .current_dir(dir.path())
            .args(&args)
            .assert()
            .success();
        direct.push((args, output.get_output().stdout.clone()));
    }
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);
    for (args, expected) in direct {
        let output = common::command()
            .current_dir(dir.path())
            .args(args)
            .assert()
            .success();
        assert_eq!(output.get_output().stdout, expected);
    }
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

/// A local serve opened at an explicit unrelated database must refuse a
/// routed write whose Git-shared destination differs from its own.
#[test]
fn git_routed_write_refuses_a_different_explicit_serve_database() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&cwd)
            .status()
            .unwrap()
            .success()
    );
    const KEY: &str = "figmogroutingfixture123";
    let response = cwd.join("response.json");
    std::fs::write(&response, common::fixture_v1().to_string()).unwrap();
    common::command()
        .current_dir(&cwd)
        .args(["pull", KEY, "--from-file"])
        .arg(&response)
        .assert()
        .success();
    let shared = cwd.join(".git/figmog").join(KEY).join("db");
    let before = common::store_snapshot(&shared);
    let unrelated = cwd.join("other/db");
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("figmog"));
    cmd.current_dir(&cwd)
        .args(["serve", KEY, "--db"])
        .arg(&unrelated)
        .args(["--no-watch", "--no-upstream"]);
    let (mut guard, mut stdin, rx) = spawn_child(cmd);
    handshake(&mut stdin, &rx);
    let output = common::command()
        .current_dir(&cwd)
        .args(["pull", KEY, "--from-file"])
        .arg(&response)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"].as_str().unwrap().contains("--no-socket"),
        "{error}"
    );
    assert!(!cwd.join(".figmog").join(KEY).exists());
    assert_eq!(common::store_snapshot(&shared), before);
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn routed_saved_file_larger_than_the_ordinary_frame_cap_is_accepted() {
    const KEY: &str = "figmoglargewrite12345";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);
    let mut file = common::fixture_v2();
    file["synthetic_padding"] = Value::String("x".repeat(9 * 1024 * 1024));
    let response = dir.path().join("large.json");
    std::fs::write(&response, serde_json::to_vec(&file).unwrap()).unwrap();
    let output = common::command()
        .current_dir(dir.path())
        .args(["pull", "--from-file"])
        .arg(&response)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let status = result_json(&call(&mut stdin, &rx, 2, "figmog_status", json!({})));
    assert_eq!(status["version"], "101");
    assert!(status["pull"]["last_success"]["operation"].is_string());
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}

#[test]
fn routed_import_preview_does_not_initialize_another_missing_mirror() {
    const KEY: &str = "figmogpreviewexisting123";
    const MISSING: &str = "figmogpreviewmissing123";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);
    std::fs::write(dir.path().join(".figmog/current"), MISSING).unwrap();
    let export = dir.path().join("variables.json");
    std::fs::write(&export, include_bytes!("fixtures/variables-export.json")).unwrap();
    let output = common::command()
        .current_dir(dir.path())
        .arg("import-variables")
        .arg(&export)
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["imported"], 3);
    assert_eq!(result["removed_ids"], json!([]));
    assert!(!dir.path().join(".figmog").join(MISSING).exists());
    drop(stdin);
    assert!(wait_with_timeout(&mut guard.0, TIMEOUT).success());
}
