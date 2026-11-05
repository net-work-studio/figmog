//! NWS-2955: image cache misses and desktop calls run off the main loop.
//! A gated call must not delay local reads from another client or from the
//! same stdio stream, and its response must still reach the connection and
//! JSON-RPC id that asked — including when two connections reuse an id.
use super::*;
use crate::upstream::UpstreamError;

/// Reads must answer well inside this while a gated call is blocked.
const PROMPT: Duration = Duration::from_millis(500);

fn frame(id: Value, name: &str, args: Value) -> String {
    json!({"jsonrpc":"2.0", "id": id, "method":"tools/call",
        "params":{"name": name, "arguments": args}})
    .to_string()
}

fn read_frame(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// A socket client: `reader` reads responses, `server` is the registered
/// write half the loop answers through.
fn connection(registry: &ConnRegistry, conn_id: u64) -> BufReader<UnixStream> {
    let (client, server) = UnixStream::pair().unwrap();
    client.set_read_timeout(Some(PROMPT)).unwrap();
    server.set_write_timeout(Some(PROMPT)).unwrap();
    registry.register(conn_id, server).unwrap();
    BufReader::new(client)
}

fn assert_silent(reader: &mut BufReader<UnixStream>) {
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
}

fn text(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A desktop server whose every call blocks until released, answering
/// scripted results in order.
struct GatedUpstream {
    tools: Vec<Value>,
    results: std::collections::VecDeque<Value>,
    entered: mpsc::Sender<String>,
    release: mpsc::Receiver<()>,
}

impl UpstreamMcp for GatedUpstream {
    fn initialize(&mut self) -> Result<(), UpstreamError> {
        Ok(())
    }
    fn tools(&self) -> &[Value] {
        &self.tools
    }
    fn call(&mut self, name: &str, _: &Value) -> Result<Value, UpstreamError> {
        self.entered.send(name.to_string()).unwrap();
        self.release
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| UpstreamError::Unreachable(e.to_string()))?;
        Ok(self.results.pop_front().unwrap())
    }
}

fn mirrored_harness() -> Harness {
    let mut h = Harness::new();
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    h
}

#[test]
fn slow_desktop_call_does_not_delay_local_reads_and_answers_its_requester() {
    let h = mirrored_harness();
    let Harness {
        release: _pull_release,
        entered: _pull_entered,
        api: _,
        manager,
        _dir: _dir_guard,
        gate_stages: _,
    } = h;
    let (entered_tx, entered) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let first = json!({"content":[{"type":"text","text":"first"}], "isError": false});
    let second = json!({"content":[{"type":"image","data":"AA==","mimeType":"image/png"}],
        "isError": true, "_meta": {"verbatim": [1, 2]}});
    let desktop = DesktopWorker::spawn(GatedUpstream {
        tools: vec![json!({"name":"get_screenshot", "inputSchema":{"type":"object"}})],
        results: [first.clone(), second.clone()].into(),
        entered: entered_tx,
        release: gate,
    });

    let (tx, rx) = mpsc::channel();
    let registry: ConnRegistry = Arc::default();
    let mut conn1 = connection(&registry, 1);
    let mut conn2 = connection(&registry, 2);
    let mut gone = connection(&registry, 3);
    let (stdout_tx, stdout) = mpsc::channel::<Value>();
    let server = std::thread::spawn(move || {
        serve_loop_with_output(
            manager,
            Some(desktop),
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

    // Stdio's desktop call blocks the desktop worker...
    tx.send(Incoming::Stdio(frame(
        json!(1),
        "get_screenshot",
        json!({}),
    )))
    .unwrap();
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "get_screenshot"
    );
    // ...while a later read on the same stdio stream answers first.
    let started = Instant::now();
    tx.send(Incoming::Stdio(frame(json!(2), "figmog_files", json!({}))))
        .unwrap();
    let files = stdout.recv_timeout(PROMPT).unwrap();
    assert!(started.elapsed() < PROMPT);
    assert_eq!(files["id"], 2);
    assert_eq!(text(&files)[0]["key"], "ABC1234567890");

    // Socket desktop calls queue behind it (same id as stdio's), and a
    // vanished client's call is answered into the void.
    tx.send(Incoming::Socket(
        1,
        frame(json!(1), "get_screenshot", json!({})),
    ))
    .unwrap();
    tx.send(Incoming::Socket(
        3,
        frame(json!(1), "get_screenshot", json!({})),
    ))
    .unwrap();
    drop(gone.get_mut().shutdown(std::net::Shutdown::Both));
    drop(gone);
    // Another connection reusing id 1 reads promptly.
    let started = Instant::now();
    tx.send(Incoming::Socket(
        2,
        frame(json!(1), "figmog_status", json!({})),
    ))
    .unwrap();
    let status = read_frame(&mut conn2);
    assert!(started.elapsed() < PROMPT);
    assert_eq!(status["id"], 1);
    assert_eq!(text(&status)["version"], "1");

    // Releasing answers each requester, in its own transport's framing,
    // with the upstream result verbatim.
    release.send(()).unwrap();
    let answer = stdout.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        answer,
        json!({"jsonrpc":"2.0", "id": 1, "result": first}),
        "stdio gets the first desktop result"
    );
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "get_screenshot"
    );
    release.send(()).unwrap();
    conn1
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(
        read_frame(&mut conn1),
        json!({"jsonrpc":"2.0", "id": 1, "result": second})
    );
    // The closed connection's call still runs; its failed delivery must
    // not disturb the loop. Fail it upstream (gate dropped) after it
    // starts, then prove the loop is still serving.
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "get_screenshot"
    );
    drop(release);
    tx.send(Incoming::Socket(
        2,
        frame(json!(9), "figmog_files", json!({})),
    ))
    .unwrap();
    conn2
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(read_frame(&mut conn2)["id"], 9);
    assert_silent(&mut conn2);
    assert_silent(&mut conn1);
    assert!(stdout.try_recv().is_err(), "no stray stdio frames");

    tx.send(Incoming::StdinClosed).unwrap();
    server.join().unwrap().unwrap();
}

#[test]
fn slow_image_download_does_not_delay_local_reads_and_answers_its_requester() {
    let h = mirrored_harness();
    h.api.block_images.store(true, Ordering::Relaxed);
    let Harness {
        release,
        entered,
        api: _,
        manager,
        _dir: _dir_guard,
        gate_stages: _,
    } = h;
    let (tx, rx) = mpsc::channel();
    let registry: ConnRegistry = Arc::default();
    let mut conn1 = connection(&registry, 1);
    let mut conn2 = connection(&registry, 2);
    let mut conn3 = connection(&registry, 3);
    let (stdout_tx, stdout) = mpsc::channel::<Value>();
    let server = std::thread::spawn(move || {
        serve_loop_with_output(
            manager,
            None,
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

    let images = |id: u64| frame(json!(id), "figmog_images", json!({"ids": ["0:0"]}));
    tx.send(Incoming::Socket(1, images(7))).unwrap();
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "render"
    );
    // A second image request for the same file queues behind the first
    // instead of failing busy or blocking the loop.
    tx.send(Incoming::Socket(2, images(7))).unwrap();

    // Reads from another connection (same id) and from stdio stay prompt.
    let started = Instant::now();
    tx.send(Incoming::Socket(
        3,
        frame(json!(7), "figmog_status", json!({})),
    ))
    .unwrap();
    let status = read_frame(&mut conn3);
    assert!(started.elapsed() < PROMPT);
    assert_eq!(status["id"], 7);
    assert_eq!(text(&status)["version"], "1");
    let started = Instant::now();
    tx.send(Incoming::Stdio(frame(
        json!(7),
        "figmog_node",
        json!({"id": "0:0"}),
    )))
    .unwrap();
    let node = stdout.recv_timeout(PROMPT).unwrap();
    assert!(started.elapsed() < PROMPT);
    assert_eq!(node["id"], 7);
    assert_eq!(node["result"]["isError"], false, "{node}");
    assert_silent(&mut conn1);
    assert_silent(&mut conn2);

    // Each image response reaches its own connection, in arrival order.
    release.send(()).unwrap();
    conn1
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let first = read_frame(&mut conn1);
    assert_eq!(first["id"], 7);
    assert!(first["result"]["content"].is_array(), "{first}");
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "render"
    );
    assert_silent(&mut conn2);
    release.send(()).unwrap();
    conn2
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let second = read_frame(&mut conn2);
    assert_eq!(second["id"], 7);
    assert_eq!(second["result"], first["result"]);
    assert_silent(&mut conn1);
    assert_silent(&mut conn3);
    assert!(stdout.try_recv().is_err(), "no stray stdio frames");

    tx.send(Incoming::StdinClosed).unwrap();
    server.join().unwrap().unwrap();
}

#[test]
fn a_pull_taking_the_worker_answers_queued_images_busy() {
    let mut h = mirrored_harness();
    h.api.block_images.store(true, Ordering::Relaxed);
    let (answers_tx, answers) = mpsc::channel();
    let reply = |tag: &'static str| -> sessions::ImageReply {
        let answers = answers_tx.clone();
        Box::new(move |result| {
            answers
                .send((tag, result.map(|items| items.len())))
                .unwrap()
        })
    };
    let session = &mut h.manager.sessions[0];
    let ids = vec!["0:0".to_string()];
    session
        .images(&ids, "png", None, true, Default::default(), reply("first"))
        .unwrap();
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "render"
    );
    session
        .images(&ids, "png", None, true, Default::default(), reply("queued"))
        .unwrap();
    // A pull requested meanwhile waits for the worker, then takes it.
    let accepted = session.request_pull(false, Duration::from_secs(3600));
    assert_eq!(accepted["state"], "queued");
    h.release.send(()).unwrap();
    assert_eq!(
        answers.recv_timeout(Duration::from_secs(5)).unwrap(),
        ("first", Ok(1))
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let (tag, result) = loop {
        h.manager.sessions[0].maintain(true);
        if let Ok(answer) = answers.try_recv() {
            break answer;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    };
    assert_eq!(tag, "queued");
    assert!(result.unwrap_err().contains("in progress"));
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
}

/// Spawn a serve loop over `manager` whose stdout frames land on the
/// returned receiver.
fn spawn_stdio_loop(
    manager: SessionManager,
    desktop: Option<DesktopWorker>,
) -> (
    mpsc::Sender<Incoming>,
    mpsc::Receiver<Value>,
    std::thread::JoinHandle<Result<(), String>>,
) {
    let (tx, rx) = mpsc::channel();
    let (stdout_tx, stdout) = mpsc::channel::<Value>();
    let server = std::thread::spawn(move || {
        serve_loop_with_output(
            manager,
            desktop,
            true,
            false,
            Duration::from_secs(3600),
            Instant::now() + Duration::from_secs(3600),
            rx,
            Arc::default(),
            move |frame: &str| {
                stdout_tx
                    .send(serde_json::from_str::<Value>(frame).unwrap())
                    .is_ok()
            },
        )
    });
    (tx, stdout, server)
}

#[test]
fn stdin_eof_still_delivers_an_in_flight_desktop_response() {
    let h = Harness::new();
    let (entered_tx, entered) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let result = json!({"content":[{"type":"text","text":"late"}], "isError": false});
    let desktop = DesktopWorker::spawn(GatedUpstream {
        tools: vec![json!({"name":"get_screenshot", "inputSchema":{"type":"object"}})],
        results: [result.clone()].into(),
        entered: entered_tx,
        release: gate,
    });
    let Harness {
        release: _r,
        entered: _e,
        manager,
        _dir: _dir_guard,
        ..
    } = h;
    let (tx, stdout, server) = spawn_stdio_loop(manager, Some(desktop));
    tx.send(Incoming::Stdio(frame(
        json!("d"),
        "get_screenshot",
        json!({}),
    )))
    .unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    tx.send(Incoming::StdinClosed).unwrap();
    release.send(()).unwrap();
    assert_eq!(
        stdout.recv_timeout(Duration::from_secs(5)).unwrap(),
        json!({"jsonrpc":"2.0", "id":"d", "result": result})
    );
    server.join().unwrap().unwrap();
}

#[test]
fn stdin_eof_still_delivers_an_in_flight_image_response() {
    let h = mirrored_harness();
    h.api.block_images.store(true, Ordering::Relaxed);
    let Harness {
        release,
        entered,
        manager,
        _dir: _dir_guard,
        ..
    } = h;
    let (tx, stdout, server) = spawn_stdio_loop(manager, None);
    tx.send(Incoming::Stdio(frame(
        json!(4),
        "figmog_images",
        json!({"ids": ["0:0"]}),
    )))
    .unwrap();
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "render"
    );
    tx.send(Incoming::StdinClosed).unwrap();
    release.send(()).unwrap();
    let response = stdout.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(response["id"], 4);
    assert!(response["result"]["content"].is_array(), "{response}");
    server.join().unwrap().unwrap();
}

#[test]
fn a_poll_due_during_images_gets_the_worker_before_queued_images() {
    // Mirrored by watch, so the next metadata poll observes no change.
    let mut h = Harness::new();
    h.mirror_from_watch();
    h.api.block_images.store(true, Ordering::Relaxed);
    h.api.block_meta.store(true, Ordering::Relaxed);
    let (answers_tx, answers) = mpsc::channel();
    let reply = |tag: &'static str| -> sessions::ImageReply {
        let answers = answers_tx.clone();
        Box::new(move |result| answers.send((tag, result.is_ok())).unwrap())
    };
    let hour = Duration::from_secs(3600);
    let ids = vec!["0:0".to_string()];
    let session = &mut h.manager.sessions[0];
    session
        .images(&ids, "png", None, true, Default::default(), reply("first"))
        .unwrap();
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "render"
    );
    // The scheduler's visit finds the worker busy with images...
    assert!(!session.poll_at(hour, Instant::now()));
    session
        .images(&ids, "png", None, true, Default::default(), reply("second"))
        .unwrap();
    h.release.send(()).unwrap();
    assert_eq!(
        answers.recv_timeout(Duration::from_secs(5)).unwrap(),
        ("first", true)
    );
    // ...so the owed poll runs before the queued image request.
    h.wait_for_event_at(Instant::now(), "metadata");
    assert!(
        answers.try_recv().is_err(),
        "queued image waits for the poll"
    );
    h.release.send(()).unwrap();
    h.wait_for_event_at(Instant::now(), "render");
    h.release.send(()).unwrap();
    assert_eq!(
        answers.recv_timeout(Duration::from_secs(5)).unwrap(),
        ("second", true)
    );
}

#[test]
fn no_watch_images_do_not_dispatch_a_due_watch_retry() {
    // Existing mirror, then a failed explicit sync: that arms the watch
    // retry deadline even under --no-watch.
    let mut h = mirrored_harness();
    *h.api.response.lock().unwrap() = Err(ApiError::Network("synthetic".into()));
    h.value("figmog_sync", json!({}));
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    h.release.send(()).unwrap();
    h.wait_for("failed");
    let past_retry = Instant::now() + Duration::from_secs(24 * 3600);
    assert!(
        h.manager.sessions[0]
            .watch_retry_deadline()
            .is_some_and(|due| due <= past_retry),
        "the failed sync armed a watch retry"
    );

    h.api.block_images.store(true, Ordering::Relaxed);
    h.api.block_meta.store(true, Ordering::Relaxed);
    let (answers_tx, answers) = mpsc::channel();
    let reply = |tag: &'static str| -> sessions::ImageReply {
        let answers = answers_tx.clone();
        Box::new(move |result| answers.send((tag, result.is_ok())).unwrap())
    };
    let ids = vec!["0:0".to_string()];
    let session = &mut h.manager.sessions[0];
    session
        .images(&ids, "png", None, true, Default::default(), reply("first"))
        .unwrap();
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "render"
    );
    session
        .images(&ids, "png", None, true, Default::default(), reply("second"))
        .unwrap();
    h.release.send(()).unwrap();
    assert_eq!(
        answers.recv_timeout(Duration::from_secs(5)).unwrap(),
        ("first", true)
    );
    // The retry deadline has passed, but --no-watch (serve's loop passes
    // `watch = false`) dispatches no poll: the queued image runs next.
    let deadline = Instant::now() + Duration::from_secs(5);
    let event = loop {
        h.manager.sessions[0].maintain_at(past_retry, false);
        if let Ok(event) = h.entered.try_recv() {
            break event;
        }
        assert!(Instant::now() < deadline, "queued image never started");
        std::thread::yield_now();
    };
    assert_eq!(event, "render", "no metadata poll under --no-watch");
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false
    );
    h.release.send(()).unwrap();
    assert_eq!(
        answers.recv_timeout(Duration::from_secs(5)).unwrap(),
        ("second", true)
    );
}

#[test]
fn stdin_eof_stops_the_desktop_queue_but_delivers_the_in_flight_call() {
    let h = mirrored_harness();
    h.api.block_images.store(true, Ordering::Relaxed);
    let (entered_tx, desktop_entered) = mpsc::channel();
    let (desktop_release, gate) = mpsc::channel();
    let first = json!({"content":[{"type":"text","text":"a"}], "isError": false});
    let second = json!({"content":[{"type":"text","text":"b"}], "isError": false});
    let desktop = DesktopWorker::spawn(GatedUpstream {
        tools: vec![json!({"name":"set_node_name", "inputSchema":{"type":"object"}})],
        results: [first.clone(), second].into(),
        entered: entered_tx,
        release: gate,
    });
    let Harness {
        release: image_release,
        entered: image_entered,
        manager,
        _dir: _dir_guard,
        ..
    } = h;
    let (tx, stdout, server) = spawn_stdio_loop(manager, Some(desktop));
    // An image request runs and another queues behind it: the queued one's
    // shutting-down error marks the moment the drain began.
    tx.send(Incoming::Stdio(frame(
        json!("img1"),
        "figmog_images",
        json!({"ids": ["0:0"]}),
    )))
    .unwrap();
    assert_eq!(
        image_entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "render"
    );
    tx.send(Incoming::Stdio(frame(
        json!("img2"),
        "figmog_images",
        json!({"ids": ["0:0"]}),
    )))
    .unwrap();
    // Desktop call A blocks upstream; B (a possible write) queues behind.
    tx.send(Incoming::Stdio(frame(
        json!("a"),
        "set_node_name",
        json!({}),
    )))
    .unwrap();
    assert_eq!(
        desktop_entered
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        "set_node_name"
    );
    tx.send(Incoming::Stdio(frame(
        json!("b"),
        "set_node_name",
        json!({}),
    )))
    .unwrap();
    tx.send(Incoming::StdinClosed).unwrap();
    let drained = stdout.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(drained["id"], "img2", "{drained}");
    assert_eq!(drained["result"]["isError"], true, "{drained}");

    desktop_release.send(()).unwrap();
    image_release.send(()).unwrap();
    let mut responses = BTreeMap::new();
    for _ in 0..3 {
        let response = stdout.recv_timeout(Duration::from_secs(5)).unwrap();
        responses.insert(response["id"].as_str().unwrap().to_string(), response);
    }
    assert_eq!(
        responses["a"],
        json!({"jsonrpc":"2.0", "id":"a", "result": first})
    );
    let b = &responses["b"];
    assert_eq!(b["result"]["isError"], true, "{b}");
    assert!(
        b["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("shutting down"),
        "{b}"
    );
    assert!(responses["img1"]["result"]["content"].is_array());
    server.join().unwrap().unwrap();
    // B never reached the upstream.
    assert!(desktop_entered.try_recv().is_err());
}

/// NWS-2955: a vanished stdout skips the drain, but serve still waits (up
/// to the exit deadline) for a read worker still running. The desktop
/// queue must stop before that wait: a queued, possibly mutating call
/// queued behind an in-flight one never reaches the upstream.
#[test]
fn broken_stdout_stops_the_desktop_queue_before_waiting_for_reads() {
    let h = mirrored_harness();
    let (entered_tx, desktop_entered) = mpsc::channel();
    let (desktop_release, gate) = mpsc::channel();
    let result = json!({"content":[{"type":"text","text":"ok"}], "isError": false});
    let desktop = DesktopWorker::spawn(GatedUpstream {
        tools: vec![json!({"name":"set_node_name", "inputSchema":{"type":"object"}})],
        results: [result.clone(), result].into(),
        entered: entered_tx,
        release: gate,
    });
    let Harness {
        manager,
        _dir: _dir_guard,
        ..
    } = h;
    // A read worker that stalls until released.
    let (read_seen_tx, read_seen) = mpsc::channel();
    let (read_release, read_gate) = mpsc::channel::<()>();
    let read_gate = Arc::new(Mutex::new(read_gate));
    let mut offload = Offload::new(Some(desktop));
    offload.exit_drain = Duration::from_secs(3);
    offload.reads.before_read = Some(Arc::new(move |name: &str| {
        let _ = read_seen_tx.send(name.to_string());
        let _ = read_gate
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
    }));
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        serve_loop_with_offload(
            manager,
            offload,
            true,
            false,
            Duration::from_secs(3600),
            Instant::now() + Duration::from_secs(3600),
            rx,
            Arc::default(),
            // Stdout is broken: the first write fails.
            |_: &str| false,
        )
    });
    tx.send(Incoming::Stdio(frame(
        json!("r"),
        "figmog_stats",
        json!({}),
    )))
    .unwrap();
    assert_eq!(
        read_seen.recv_timeout(Duration::from_secs(5)).unwrap(),
        "figmog_stats"
    );
    // Desktop call A blocks upstream; B (a possible write) queues behind.
    tx.send(Incoming::Stdio(frame(
        json!("a"),
        "set_node_name",
        json!({}),
    )))
    .unwrap();
    assert_eq!(
        desktop_entered
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        "set_node_name"
    );
    tx.send(Incoming::Stdio(frame(
        json!("b"),
        "set_node_name",
        json!({}),
    )))
    .unwrap();
    // An immediate answer hits the broken stdout: the loop exits without
    // draining and waits on the stalled read.
    tx.send(Incoming::Stdio(
        json!({"jsonrpc":"2.0", "id":"p", "method":"ping"}).to_string(),
    ))
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // A completes during that wait; B must not be sent upstream.
    desktop_release.send(()).unwrap();
    assert!(
        desktop_entered
            .recv_timeout(Duration::from_millis(700))
            .is_err(),
        "queued desktop call reached the upstream after stdout broke"
    );
    read_release.send(()).unwrap();
    server.join().unwrap().unwrap();
    assert!(desktop_entered.try_recv().is_err());
}
