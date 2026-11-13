//! NWS-2962: slow clients and heavy reads must not freeze serve for other
//! clients. Heavy reads run on read workers against a leased published
//! view; responses are written by per-connection writer threads; a store
//! reset waits behind the per-store barrier. Synthetic documents only.
use super::*;
use crate::sessions::{FileSession, StoreReset};
use std::io::Read;

const KEY_A: &str = "AAA1234567890";
const KEY_B: &str = "BBB1234567890";

/// A synthetic document of roughly `nodes` nodes: pages of 64 frames, each
/// holding 48 text, instance and rectangle leaves. Deterministic. At 100k
/// nodes its whole-document `figmog_subtree` is about 20 MB.
fn synthetic_document(nodes: usize) -> Value {
    const PER_FRAME: usize = 48;
    const FRAMES_PER_PAGE: usize = 64;
    let frames = nodes.div_ceil(PER_FRAME + 1);
    let pages = frames.div_ceil(FRAMES_PER_PAGE).max(1);
    let copy = "lorem ipsum dolor sit amet ".repeat(4);
    let mut page_values = Vec::new();
    for p in 0..pages {
        let frame_values: Vec<Value> = (p * FRAMES_PER_PAGE..((p + 1) * FRAMES_PER_PAGE).min(frames))
            .map(|frame| {
                let leaves: Vec<Value> = (0..PER_FRAME)
                    .map(|i| {
                        let id = format!("{}:{}", frame + 10, i + 1);
                        match i % 3 {
                            0 => json!({"id": id, "name": format!("Label {frame}-{i}"), "type": "TEXT",
                                "characters": format!("Synthetic copy number {frame} {i} {copy}"),
                                "style": {"fontFamily": "Basis", "fontSize": 14.0},
                                "styles": {"text": "S:2"}, "children": []}),
                            1 => json!({"id": id, "name": "Button", "type": "INSTANCE",
                                "componentId": "2:2", "children": []}),
                            _ => json!({"id": id, "name": format!("Box {i}"), "type": "RECTANGLE",
                                "absoluteBoundingBox": {"x": i as f64, "y": 0.0, "width": 10.0, "height": 10.0},
                                "fills": [{"type": "SOLID", "color": {"r": 0.1, "g": 0.2, "b": 0.3, "a": 1.0}}],
                                "styles": {"fill": "S:1"}, "children": []}),
                        }
                    })
                    .collect();
                json!({"id": format!("{}:0", frame + 10), "name": format!("Frame {frame}"),
                    "type": "FRAME", "children": leaves})
            })
            .collect();
        page_values.push(
            json!({"id": format!("0:{}", p + 10), "name": format!("Page {p}"),
            "type": "CANVAS", "children": frame_values}),
        );
    }
    page_values.push(
        json!({"id": "0:2", "name": "Components", "type": "CANVAS", "children": [
        {"id": "2:2", "name": "Button", "type": "COMPONENT", "children": []}]}),
    );
    json!({"name": "Synthetic", "version": "1", "lastModified": "2026-01-01T12:00:00Z",
    "document": {"id": "0:0", "name": "Document", "type": "DOCUMENT", "children": page_values},
    "components": {"2:2": {"key": "k22", "name": "Button", "description": "", "remote": false}},
    "styles": {
        "S:1": {"key": "sk1", "name": "Brand/Primary", "styleType": "FILL", "description": "", "remote": false},
        "S:2": {"key": "sk2", "name": "Body", "styleType": "TEXT", "description": "", "remote": false}
    }})
}

struct DocApi(Value);

impl FigmaApi for DocApi {
    fn file_meta(&self, _: &str) -> Result<FileMetaResp, ApiError> {
        Ok(FileMetaResp {
            name: "Synthetic".into(),
            last_touched_at: "t".into(),
        })
    }
    fn file(&self, _: &str, _: bool) -> Result<Value, ApiError> {
        Ok(self.0.clone())
    }
}

/// A session whose store holds a `nodes`-node synthetic mirror.
fn mirrored(dir: &std::path::Path, key: &str, nodes: usize) -> FileSession {
    mirrored_doc(dir, key, synthetic_document(nodes))
}

fn manager_of(dir: &std::path::Path, sessions: Vec<FileSession>) -> SessionManager {
    SessionManager {
        default_key: Some(sessions[0].key.clone()),
        sole_default: true,
        sessions,
        root: dir.to_path_buf().into(),
        token: None,
        api: None,
    }
}

fn request(id: Value, name: &str, args: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": name, "arguments": args}})
    .to_string()
}

/// The response frame the pre-NWS-2962 loop wrote: dispatched inline on
/// the session and serialized as one compact line.
fn inline_frame(session: &FileSession, line: &str, mode: ToolErrorMode) -> String {
    let mut handler = FnHandler(|name: &str, args: &Value| {
        let mut args = args.clone();
        args.as_object_mut().unwrap().remove("file");
        session.dispatch(name, &args)
    });
    match mcp::dispatch_message(line, &dispatch::tool_registry(), &mut handler, mode) {
        Some(Reply::Now(response)) => response.to_string(),
        _ => unreachable!("inline dispatch answers now"),
    }
}

fn read_line(reader: &mut BufReader<UnixStream>) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.ends_with('\n'), "truncated frame");
    line.pop();
    line
}

/// A running serve loop with registered socket connections.
struct Server {
    tx: mpsc::Sender<Incoming>,
    registry: ConnRegistry,
    stdout: mpsc::Receiver<String>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl Server {
    fn start(manager: SessionManager, offload: Offload) -> Self {
        let (tx, rx) = mpsc::channel();
        let registry: ConnRegistry = Arc::default();
        let (stdout_tx, stdout) = mpsc::channel();
        let loop_registry = registry.clone();
        let thread = std::thread::spawn(move || {
            serve_loop_with_offload(
                manager,
                offload,
                true,
                false,
                Duration::from_secs(10),
                Instant::now() + Duration::from_secs(3600),
                rx,
                loop_registry,
                move |frame: &str| stdout_tx.send(frame.to_string()).is_ok(),
            )
        });
        Self {
            tx,
            registry,
            stdout,
            thread: Some(thread),
        }
    }

    fn connect(&self, conn_id: u64) -> BufReader<UnixStream> {
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        self.registry.register(conn_id, server).unwrap();
        BufReader::new(client)
    }

    fn send(&self, conn_id: u64, line: String) {
        self.tx.send(Incoming::Socket(conn_id, line)).unwrap();
    }

    fn stop(mut self) {
        self.tx.send(Incoming::StdinClosed).unwrap();
        self.thread.take().unwrap().join().unwrap().unwrap();
    }
}

fn stdio_call(server: &Server, name: &str, args: Value) -> Value {
    server
        .tx
        .send(Incoming::Stdio(request(json!(1), name, args)))
        .unwrap();
    serde_json::from_str(&server.stdout.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap()
}

#[test]
fn empty_server_auto_opens_first_explicit_and_node_url_references_without_watch() {
    const KEY: &str = "NEW1234567890";
    for args in [
        json!({"file": KEY, "id": "0:0"}),
        json!({"id": format!("https://www.figma.com/design/{KEY}/Synthetic?node-id=0-0")}),
    ] {
        let mut h = Harness::new();
        h.manager.sessions.clear();
        h.manager.default_key = None;
        let root = h._dir.path().join("empty");
        std::fs::create_dir(&root).unwrap();
        h.manager.root = root.clone().into();
        let server = Server::start(h.manager, Offload::new(None));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let first = stdio_call(&server, "figmog_node", args.clone());
            assert_eq!(first["result"]["isError"], true, "{first}");
            assert!(
                first["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("not ready"),
                "{first}"
            );
            assert_eq!(
                h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
                "fetch"
            );
            assert!(root.join(KEY).join("db").is_dir());
            let status = stdio_call(&server, "figmog_status", json!({"file": KEY}));
            let status: Value =
                serde_json::from_str(status["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(status["pull"]["job"]["id"], 1);
            h.release.send(()).unwrap();

            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let status = stdio_call(&server, "figmog_status", json!({"file": KEY}));
                let status: Value =
                    serde_json::from_str(status["result"]["content"][0]["text"].as_str().unwrap())
                        .unwrap();
                if status["pull"]["job"]["state"] == "succeeded" {
                    break;
                }
                assert!(Instant::now() < deadline, "{status}");
            }
            let node = stdio_call(&server, "figmog_node", args);
            assert_eq!(node["result"]["isError"], false, "{node}");
            let node: Value =
                serde_json::from_str(node["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(node["name"], "1");
            assert_eq!(h.api.calls.load(Ordering::Relaxed), 1);
        }));
        // Release the worker before teardown even if an assertion fails, so
        // its store cannot outlive the temporary root during unwinding.
        let _ = h.release.send(());
        server.stop();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

#[test]
fn bare_socket_reads_of_an_unseen_file_never_create_a_store() {
    let mut h = Harness::new();
    h.manager.sessions.clear();
    h.manager.default_key = None;
    let root = h._dir.path().join("empty");
    std::fs::create_dir(&root).unwrap();
    h.manager.root = root.clone().into();
    let server = Server::start(h.manager, Offload::new(None));
    let mut client = server.connect(1);
    for name in ["figmog_status", "figmog_pages", "figmog_vars"] {
        server.send(1, request(json!(1), name, json!({"file":"NEW1234567890"})));
        let result: Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert!(
            result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no mirror here"),
            "{result}"
        );
        assert!(!root.join("NEW1234567890").exists());
    }
    assert_eq!(h.api.calls.load(Ordering::Relaxed), 0);
    server.stop();
}

/// While one client trickle-reads a whole-document subtree and another
/// keeps `figmog_stats` running, a third client's `figmog_node` round
/// trips stay under `bound`. Returns (worst, median) latency.
fn isolation_scenario(nodes: usize, rounds: usize, bound: Duration) -> (Duration, Duration) {
    let dir = tempfile::tempdir().unwrap();
    let session = mirrored(dir.path(), KEY_A, nodes);
    let big = request(json!(1), "figmog_subtree", json!({"id": "0:0"}));
    let expected_len = inline_frame(&session, &big, ToolErrorMode::JsonRpc).len() + 1;
    let server = Server::start(manager_of(dir.path(), vec![session]), Offload::new(None));
    let trickle = server.connect(1);
    let mut stats = server.connect(2);
    let mut fast = server.connect(3);
    let stop = Arc::new(AtomicBool::new(false));

    server.send(1, big);
    let trickled = {
        let stop = stop.clone();
        let mut client = trickle.into_inner();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let mut total = 0;
            while !stop.load(Ordering::Relaxed) {
                match client.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total += n,
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            (client, total)
        })
    };
    let stats_tx = server.tx.clone();
    let stats_runner = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut answered = 0u64;
            while !stop.load(Ordering::Relaxed) {
                stats_tx
                    .send(Incoming::Socket(
                        2,
                        request(json!(answered), "figmog_stats", json!({})),
                    ))
                    .unwrap();
                let frame: Value = serde_json::from_str(&read_line(&mut stats)).unwrap();
                assert_eq!(frame["id"], answered, "{frame}");
                assert!(
                    frame["result"]["structuredContent"].is_object()
                        || frame["result"]["content"].is_array(),
                    "{frame}"
                );
                answered += 1;
            }
            answered
        })
    };
    // Let the subtree response start trickling and stats occupy a worker.
    std::thread::sleep(Duration::from_millis(300));
    let mut latencies = Vec::new();
    for round in 0..rounds {
        let start = Instant::now();
        server.send(
            3,
            request(json!(round), "figmog_node", json!({"id": "12:3"})),
        );
        let frame: Value = serde_json::from_str(&read_line(&mut fast)).unwrap();
        latencies.push(start.elapsed());
        assert_eq!(frame["id"], round, "{frame}");
        let node: Value =
            serde_json::from_str(frame["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(node["id"], "12:3", "{node}");
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Relaxed);
    let stats_answered = stats_runner.join().unwrap();
    let (trickle_client, trickled_bytes) = trickled.join().unwrap();
    assert!(
        trickled_bytes < expected_len,
        "the trickle reader must still be mid-response ({trickled_bytes} of {expected_len} bytes)"
    );
    // Closing the trickle client frees its writer before shutdown.
    drop(trickle_client);
    server.stop();
    latencies.sort();
    let (worst, median) = (
        latencies[latencies.len() - 1],
        latencies[latencies.len() / 2],
    );
    eprintln!(
        "{nodes} nodes, {expected_len}-byte trickled response, {stats_answered} stats answered: \
         node round trip median {median:?}, worst {worst:?}"
    );
    assert!(stats_answered > 0, "stats must have run concurrently");
    assert!(
        worst < bound,
        "worst node round trip {worst:?} >= {bound:?}"
    );
    (worst, median)
}

/// Always-on, scaled down: a 20k-node mirror. The bound is loose (250 ms)
/// so a loaded CI machine does not flake; the full-size benchmark below
/// holds the 50 ms target.
#[test]
fn slow_reader_and_heavy_stats_do_not_delay_another_clients_point_read() {
    isolation_scenario(20_000, 40, Duration::from_millis(250));
}

/// The NWS-2962 acceptance benchmark: a 100k-node mirror, a client
/// trickle-reading its ~20 MB whole-document subtree, another running
/// `figmog_stats` continuously; a third client's `figmog_node` stays
/// under 50 ms. Slow (the synthetic pull alone takes several seconds):
/// `cargo test --lib isolation_benchmark -- --ignored --nocapture`.
#[test]
#[ignore = "manual 100k-node latency benchmark; slow synthetic import"]
fn isolation_benchmark_100k_nodes_20mb_trickle() {
    isolation_scenario(100_000, 100, Duration::from_millis(50));
}

/// Per-tool cost on a 100k-node mirror, which drove the inline/worker
/// classification in `reads.rs`:
/// `cargo test --lib measure_read_costs -- --ignored --nocapture`.
#[test]
#[ignore = "manual per-tool timing on a 100k-node mirror"]
fn measure_read_costs() {
    let dir = tempfile::tempdir().unwrap();
    let session = mirrored(dir.path(), KEY_A, 100_000);
    let calls = [
        ("figmog_status", json!({})),
        ("figmog_pages", json!({})),
        ("figmog_node", json!({"id": "500:3"})),
        ("figmog_node", json!({"id": "500:0", "children": true})),
        ("figmog_path", json!({"id": "500:3"})),
        ("figmog_components", json!({})),
        ("figmog_tree", json!({"depth": 2})),
        ("figmog_tree", json!({})),
        ("figmog_subtree", json!({"id": "500:0"})),
        ("figmog_subtree", json!({"id": "0:0"})),
        ("figmog_find", json!({"type": "TEXT"})),
        ("figmog_search", json!({"query": "copy number 7"})),
        ("figmog_instances", json!({"target": "2:2"})),
        ("figmog_styles", json!({})),
        ("figmog_uses", json!({"id": "S:1"})),
        ("figmog_vars", json!({})),
        ("figmog_stats", json!({})),
        ("figmog_text", json!({})),
        (
            "figmog_where",
            json!({"pointer": "/type", "equals": "TEXT"}),
        ),
        ("figmog_at", json!({"x": 1.0, "y": 1.0})),
    ];
    for (name, args) in calls {
        let start = Instant::now();
        let out = session.dispatch(name, &args);
        let elapsed = start.elapsed();
        let bytes = match &out {
            Ok(ToolOutput::Json(v)) => v.to_string().len(),
            _ => 0,
        };
        eprintln!("{name} {args}: {elapsed:?}, {bytes} B {:?}", out.err());
    }
}

/// Two connections (and stdio) reuse the same JSON-RPC ids for different
/// inline and worker reads: every response reaches the connection that
/// asked, byte-identical to the frame the inline loop used to write.
#[test]
fn colliding_ids_route_each_response_to_its_own_connection_byte_identically() {
    let dir = tempfile::tempdir().unwrap();
    let a = mirrored(dir.path(), KEY_A, 2_000);
    let b = mirrored(dir.path(), KEY_B, 2_000);
    let plan: Vec<(u64, String)> = vec![
        (
            1,
            request(json!(7), "figmog_subtree", json!({"id": "12:0"})),
        ),
        (
            2,
            request(
                json!(7),
                "figmog_subtree",
                json!({"id": "13:0", "file": KEY_B}),
            ),
        ),
        (1, request(json!(8), "figmog_node", json!({"id": "12:1"}))),
        (2, request(json!(8), "figmog_stats", json!({"file": KEY_B}))),
        (1, request(json!("x"), "figmog_text", json!({}))),
        (
            2,
            request(
                json!("x"),
                "figmog_node",
                json!({"id": "nope", "file": KEY_B}),
            ),
        ),
    ];
    let session_for = |line: &str| if line.contains(KEY_B) { &b } else { &a };
    let expected: Vec<(u64, String)> = plan
        .iter()
        .map(|(conn, line)| {
            (
                *conn,
                inline_frame(session_for(line), line, ToolErrorMode::JsonRpc),
            )
        })
        .collect();
    let stdio_line = request(json!(7), "figmog_stats", json!({}));
    let stdio_expected = inline_frame(&a, &stdio_line, ToolErrorMode::McpContent);
    let server = Server::start(manager_of(dir.path(), vec![a, b]), Offload::new(None));
    let mut conns = [server.connect(1), server.connect(2)];
    for (conn, line) in &plan {
        server.send(*conn, line.clone());
    }
    server.tx.send(Incoming::Stdio(stdio_line)).unwrap();
    for (index, reader) in conns.iter_mut().enumerate() {
        let conn = index as u64 + 1;
        let mut want: Vec<&String> = expected
            .iter()
            .filter(|(c, _)| *c == conn)
            .map(|(_, f)| f)
            .collect();
        let mut got: Vec<String> = (0..want.len()).map(|_| read_line(reader)).collect();
        // Worker reads may overtake inline ones on a stream (JSON-RPC
        // matches by id); compare as sets of exact frames.
        want.sort();
        got.sort();
        assert_eq!(got.iter().collect::<Vec<_>>(), want, "connection {conn}");
    }
    let stdout = server.stdout.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(stdout, stdio_expected);
    server.stop();
}

/// A reader hook that parks every read of `tool` (`"*"`: every read)
/// until released, reporting each one it sees.
fn gated_offload(tool: &'static str) -> (Offload, mpsc::Receiver<String>, mpsc::Sender<()>) {
    let (seen_tx, seen) = mpsc::channel();
    let (release, gate) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(gate));
    let mut offload = Offload::new(None);
    offload.reads.before_read = Some(Arc::new(move |name: &str| {
        let _ = seen_tx.send(name.to_string());
        if tool == "*" || name == tool {
            gate.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    }));
    (offload, seen, release)
}

/// Dispatch one stdio request; a deferred one is parked on the pool and
/// its call returned.
fn begin(
    manager: &mut SessionManager,
    offload: &mut Offload,
    line: &str,
) -> Result<PendingCall, Value> {
    let live = Arc::new(AtomicBool::new(false));
    let mut handler = FnHandler(|name: &str, args: &Value| {
        handle_tool_call(
            manager,
            offload,
            &live,
            "disabled",
            Duration::ZERO,
            name,
            args,
            sessions::SessionResolution::AutoOpen,
        )
    });
    match mcp::dispatch_message(
        line,
        &dispatch::tool_registry(),
        &mut handler,
        ToolErrorMode::McpContent,
    ) {
        Some(Reply::Now(response)) => Err(response),
        Some(Reply::Later(call)) => {
            offload.park(&call);
            Ok(call)
        }
        None => unreachable!(),
    }
}

fn finish(offload: &Offload, call: PendingCall) -> String {
    loop {
        let done = offload
            .done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("deferred read answers");
        if done.token == call.token() {
            let mut frame = answer_frame(done.answer, call);
            frame.pop();
            return frame;
        }
    }
}

fn answer(manager: &mut SessionManager, offload: &mut Offload, line: &str) -> Value {
    let response = match begin(manager, offload, line) {
        Err(response) => response.to_string(),
        Ok(call) => finish(offload, call),
    };
    serde_json::from_str(&response).unwrap()
}

/// The reset barrier: a reset that overlaps a paused worker subtree stops
/// admitting reads for that store only, waits for the paused read to
/// release its view (which still answers correctly), and only then
/// reopens and republishes the store. The other file stays readable
/// throughout.
#[test]
fn reset_waits_for_a_paused_subtree_and_leaves_other_files_responsive() {
    let dir = tempfile::tempdir().unwrap();
    let a = mirrored(dir.path(), KEY_A, 2_000);
    let b = mirrored(dir.path(), KEY_B, 2_000);
    let subtree = request(json!(1), "figmog_subtree", json!({"id": "10:0"}));
    let expected = inline_frame(&a, &subtree, ToolErrorMode::McpContent);
    let mut manager = manager_of(dir.path(), vec![a, b]);
    let (mut offload, seen, release) = gated_offload("figmog_subtree");

    let paused = begin(&mut manager, &mut offload, &subtree).unwrap();
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(10)).unwrap(),
        "figmog_subtree"
    );

    // 1. Validation happens before anything stops.
    assert!(
        manager
            .begin_reset("not a key", StoreReset::Reopen)
            .is_err()
    );
    assert!(
        manager
            .begin_reset("CCC1234567890", StoreReset::Reopen)
            .is_err()
    );
    // 2. Stop admitting reads for that store only.
    manager.begin_reset(KEY_A, StoreReset::Reopen).unwrap();
    assert!(
        manager.begin_reset(KEY_A, StoreReset::Reopen).is_err(),
        "one reset at a time"
    );
    for (tool, args) in [
        ("figmog_stats", json!({})),
        ("figmog_node", json!({"id": "10:1"})),
        ("figmog_status", json!({})),
    ] {
        let refused = answer(&mut manager, &mut offload, &request(json!(2), tool, args));
        assert_eq!(refused["result"]["isError"], true, "{tool}: {refused}");
        let text = refused["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("being reset"), "{tool}: {text}");
    }
    // The other file stays responsive, inline and on workers.
    for (tool, args) in [
        ("figmog_stats", json!({"file": KEY_B})),
        ("figmog_node", json!({"id": "10:1", "file": KEY_B})),
    ] {
        let ok = answer(&mut manager, &mut offload, &request(json!(3), tool, args));
        assert_eq!(ok["result"]["isError"], false, "{tool}: {ok}");
    }
    // 3. While the paused read holds its view, the store is not reopened.
    for _ in 0..20 {
        assert!(manager.service_resets().is_empty());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(manager.sessions[0].key, KEY_A);
    release.send(()).unwrap();
    // The paused read answers from the view it leased, unchanged.
    assert_eq!(finish(&offload, paused), expected);
    // 4–5. Then reset, reopen and publish in place.
    let deadline = Instant::now() + Duration::from_secs(10);
    let finished = loop {
        let finished = manager.service_resets();
        if !finished.is_empty() {
            break finished;
        }
        assert!(Instant::now() < deadline, "reset never drained");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(finished, vec![(KEY_A.to_string(), Ok(()))]);
    assert_eq!(manager.sessions[0].key, KEY_A);
    let again = request(json!(1), "figmog_subtree", json!({"id": "10:0"}));
    release.send(()).unwrap(); // the hook parks this read too
    assert_eq!(
        answer(&mut manager, &mut offload, &again),
        serde_json::from_str::<Value>(&expected).unwrap()
    );
}

/// A connection may have at most `MAX_PENDING_PER_REQUESTER` calls
/// pending; the next is refused immediately. A queued read whose
/// connection closes is skipped, never computed.
#[test]
fn pending_requests_are_bounded_per_connection_and_closed_connections_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let a = mirrored(dir.path(), KEY_A, 500);
    let (offload, seen, release) = gated_offload("*");
    let server = Server::start(manager_of(dir.path(), vec![a]), offload);
    let mut busy = server.connect(1);
    let mut closing = server.connect(2);
    // Set the timeout before peer shutdown. macOS rejects later timeout changes.
    closing
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    for id in 0..MAX_PENDING_PER_REQUESTER {
        server.send(1, request(json!(id), "figmog_stats", json!({})));
    }
    server.send(1, request(json!("over"), "figmog_stats", json!({})));
    let refused: Value = serde_json::from_str(&read_line(&mut busy)).unwrap();
    assert_eq!(refused["id"], "over");
    assert_eq!(refused["error"]["code"], -32000, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too many pending requests"),
        "{refused}"
    );
    // Inline reads from another connection are unaffected.
    let mut other = server.connect(3);
    server.send(3, request(json!(1), "figmog_path", json!({"id": "10:1"})));
    let node: Value = serde_json::from_str(&read_line(&mut other)).unwrap();
    assert_eq!(node["id"], 1);
    assert!(node.get("error").is_none(), "{node}");

    // Connection 2 queues a read behind the stalled pool, then closes.
    server.send(2, request(json!(1), "figmog_text", json!({})));
    std::thread::sleep(Duration::from_millis(50));
    server.registry.unregister(2);
    for _ in 0..MAX_PENDING_PER_REQUESTER {
        release.send(()).unwrap();
    }
    let mut ids: Vec<u64> = (0..MAX_PENDING_PER_REQUESTER)
        .map(|_| {
            let frame: Value = serde_json::from_str(&read_line(&mut busy)).unwrap();
            assert!(frame.get("error").is_none(), "{frame}");
            frame["id"].as_u64().unwrap()
        })
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        (0..MAX_PENDING_PER_REQUESTER as u64).collect::<Vec<_>>()
    );
    drop(release);
    let names: Vec<String> = seen.try_iter().collect();
    assert_eq!(names.len(), MAX_PENDING_PER_REQUESTER, "{names:?}");
    assert!(
        !names.iter().any(|n| n == "figmog_text"),
        "closed connection's read ran"
    );
    let mut line = String::new();
    assert!(
        closing.read_line(&mut line).map_or(true, |n| n == 0),
        "closed connection got {line:?}"
    );
    server.stop();
}

/// A synthetic component library: `sets` component sets of `variants`
/// variants each, one page.
fn synthetic_library(sets: usize, variants: usize) -> Value {
    let mut children = Vec::new();
    let mut components = serde_json::Map::new();
    let mut component_sets = serde_json::Map::new();
    for set in 0..sets {
        let set_id = format!("{}:0", set + 10);
        let kids: Vec<Value> = (0..variants)
            .map(|v| {
                let id = format!("{}:{}", set + 10, v + 1);
                components.insert(
                    id.clone(),
                    json!({"key": format!("k{set}-{v}"), "name": format!("Variant={v}"),
                        "description": "", "componentSetId": set_id, "remote": false}),
                );
                json!({"id": id, "name": format!("Variant={v}"), "type": "COMPONENT", "children": []})
            })
            .collect();
        component_sets.insert(
            set_id.clone(),
            json!({"key": format!("ks{set}"), "name": format!("Set {set}"), "description": "", "remote": false}),
        );
        children.push(
            json!({"id": set_id, "name": format!("Set {set}"), "type": "COMPONENT_SET",
            "componentPropertyDefinitions": {"Variant": {"type": "VARIANT", "defaultValue": "0"}},
            "children": kids}),
        );
    }
    json!({"name": "Library", "version": "1", "lastModified": "2026-01-01T12:00:00Z",
        "document": {"id": "0:0", "name": "Document", "type": "DOCUMENT", "children": [
            {"id": "0:1", "name": "Components", "type": "CANVAS", "children": children}]},
        "components": components, "componentSets": component_sets})
}

fn mirrored_doc(dir: &std::path::Path, key: &str, doc: Value) -> FileSession {
    let mut session = sessions::open_session_with_api(
        dir.join(key).join("db"),
        key.into(),
        Some(key),
        Some(Arc::new(DocApi(doc))),
    )
    .unwrap();
    session.request_pull(false, Duration::ZERO);
    let deadline = Instant::now() + Duration::from_secs(300);
    while !session.mirrored {
        session.maintain(true);
        assert!(Instant::now() < deadline, "synthetic pull timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
    session
}

/// `figmog_components` is O(sets × components): on a 2k-set × 20k-component
/// library it is far too slow for the loop, which is why it runs on a
/// read worker: `cargo test --lib measure_components_cost -- --ignored --nocapture`.
#[test]
#[ignore = "manual timing on a 20k-component synthetic library"]
fn measure_components_cost() {
    let dir = tempfile::tempdir().unwrap();
    let session = mirrored_doc(dir.path(), KEY_A, synthetic_library(2_000, 10));
    for (name, args) in [
        ("figmog_components", json!({})),
        ("figmog_pages", json!({})),
        ("figmog_node", json!({"id": "1500:0", "children": true})),
        ("figmog_path", json!({"id": "1500:3"})),
    ] {
        let start = Instant::now();
        let out = session.dispatch(name, &args);
        let bytes = match &out {
            Ok(ToolOutput::Json(v)) => v.to_string().len(),
            _ => 0,
        };
        eprintln!("{name} {args}: {:?}, {bytes} B", start.elapsed());
    }
}

/// `figmog_components` runs on a read worker, never inline.
#[test]
fn components_runs_on_a_read_worker() {
    let dir = tempfile::tempdir().unwrap();
    let a = mirrored_doc(dir.path(), KEY_A, synthetic_library(20, 5));
    let line = request(json!(1), "figmog_components", json!({}));
    let expected = inline_frame(&a, &line, ToolErrorMode::McpContent);
    let mut manager = manager_of(dir.path(), vec![a]);
    let (mut offload, seen, release) = gated_offload("figmog_components");
    let call = begin(&mut manager, &mut offload, &line).expect("components is deferred");
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(10)).unwrap(),
        "figmog_components"
    );
    release.send(()).unwrap();
    assert_eq!(finish(&offload, call), expected);
}

/// A read still running when the exit deadline passes does not hold
/// shutdown: serve returns within the deadline plus a small margin, the
/// read is detached with its own lease, and nothing reopens its store.
#[test]
fn shutdown_is_bounded_by_the_exit_deadline_despite_a_stalled_read() {
    let dir = tempfile::tempdir().unwrap();
    let a = mirrored(dir.path(), KEY_A, 500);
    let (mut offload, seen, release) = gated_offload("figmog_stats");
    let drain = Duration::from_millis(300);
    offload.exit_drain = drain;
    let server = Server::start(manager_of(dir.path(), vec![a]), offload);
    let mut client = server.connect(1);
    // Set the timeout before peer shutdown. macOS rejects later timeout changes.
    client
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    server.send(1, request(json!(1), "figmog_stats", json!({})));
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(10)).unwrap(),
        "figmog_stats"
    );
    // Configure before shutdown: macOS rejects timeout changes on an
    // already-closed Unix socket.
    client
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let start = Instant::now();
    server.stop();
    let elapsed = start.elapsed();
    assert!(
        elapsed < drain + Duration::from_millis(700),
        "shutdown took {elapsed:?} with a {drain:?} exit deadline"
    );
    // The stalled read's response was not owed past the deadline.
    let mut line = String::new();
    assert!(
        client.read_line(&mut line).map_or(true, |n| n == 0),
        "{line:?}"
    );
    // The detached read still completes against its own leased view.
    release.send(()).unwrap();
}

/// All node payloads use workers, including wide children and raw geometry.
#[test]
fn wide_and_geometry_heavy_nodes_run_on_workers_without_blocking_inline_reads() {
    for wide in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let children: Vec<Value> = (0..if wide { 512 } else { 1 })
            .map(|n| {
                json!({"id": format!("1:{n}"), "name": "Shape", "type": "VECTOR",
                "fillGeometry": [{"path": "M0 0 L1 1 ".repeat(if wide { 1 } else { 100_000 })}]})
            })
            .collect();
        let doc = json!({"name": "Wide or geometry-heavy", "version": "1",
        "lastModified": "2026-01-01T12:00:00Z", "document": {
            "id": "0:0", "name": "Document", "type": "DOCUMENT", "children": [{
                "id": "0:1", "name": "Page", "type": "CANVAS", "children": children
            }]}});
        let session = mirrored_doc(dir.path(), KEY_A, doc);
        let line = request(
            json!(1),
            "figmog_node",
            json!({"id": if wide { "0:1" } else { "1:0" }, "children": true}),
        );
        let expected = inline_frame(&session, &line, ToolErrorMode::McpContent);
        let mut manager = manager_of(dir.path(), vec![session]);
        let (mut offload, seen, release) = gated_offload("figmog_node");
        let call = begin(&mut manager, &mut offload, &line).expect("node is deferred");
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(10)).unwrap(),
            "figmog_node"
        );
        let pages = begin(
            &mut manager,
            &mut offload,
            &request(json!(2), "figmog_pages", json!({})),
        )
        .expect_err("pages stays inline");
        assert_eq!(pages["result"]["isError"], false, "{pages}");
        release.send(()).unwrap();
        assert_eq!(finish(&offload, call), expected);
    }
}

#[test]
fn global_read_queue_rejects_overflow_without_leaking_leases_or_replies() {
    let dir = tempfile::tempdir().unwrap();
    let session = mirrored(dir.path(), KEY_A, 100);
    let mut manager = manager_of(dir.path(), vec![session]);
    let (mut offload, seen, release) = gated_offload("*");
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 4);
    let mut calls = Vec::new();
    for id in 0..workers {
        calls.push(
            begin(
                &mut manager,
                &mut offload,
                &request(json!(id), "figmog_stats", json!({})),
            )
            .unwrap(),
        );
    }
    for _ in 0..workers {
        seen.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    for id in 0..super::super::reads::MAX_QUEUED_READS {
        calls.push(
            begin(
                &mut manager,
                &mut offload,
                &request(json!(id + workers), "figmog_stats", json!({})),
            )
            .unwrap(),
        );
    }
    let over = begin(
        &mut manager,
        &mut offload,
        &request(json!("over"), "figmog_stats", json!({})),
    )
    .unwrap();
    let refused: Value = serde_json::from_str(&finish(&offload, over)).unwrap();
    assert_eq!(refused["id"], "over");
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(
        refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("too many queued reads")
    );

    // Dequeuing one job frees a slot even while that next job is paused.
    release.send(()).unwrap();
    seen.recv_timeout(Duration::from_secs(10)).unwrap();
    calls.push(
        begin(
            &mut manager,
            &mut offload,
            &request(json!("recovered"), "figmog_stats", json!({})),
        )
        .unwrap(),
    );
    for _ in 0..calls.len() {
        release.send(()).unwrap();
    }
    let mut tokens = std::collections::BTreeSet::new();
    for _ in 0..calls.len() {
        let done = offload
            .done_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        assert!(tokens.insert(done.token), "duplicate reply");
        let call = calls
            .iter()
            .find(|call| call.token() == done.token)
            .unwrap()
            .clone();
        let response: Value = serde_json::from_str(&answer_frame(done.answer, call)).unwrap();
        assert_eq!(response["result"]["isError"], false, "{response}");
    }
    assert!(offload.done_rx.try_recv().is_err());
    // Every accepted and refused task released its lease, so reset proceeds.
    manager.begin_reset(KEY_A, StoreReset::Reopen).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let finished = manager.service_resets();
        if !finished.is_empty() {
            assert_eq!(finished, vec![(KEY_A.to_string(), Ok(()))]);
            break;
        }
        assert!(Instant::now() < deadline, "read queue leaked a lease");
        std::thread::yield_now();
    }
}
