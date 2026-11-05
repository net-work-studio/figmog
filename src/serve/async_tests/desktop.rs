//! NWS-2959: the Figma desktop app is re-detected after startup. Its tools
//! appear and disappear without a restart, `figmog_status` follows, and
//! only clients that sent `initialize` hear `tools/list_changed`.
use super::*;
use crate::upstream::UpstreamError;

/// Generous bound for a frame that is expected (probes run every few ms).
const EXPECT: Duration = Duration::from_secs(3);

/// A desktop server the test switches on and off.
struct Switchable {
    up: Arc<AtomicBool>,
    pings: Arc<AtomicUsize>,
    handshakes: Arc<AtomicUsize>,
    current: Vec<Value>,
}

fn desktop_tools() -> Vec<Value> {
    vec![json!({"name": "get_code", "description": "Code", "inputSchema": {"type": "object"}})]
}

fn down() -> UpstreamError {
    UpstreamError::Unreachable("connection refused".into())
}

impl UpstreamMcp for Switchable {
    fn initialize(&mut self) -> Result<(), UpstreamError> {
        self.handshakes.fetch_add(1, Ordering::Relaxed);
        if !self.up.load(Ordering::Relaxed) {
            return Err(down());
        }
        self.current = desktop_tools();
        Ok(())
    }
    fn tools(&self) -> &[Value] {
        &self.current
    }
    fn call(&mut self, _: &str, _: &Value) -> Result<Value, UpstreamError> {
        if !self.up.load(Ordering::Relaxed) {
            return Err(down());
        }
        Ok(json!({"content": [{"type": "text", "text": "live"}], "isError": false}))
    }
    fn ping(&mut self) -> Result<(), UpstreamError> {
        self.pings.fetch_add(1, Ordering::Relaxed);
        if self.up.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(down())
        }
    }
}

struct Desktop {
    up: Arc<AtomicBool>,
    pings: Arc<AtomicUsize>,
    handshakes: Arc<AtomicUsize>,
}

/// A desktop worker over a [`Switchable`] that starts `up` or not.
fn desktop(up: bool, cadence: ProbeCadence) -> (Desktop, DesktopWorker) {
    let handle = Desktop {
        up: Arc::new(AtomicBool::new(up)),
        pings: Arc::default(),
        handshakes: Arc::default(),
    };
    let mut upstream = Switchable {
        up: handle.up.clone(),
        pings: handle.pings.clone(),
        handshakes: handle.handshakes.clone(),
        current: Vec::new(),
    };
    let connected = upstream.initialize().is_ok();
    let worker = DesktopWorker::spawn_with(upstream, connected, cadence);
    (handle, worker)
}

fn fast() -> ProbeCadence {
    ProbeCadence {
        missing: Duration::from_millis(20),
        live: Duration::from_millis(20),
    }
}

fn never() -> ProbeCadence {
    ProbeCadence {
        missing: Duration::from_secs(3600),
        live: Duration::from_secs(3600),
    }
}

fn request(id: Value, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn call(id: Value, name: &str) -> String {
    request(id, "tools/call", json!({"name": name, "arguments": {}}))
}

/// A running loop with stdio and two socket connections: 1 sends
/// `initialize`, 2 (like the CLI) does not.
struct Server {
    tx: mpsc::Sender<Incoming>,
    stdout: mpsc::Receiver<Value>,
    buffered: std::cell::RefCell<std::collections::VecDeque<Value>>,
    conns: [BufReader<UnixStream>; 2],
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
    _harness: (
        mpsc::Sender<()>,
        mpsc::Receiver<&'static str>,
        tempfile::TempDir,
    ),
}

impl Server {
    fn start(worker: Option<DesktopWorker>) -> Self {
        let Harness {
            release,
            entered,
            manager,
            _dir,
            ..
        } = Harness::new();
        let (tx, rx) = mpsc::channel();
        let registry: ConnRegistry = Arc::default();
        let conns = [1, 2].map(|conn_id| {
            let (client, server) = UnixStream::pair().unwrap();
            client.set_read_timeout(Some(EXPECT)).unwrap();
            registry.register(conn_id, server).unwrap();
            BufReader::new(client)
        });
        let (stdout_tx, stdout) = mpsc::channel::<Value>();
        let thread = std::thread::spawn(move || {
            serve_loop_with_output(
                manager,
                worker,
                true,
                false,
                Duration::from_secs(3600),
                Instant::now() + Duration::from_secs(3600),
                rx,
                registry,
                move |frame: &str| {
                    stdout_tx
                        .send(serde_json::from_str::<Value>(frame).unwrap())
                        .is_ok()
                },
            )
        });
        Self {
            tx,
            stdout,
            buffered: Default::default(),
            conns,
            thread: Some(thread),
            _harness: (release, entered, _dir),
        }
    }

    fn stdio(&self, line: String) -> Value {
        let id = serde_json::from_str::<Value>(&line).unwrap()["id"].clone();
        self.tx.send(Incoming::Stdio(line)).unwrap();
        loop {
            let frame = self.stdout.recv_timeout(EXPECT).expect("a stdio response");
            if frame.get("id") == Some(&id) {
                return frame;
            }
            self.buffered.borrow_mut().push_back(frame);
        }
    }

    fn next_stdio(&self) -> Value {
        self.buffered
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| self.stdout.recv_timeout(EXPECT).expect("a stdio frame"))
    }

    fn assert_stdio_silent(&self, for_: Duration) {
        assert!(
            self.buffered.borrow().is_empty(),
            "unexpected buffered stdio frames"
        );
        if let Ok(frame) = self.stdout.recv_timeout(for_) {
            panic!("unexpected stdio frame {frame}");
        }
    }

    fn socket(&mut self, conn: usize, line: String) -> Value {
        self.tx
            .send(Incoming::Socket(conn as u64 + 1, line))
            .unwrap();
        self.next_socket(conn)
    }

    fn next_socket(&mut self, conn: usize) -> Value {
        let mut line = String::new();
        self.conns[conn].read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn assert_socket_silent(&mut self, conn: usize) {
        let reader = &mut self.conns[conn];
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut line = String::new();
        let error = reader.read_line(&mut line).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "unexpected frame {line:?} ({error})"
        );
        reader.get_ref().set_read_timeout(Some(EXPECT)).unwrap();
    }

    fn tool_names(&self, id: u64) -> Vec<String> {
        let response = self.stdio(request(json!(id), "tools/list", json!({})));
        assert_eq!(response["id"], id, "{response}");
        response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn upstream_status(&self, id: u64) -> Value {
        let response = self.stdio(call(json!(id), "figmog_status"));
        assert_eq!(response["id"], id, "{response}");
        text(&response)["upstream"].clone()
    }

    fn finish(mut self) {
        // `figmog_status` on the unmirrored harness file queued a gated
        // pull: release it so shutdown need not wait out its gate.
        let _ = self._harness.0.send(());
        self.tx.send(Incoming::StdinClosed).unwrap();
        self.thread.take().unwrap().join().unwrap().unwrap();
    }
}

fn text(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn list_changed() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
}

/// Both initializing clients get `initialize`'s `listChanged` capability
/// and later the notifications; the non-initializing socket client gets
/// neither.
fn initialize_clients(server: &mut Server, list_changed: bool) {
    let init = request(json!(1), "initialize", json!({}));
    let response = server.stdio(init.clone());
    assert_eq!(
        response["result"]["capabilities"]["tools"],
        json!({"listChanged": list_changed})
    );
    let response = server.socket(0, init);
    assert_eq!(response["id"], 1);
}

#[test]
fn desktop_tools_appear_and_disappear_with_notifications_only_to_initialized_clients() {
    let (desktop, worker) = desktop(false, fast());
    let mut server = Server::start(Some(worker));
    initialize_clients(&mut server, true);
    assert!(!server.tool_names(2).contains(&"get_code".to_string()));
    assert_eq!(server.upstream_status(3), "unreachable");

    // The desktop app starts: the next probe finds it.
    desktop.up.store(true, Ordering::Relaxed);
    assert_eq!(server.next_stdio(), list_changed());
    assert_eq!(server.next_socket(0), list_changed());
    let names = server.tool_names(4);
    assert_eq!(names.len(), 22);
    assert_eq!(names.last().unwrap(), "get_code");
    assert_eq!(server.upstream_status(5), "connected");
    let response = server.stdio(call(json!(6), "get_code"));
    assert_eq!(response["result"]["content"][0]["text"], "live");

    // Liveness pings that confirm the current state announce nothing.
    let pings = desktop.pings.load(Ordering::Relaxed);
    server.assert_stdio_silent(Duration::from_millis(200));
    assert!(desktop.pings.load(Ordering::Relaxed) > pings);
    server.assert_socket_silent(0);

    // The desktop app quits: the next ping fails, the handshake fails, and
    // its tools go away.
    desktop.up.store(false, Ordering::Relaxed);
    assert_eq!(server.next_stdio(), list_changed());
    assert_eq!(server.next_socket(0), list_changed());
    assert!(!server.tool_names(7).contains(&"get_code".to_string()));
    assert_eq!(server.upstream_status(8), "unreachable");

    // A socket client that never sent `initialize` (the CLI) heard none of
    // it; it shares JSON-RPC ids with the other clients and still gets
    // exactly its own reply.
    server.assert_socket_silent(1);
    let response = server.socket(1, request(json!(1), "tools/list", json!({})));
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 21);
    server.assert_socket_silent(1);
    server.finish();
}

#[test]
fn calling_a_missing_desktop_tool_rechecks_the_desktop_at_once() {
    let (desktop, worker) = desktop(false, never());
    let mut server = Server::start(Some(worker));
    initialize_clients(&mut server, true);
    let handshakes = desktop.handshakes.load(Ordering::Relaxed);
    desktop.up.store(true, Ordering::Relaxed);

    // Refused right away as before; the probe it triggers finds the app.
    let response = server.stdio(call(json!(2), "get_code"));
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(
        response["result"]["content"][0]["text"],
        "unknown tool: get_code"
    );
    assert_eq!(server.next_stdio(), list_changed());
    assert_eq!(desktop.handshakes.load(Ordering::Relaxed), handshakes + 1);
    let response = server.stdio(call(json!(3), "get_code"));
    assert_eq!(response["result"]["content"][0]["text"], "live");

    // An unknown local tool name never probes.
    let response = server.stdio(call(json!(4), "figmog_nope"));
    assert_eq!(response["result"]["isError"], true);
    server.assert_stdio_silent(Duration::from_millis(100));
    assert_eq!(desktop.handshakes.load(Ordering::Relaxed), handshakes + 1);
    server.finish();
}

#[test]
fn a_failed_desktop_call_rechecks_and_removes_a_vanished_desktop() {
    let (desktop, worker) = desktop(true, never());
    let mut server = Server::start(Some(worker));
    initialize_clients(&mut server, true);
    assert_eq!(server.upstream_status(2), "connected");
    desktop.up.store(false, Ordering::Relaxed);

    let response = server.stdio(call(json!(3), "get_code"));
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("upstream unreachable"),
        "{response}"
    );
    assert_eq!(server.next_stdio(), list_changed());
    assert_eq!(server.next_socket(0), list_changed());
    assert_eq!(server.upstream_status(4), "unreachable");
    server.finish();
}

#[test]
fn a_failed_call_to_a_live_desktop_changes_nothing() {
    // A transient failure (the desktop answers the follow-up ping) keeps
    // its tools and announces nothing: no flapping.
    struct Flaky(Switchable);
    impl UpstreamMcp for Flaky {
        fn initialize(&mut self) -> Result<(), UpstreamError> {
            self.0.initialize()
        }
        fn tools(&self) -> &[Value] {
            self.0.tools()
        }
        fn call(&mut self, _: &str, _: &Value) -> Result<Value, UpstreamError> {
            Err(UpstreamError::Unreachable("timed out".into()))
        }
        fn ping(&mut self) -> Result<(), UpstreamError> {
            self.0.ping()
        }
    }
    let handle = Desktop {
        up: Arc::new(AtomicBool::new(true)),
        pings: Arc::default(),
        handshakes: Arc::default(),
    };
    let mut upstream = Flaky(Switchable {
        up: handle.up.clone(),
        pings: handle.pings.clone(),
        handshakes: handle.handshakes.clone(),
        current: Vec::new(),
    });
    upstream.initialize().unwrap();
    let mut server = Server::start(Some(DesktopWorker::spawn_with(upstream, true, never())));
    initialize_clients(&mut server, true);
    let response = server.stdio(call(json!(2), "get_code"));
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
    // The call's transport error dropped the session, so the probe
    // re-handshakes (rather than pinging) and finds the same tools.
    let deadline = Instant::now() + EXPECT;
    while handle.handshakes.load(Ordering::Relaxed) < 2 {
        assert!(Instant::now() < deadline, "no probe after the failed call");
        std::thread::sleep(Duration::from_millis(5));
    }
    server.assert_stdio_silent(Duration::from_millis(100));
    assert_eq!(server.upstream_status(3), "connected");
    assert_eq!(server.tool_names(4).len(), 22);
    server.finish();
}

#[test]
fn without_an_upstream_nothing_is_probed_or_announced() {
    // `--no-upstream` builds no desktop worker: there is nothing to probe,
    // a desktop tool name is simply unknown, and status stays disabled.
    let mut server = Server::start(None);
    initialize_clients(&mut server, false);
    let response = server.stdio(call(json!(2), "get_code"));
    assert_eq!(
        response["result"]["content"][0]["text"],
        "unknown tool: get_code"
    );
    assert_eq!(server.upstream_status(3), "disabled");
    server.assert_stdio_silent(Duration::from_millis(100));
    server.assert_socket_silent(0);
    server.finish();
}
