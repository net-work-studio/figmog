//! Regression coverage through JSON-RPC tools/call and the socket CLI.
use super::*;
use crate::api::{ApiError, FigmaApi, FileMetaResp};
use crate::store;
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};

fn fixture(version: &str) -> Value {
    json!({"name": "Synthetic", "version": version, "lastModified": "2026-01-01T12:00:00Z",
        "document": {"id": "0:0", "name": version, "type": "DOCUMENT"}})
}

struct Api {
    response: Mutex<Result<Value, ApiError>>,
    metadata: Mutex<String>,
    calls: AtomicUsize,
    geometry: Mutex<Vec<bool>>,
    entered: mpsc::Sender<&'static str>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
    block_meta: AtomicBool,
    metadata_error: Mutex<Option<ApiError>>,
    /// Gate every render request (a slow image cache miss).
    block_images: AtomicBool,
}

impl FigmaApi for Api {
    fn file_meta(&self, _: &str) -> Result<FileMetaResp, ApiError> {
        if self.block_meta.load(Ordering::Relaxed) {
            self.entered.send("metadata").unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        if let Some(error) = self.metadata_error.lock().unwrap().take() {
            return Err(error);
        }
        Ok(FileMetaResp {
            name: "Synthetic".into(),
            last_touched_at: self.metadata.lock().unwrap().clone(),
        })
    }
    fn file(&self, _: &str, geometry: bool) -> Result<Value, ApiError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.geometry.lock().unwrap().push(geometry);
        self.entered.send("fetch").unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        std::mem::replace(&mut *self.response.lock().unwrap(), Ok(fixture("2")))
    }
    fn images_render(
        &self,
        _: &str,
        _: &[String],
        _: &str,
        _: Option<f64>,
        _: Option<&str>,
    ) -> Result<Value, ApiError> {
        if self.block_images.load(Ordering::Relaxed) {
            self.entered.send("render").unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        Ok(json!({"images": {}}))
    }
}

/// Dispatch one stdio frame through `handle_tool_call` and, when the call
/// is deferred, wait for its result and apply it exactly as the serve loop
/// does.
fn call_and_wait(
    manager: &mut SessionManager,
    offload: &mut Offload,
    upstream_status: &'static str,
    no_watch: bool,
    interval: Duration,
    tools: &[mcp::ToolDef],
    message: &Value,
) -> Value {
    let live = Arc::new(AtomicBool::new(false));
    let mut handler = FnHandler(|name: &str, args: &Value| {
        handle_tool_call(
            manager,
            offload,
            &live,
            upstream_status,
            interval,
            name,
            args,
        )
    });
    let reply = mcp::dispatch_message(
        &message.to_string(),
        tools,
        &mut handler,
        ToolErrorMode::McpContent,
    )
    .unwrap();
    let call = match reply {
        Reply::Now(response) => return response,
        Reply::Later(call) => call,
    };
    offload.park(&call);
    let mut deadline = Instant::now();
    loop {
        let done = offload
            .done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("deferred call answers");
        if done.token == call.token() {
            let answer = apply_done(manager, no_watch, interval, &mut deadline, done);
            return serde_json::from_str(&answer_frame(answer, call)).unwrap();
        }
    }
}

struct Harness {
    // Drop the release sender before sessions so a failed assertion cannot
    // leave a gate blocking shutdown indefinitely.
    release: mpsc::Sender<()>,
    entered: mpsc::Receiver<&'static str>,
    api: Arc<Api>,
    manager: SessionManager,
    gate_stages: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("fm-")
            .tempdir_in("/tmp")
            .unwrap();
        let (entered, events) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let gate = Arc::new(Mutex::new(gate));
        let gate_stages = Arc::new(AtomicBool::new(false));
        let stage_hook = {
            let enabled = gate_stages.clone();
            let events = entered.clone();
            let gate = gate.clone();
            Arc::new(move |phase: &str| {
                if enabled.load(Ordering::Relaxed) {
                    let phase = match phase {
                        "flattening" => "flattening",
                        "applying" => "applying",
                        "publishing" => "publishing",
                        _ => return,
                    };
                    events.send(phase).unwrap();
                    gate.lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
            })
        };
        let api = Arc::new(Api {
            response: Mutex::new(Ok(fixture("1"))),
            metadata: Mutex::new("t1".into()),
            calls: AtomicUsize::new(0),
            geometry: Mutex::new(vec![]),
            entered,
            release: gate,
            block_meta: AtomicBool::new(false),
            metadata_error: Mutex::new(None),
            block_images: AtomicBool::new(false),
        });
        let session = sessions::open_session_with_hooks(
            dir.path().join("ABC1234567890/db"),
            "ABC1234567890".into(),
            Some("ABC1234567890"),
            Some(api.clone()),
            sessions::WorkerHooks {
                on_phase: Some(stage_hook),
            },
        )
        .unwrap();
        let manager = SessionManager {
            sessions: vec![session],
            root: dir.path().to_path_buf().into(),
            token: None,
            default_key: Some("ABC1234567890".into()),
            api: Some(api.clone()),
        };
        Self {
            release,
            entered: events,
            api,
            manager,
            gate_stages,
            _dir: dir,
        }
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        for session in &mut self.manager.sessions {
            session.maintain(false);
        }
        let mut offload = Offload::new(None);
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
        call_and_wait(
            &mut self.manager,
            &mut offload,
            "disabled",
            true,
            Duration::ZERO,
            &dispatch::tool_registry(),
            &message,
        )
    }

    fn value(&mut self, name: &str, args: Value) -> Value {
        let response = self.call(name, args);
        assert_eq!(response["result"]["isError"], false, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn wait_for(&mut self, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = self.value("figmog_status", json!({}));
            if status["pull"]["job"]["state"] == state {
                return status;
            }
            assert!(Instant::now() < deadline, "waiting for {state}: {status}");
            std::thread::yield_now();
        }
    }
}

#[test]
fn async_open_acknowledges_before_fetch_and_reports_first_publication() {
    let mut h = Harness::new();
    let accepted = h.value("figmog_open", json!({"file": "ABC1234567890"}));
    assert_eq!(
        accepted,
        json!({"accepted": true, "key": "ABC1234567890", "job_id": 1, "state": "running"})
    );
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    assert_eq!(h.value("figmog_status", json!({}))["ready"], false);
    assert_eq!(
        h.value("figmog_files", json!({}))[0]["pull"]["job"]["id"],
        1
    );
    let read = h.call("figmog_node", json!({"id": "0:0"}));
    assert_eq!(read["result"]["isError"], true);
    assert!(
        read["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not ready")
    );
    h.release.send(()).unwrap();
    let status = h.wait_for("succeeded");
    assert_eq!(status["ready"], true);
    assert_eq!(status["version"], "1");
    assert_eq!(status["pull"]["last_success"]["churn"]["added"], 1);
    assert_eq!(h.value("figmog_node", json!({"id": "0:0"}))["name"], "1");
    // The async response has a machine-readable schema and content too.
    let tools = mcp::handle_message(
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string(),
        &dispatch::tool_registry(),
        &mut FnHandler(|_: &str, _: &Value| unreachable!()),
    )
    .unwrap();
    let open = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "figmog_open")
        .unwrap();
    assert_eq!(
        open["outputSchema"]["properties"]["accepted"]["const"],
        true
    );
}

#[test]
fn async_sync_keeps_committed_reads_available_through_every_pull_phase() {
    let mut h = Harness::new();
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    // Another real mirror, independently readable while the first applies.
    let other_path = h._dir.path().join("OTHER123456789/db");
    {
        let mut store = crate::open_store!(&other_path);
        store::sync(
            &mut store,
            &Default::default(),
            &crate::flatten::flatten_file(&fixture("other")).unwrap(),
            1,
        );
    }
    h.manager.open("OTHER123456789").unwrap();
    h.gate_stages.store(true, Ordering::Relaxed);
    let accepted = h.value("figmog_sync", json!({}));
    assert_eq!(accepted["job_id"], 2);
    for phase in ["fetch", "flattening", "applying", "publishing"] {
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            phase
        );
        let status = h.value("figmog_status", json!({}));
        assert_eq!(status["version"], "1", "{phase}: {status}");
        assert_eq!(status["pull"]["job"]["state"], "running");
        assert_eq!(status["pull"]["last_success"]["version"], "1");
        assert_eq!(h.value("figmog_node", json!({"id":"0:0"}))["name"], "1");
        assert_eq!(
            h.value("figmog_node", json!({"file":"OTHER123456789", "id":"0:0"}))["name"],
            "other"
        );
        let files = h.value("figmog_files", json!({}));
        assert_eq!(files[0]["version"], "1");
        assert_eq!(files[1]["version"], "other");
        assert_eq!(
            h.value("figmog_sync", json!({}))["job_id"],
            2,
            "duplicates coalesce at {phase}"
        );
        h.release.send(()).unwrap();
    }
    let status = h.wait_for("succeeded");
    assert_eq!(status["version"], "2");
    assert_eq!(status["pull"]["last_success"]["churn"]["changed"], 1);
    assert_eq!(h.api.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        h.value("figmog_status", json!({})),
        status,
        "completion is published once"
    );
}

#[test]
fn async_failed_pull_retains_prior_mirror_and_can_be_retried() {
    let mut h = Harness::new();
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    *h.api.response.lock().unwrap() = Err(ApiError::RateLimited {
        retry_after: Duration::ZERO,
    });
    assert_eq!(h.value("figmog_sync", json!({}))["job_id"], 2);
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    let failed = h.wait_for("failed");
    assert_eq!(failed["ready"], true);
    assert_eq!(failed["version"], "1");
    assert_eq!(failed["pull"]["last_success"]["job_id"], 1);
    assert!(
        failed["pull"]["job"]["error"]
            .as_str()
            .unwrap()
            .contains("retry after")
    );
    assert_eq!(h.value("figmog_node", json!({"id":"0:0"}))["name"], "1");
    assert_eq!(
        h.value("figmog_status", json!({})),
        failed,
        "inspection must retain the failure"
    );
    assert_eq!(h.value("figmog_sync", json!({}))["job_id"], 3);
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    let recovered = h.wait_for("succeeded");
    assert_eq!(recovered["version"], "2");
    assert_eq!(recovered["pull"]["last_success"]["job_id"], 3);
}

#[test]
fn async_malformed_document_pull_preserves_the_readable_mirror() {
    for document in [
        json!(null),
        json!({"id": "0:0", "type": "DOCUMENT", "children": [{
            "id": "0:1", "type": "CANVAS", "children": null
        }]}),
        json!({"id": "0:0", "type": "DOCUMENT", "children": [{
            "id": "0:1", "type": "CANVAS", "children": [{"type": "TEXT"}]
        }]}),
    ] {
        let mut h = Harness::new();
        let mut seed = fixture("1");
        seed["document"]["children"] = json!([{
            "id": "0:1", "type": "CANVAS", "name": "Page", "children": [{
                "id": "1:1", "type": "TEXT", "characters": "Synthetic text"
            }]
        }]);
        *h.api.response.lock().unwrap() = Ok(seed);
        h.value("figmog_sync", json!({}));
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
        h.release.send(()).unwrap();
        let initial = h.wait_for("succeeded");
        let tree = h.value("figmog_node", json!({"id": "0:0", "children": true}));

        let mut response = fixture("2");
        response["document"] = document;
        *h.api.response.lock().unwrap() = Ok(response);
        h.value("figmog_sync", json!({}));
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
        h.release.send(()).unwrap();
        let failed = h.wait_for("failed");
        assert_eq!(failed["ready"], true);
        assert_eq!(failed["version"], "1");
        assert_eq!(failed["nodes"], 3);
        assert_eq!(
            failed["pull"]["last_success"],
            initial["pull"]["last_success"]
        );
        assert!(
            failed["pull"]["job"]["error"]
                .as_str()
                .unwrap()
                .contains("document")
        );
        assert_eq!(
            h.value("figmog_node", json!({"id": "0:0", "children": true})),
            tree
        );
        // Reopen the worker's store to prove the rejected pull never reached
        // the persisted mirror, even if a previous published snapshot survived.
        h.manager.sessions.clear();
        let reopened =
            sessions::open_session(&h._dir.path().to_path_buf().into(), "ABC1234567890", None)
                .unwrap();
        let status = reopened.status().unwrap();
        assert_eq!(status["version"], "1");
        assert_eq!(status["nodes"], 3);
    }
}

#[test]
fn async_geometry_upgrade_queues_one_followup_and_survives_reopen() {
    let mut h = Harness::new();
    h.value("figmog_open", json!({"file":"ABC1234567890"}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..3 {
        let queued = h.value(
            "figmog_open",
            json!({"file":"https://www.figma.com/design/ABC1234567890/F", "geometry":true}),
        );
        assert_eq!(queued["job_id"], 2);
        assert_eq!(queued["state"], "queued");
    }
    assert_eq!(h.manager.sessions.len(), 1, "URL and key dedupe");
    h.release.send(()).unwrap();
    // Drive publication of job 1, which starts the queued geometry job.
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.api.calls.load(Ordering::Relaxed) < 2 {
        h.value("figmog_status", json!({}));
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    let status = h.value("figmog_status", json!({}));
    assert_eq!(status["version"], "1");
    assert_eq!(status["pull"]["last_completed"]["id"], 1);
    assert_eq!(status["pull"]["last_completed"]["state"], "succeeded");
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    assert_eq!(*h.api.geometry.lock().unwrap(), vec![false, true]);
    h.manager.sessions.clear();
    let session = sessions::open_session_with_api(
        h._dir.path().join("ABC1234567890/db"),
        "ABC1234567890".into(),
        Some("ABC1234567890"),
        Some(h.api.clone()),
    )
    .unwrap();
    h.manager.sessions.push(session);
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    assert_eq!(*h.api.geometry.lock().unwrap(), vec![false, true, true]);
}

#[test]
fn async_retry_after_keeps_explicit_retries_queued_without_hiding_errors() {
    let mut h = Harness::new();
    *h.api.response.lock().unwrap() = Err(ApiError::RateLimited {
        retry_after: Duration::from_secs(90),
    });
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    let failed = h.wait_for("failed");
    assert_eq!(failed["ready"], false);
    let retry = h.value("figmog_sync", json!({}));
    assert_eq!(retry["state"], "queued");
    assert_eq!(retry["job_id"], 2);
    assert_eq!(h.value("figmog_sync", json!({}))["job_id"], 2);
    assert!(matches!(
        h.entered.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    let status = h.value("figmog_status", json!({}));
    assert_eq!(status["pull"]["last_completed"], failed["pull"]["job"]);
}

#[test]
fn async_delayed_metadata_poll_keeps_reads_responsive_and_reconciles_once() {
    let mut h = Harness::new();
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    h.api.block_meta.store(true, Ordering::Relaxed);
    h.manager.sessions[0].poll_at(Duration::ZERO, Instant::now());
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    assert_eq!(h.value("figmog_node", json!({"id":"0:0"}))["name"], "1");
    assert_eq!(h.value("figmog_files", json!({}))[0]["version"], "1");
    assert_eq!(h.value("figmog_status", json!({}))["version"], "1");
    h.api.block_meta.store(false, Ordering::Relaxed);
    h.release.send(()).unwrap();
    // A first metadata observation after an explicit fetch needs one
    // reconciliation. Drive it through status, without blocking dispatch.
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.api.calls.load(Ordering::Relaxed) < 2 {
        h.value("figmog_status", json!({}));
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    // An edit during this fetch is outside its pre-fetch watermark.
    *h.api.metadata.lock().unwrap() = "t2".into();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    h.manager.sessions[0].poll_at(Duration::ZERO, Instant::now());
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.api.calls.load(Ordering::Relaxed) < 3 {
        h.value("figmog_status", json!({}));
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    // No further edit: this poll must not create job 4.
    h.manager.sessions[0].poll_at(Duration::ZERO, Instant::now());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = h.value("figmog_status", json!({}));
        if status["pull"]["polling"] == false {
            assert_eq!(status["pull"]["job"]["id"], 3);
            assert_eq!(h.api.calls.load(Ordering::Relaxed), 3);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn async_graceful_shutdown_drains_running_pull_and_discards_followup() {
    let mut h = Harness::new();
    h.value("figmog_open", json!({"file":"ABC1234567890"}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        h.value(
            "figmog_open",
            json!({"file":"ABC1234567890", "geometry":true})
        )["state"],
        "queued"
    );
    let Harness {
        release,
        entered: _,
        api,
        manager,
        _dir,
        gate_stages: _,
    } = h;
    let (done, finished) = mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        drop(manager);
        done.send(()).unwrap();
    });
    assert!(matches!(
        finished.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    finished.recv_timeout(Duration::from_secs(5)).unwrap();
    shutdown.join().unwrap();
    assert_eq!(
        api.calls.load(Ordering::Relaxed),
        1,
        "queued upgrade must not start during shutdown"
    );
    // The running transaction finished and releases its store lock.
    let session = sessions::open_session_at(
        _dir.path().join("ABC1234567890/db"),
        "ABC1234567890".into(),
        Some("ABC1234567890"),
        None,
    )
    .unwrap();
    assert_eq!(session.status().unwrap()["version"], "1");
}

#[test]
fn async_socket_cli_acknowledges_open_and_sync_before_worker_release() {
    let Harness {
        release,
        entered,
        api,
        manager,
        _dir,
        gate_stages: _,
    } = Harness::new();
    let (listener, path) = bind_socket(&_dir.path().join(".figmog")).unwrap();
    let _socket_guard = SocketGuard::new(path).unwrap();
    let (tx, rx) = mpsc::channel();
    let registry: ConnRegistry = Arc::default();
    spawn_socket_acceptor(listener, tx.clone(), registry.clone());
    let server = std::thread::spawn(move || {
        serve_loop(
            manager,
            None,
            true,
            false,
            Duration::from_secs(10),
            rx,
            registry,
        )
    });
    let cli = |tool: &str, args: &str| {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("figmog"))
            .args(["call", tool, "--args", args])
            .current_dir(_dir.path())
            .env("FIGMA_TOKEN", "")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let open = cli("figmog_open", r#"{"file":"ABC1234567890"}"#);
    assert_eq!(open["accepted"], true);
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    let pending = std::process::Command::new(assert_cmd::cargo::cargo_bin("figmog"))
        .args(["status"])
        .current_dir(_dir.path())
        .env("FIGMA_TOKEN", "")
        .output()
        .unwrap();
    assert!(!pending.status.success());
    assert!(
        String::from_utf8_lossy(&pending.stderr).contains("not ready"),
        "{}",
        String::from_utf8_lossy(&pending.stderr)
    );
    // The CLI has exited. Its disconnect cannot cancel the accepted job.
    assert_eq!(
        cli("figmog_status", r#"{"file":"ABC1234567890"}"#)["ready"],
        false
    );
    release.send(()).unwrap();
    let wait = || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = cli("figmog_status", r#"{"file":"ABC1234567890"}"#);
            if status["pull"]["job"]["state"] == "succeeded" {
                return status;
            }
            assert!(Instant::now() < deadline, "{status}");
        }
    };
    assert_eq!(wait()["version"], "1");
    let sync = cli("figmog_sync", r#"{"file":"ABC1234567890"}"#);
    assert_eq!(sync["accepted"], true);
    assert_eq!(sync["job_id"], 2);
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    assert_eq!(
        cli("figmog_node", r#"{"file":"ABC1234567890","id":"0:0"}"#)["name"],
        "1"
    );
    release.send(()).unwrap();
    assert_eq!(wait()["version"], "2");
    assert_eq!(api.calls.load(Ordering::Relaxed), 2);
    tx.send(Incoming::StdinClosed).unwrap();
    server.join().unwrap().unwrap();
}

#[test]
fn async_stdio_open_acknowledges_and_accepts_requests_while_fetch_is_gated() {
    let Harness {
        release,
        entered,
        api: _,
        manager,
        _dir: _dir_guard,
        gate_stages: _,
    } = Harness::new();
    let (input, rx) = mpsc::channel();
    let (output, responses) = mpsc::channel();
    let server = std::thread::spawn(move || {
        serve_loop_with_output(
            manager,
            None,
            true,
            false,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(10),
            rx,
            Arc::default(),
            move |frame: &str| {
                output
                    .send(serde_json::from_str::<Value>(frame).unwrap())
                    .is_ok()
            },
        )
    });
    let call = |id, name, arguments| {
        input
            .send(Incoming::Stdio(
                json!({"jsonrpc":"2.0", "id":id, "method":"tools/call",
            "params":{"name":name,"arguments":arguments}})
                .to_string(),
            ))
            .unwrap();
        let response = responses.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(response["result"]["isError"], false, "{response}");
        serde_json::from_str::<Value>(response["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap()
    };
    assert_eq!(
        call(1, "figmog_open", json!({"file":"ABC1234567890"}))["accepted"],
        true
    );
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    assert_eq!(call(2, "figmog_files", json!({}))[0]["ready"], false);
    assert_eq!(
        call(3, "figmog_sync", json!({}))["job_id"],
        1,
        "sync coalesces with open"
    );
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = call(4, "figmog_status", json!({}));
        if status["ready"] == true {
            assert_eq!(status["version"], "1");
            break;
        }
        assert!(Instant::now() < deadline, "{status}");
    }
    input.send(Incoming::StdinClosed).unwrap();
    server.join().unwrap().unwrap();
}

#[test]
fn async_images_do_not_wait_behind_a_gated_pull() {
    let mut h = Harness::new();
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    // Even a cache hit must never queue behind the running pull. Busy
    // images return an actionable error before the gate is released.
    let images = h.call("figmog_images", json!({"ids":["0:0"]}));
    assert_eq!(images["result"]["isError"], true);
    assert!(
        images["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("in progress")
    );
    assert_eq!(h.value("figmog_status", json!({}))["version"], "1");
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
}

#[test]
fn async_implicit_first_read_opens_and_coalesces_a_gated_pull() {
    let mut h = Harness::new();
    h.manager.sessions.clear();
    h.manager.default_key = None;
    let args = json!({"file":"ABC1234567890", "id":"0:0"});
    let response = h.call("figmog_node", args.clone());
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not ready")
    );
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..3 {
        assert_eq!(
            h.call("figmog_node", args.clone())["result"]["isError"],
            true
        );
        let files = h.value("figmog_files", json!({}));
        assert_eq!(files.as_array().unwrap().len(), 1);
        assert_eq!(files[0]["pull"]["job"]["id"], 1);
    }
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    assert_eq!(h.value("figmog_node", args)["name"], "1");
    assert_eq!(h.api.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn async_startup_queues_missing_mirror_without_waiting_for_its_pull() {
    let mut h = Harness::new();
    h.manager.sessions.clear();
    h.manager.default_key = None;
    add_startup_files(
        &mut h.manager,
        &["ABC1234567890".into()],
        false,
        Duration::ZERO,
    )
    .unwrap();
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let files = h.value("figmog_files", json!({}));
    assert_eq!(files[0]["default"], true);
    assert_eq!(files[0]["ready"], false);
    assert_eq!(files[0]["pull"]["job"]["state"], "running");
    assert_eq!(h.value("figmog_sync", json!({}))["job_id"], 1);
    h.release.send(()).unwrap();
    assert_eq!(h.wait_for("succeeded")["version"], "1");
    assert_eq!(h.api.calls.load(Ordering::Relaxed), 1);
}

impl Harness {
    /// Prime a real watch observation and its pull, making later polls
    /// unchanged without assuming full-file and metadata timestamps agree.
    fn mirror_from_watch(&mut self) {
        self.manager.sessions[0].poll_at(Duration::ZERO, Instant::now());
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.api.calls.load(Ordering::Relaxed) == 0 {
            self.manager.sessions[0].maintain(true);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            self.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "fetch"
        );
        self.release.send(()).unwrap();
        self.wait_for("succeeded");
    }

    fn finish_poll_at(&mut self, now: Instant) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.manager.sessions[0].maintain_at(now, true);
            if self.manager.sessions[0].status().unwrap()["pull"]["polling"] == false {
                return;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    fn finish_pull_at(&mut self, now: Instant) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.manager.sessions[0].maintain_at(now, true);
            if self.manager.sessions[0].status().unwrap()["pull"]["job"]["state"] == "succeeded" {
                return;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
}

#[test]
fn watch_cadence_does_not_skip_rounds_after_delayed_unchanged_metadata() {
    for seconds in [2, 10] {
        let mut h = Harness::new();
        h.mirror_from_watch();
        h.api.block_meta.store(true, Ordering::Relaxed);
        let interval = Duration::from_secs(seconds);
        let mut due = Instant::now() + Duration::from_secs(60);
        let mut cursor = 0;
        for round in 0..3 {
            let (next, _) = watch_tick_at(&mut h.manager, &mut cursor, interval, due);
            assert_eq!(
                h.manager.sessions[0].status().unwrap()["pull"]["polling"],
                true,
                "round {round} at configured {seconds}s cadence was skipped"
            );
            assert_eq!(
                h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
                "metadata"
            );
            h.release.send(()).unwrap();
            h.finish_poll_at(due + Duration::from_millis(100));
            due = next;
        }
        assert_eq!(
            h.api.calls.load(Ordering::Relaxed),
            1,
            "unchanged polls must not pull"
        );
    }
}

impl Harness {
    fn desktop_write(&mut self, no_watch: bool) {
        let mut desktop = crate::upstream::FakeUpstream::new(vec![json!({
            "name":"set_node_name", "inputSchema":{"type":"object"}
        })]);
        desktop.push_result(Ok(json!({"content":[], "isError":false})));
        let (tools, _) = proxy::merge_registry(dispatch::tool_registry(), desktop.tools());
        let mut offload = Offload::new(Some(DesktopWorker::spawn(desktop)));
        let response = call_and_wait(
            &mut self.manager,
            &mut offload,
            "connected",
            no_watch,
            Duration::from_secs(3600),
            &tools,
            &json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"set_node_name", "arguments":{}}}),
        );
        assert_eq!(response["result"]["isError"], false, "{response}");
    }

    fn wait_for_metadata_at(&mut self, now: Instant) {
        self.wait_for_event_at(now, "metadata");
    }

    fn wait_for_event_at(&mut self, now: Instant, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.manager.sessions[0].maintain_at(now, true);
            if let Ok(event) = self.entered.try_recv() {
                assert_eq!(event, expected);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected} request never started"
            );
            std::thread::yield_now();
        }
    }
}

#[test]
fn desktop_write_expedites_recent_poll_and_retains_writes_during_poll() {
    let mut h = Harness::new();
    h.mirror_from_watch();
    h.api.block_meta.store(true, Ordering::Relaxed);
    let hour = Duration::from_secs(3600);
    let base = Instant::now() + Duration::from_secs(60);
    h.manager.sessions[0].poll_at(hour, base);
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_millis(100));

    h.desktop_write(false);
    watch_tick_at(
        &mut h.manager,
        &mut 0,
        hour,
        base + Duration::from_millis(200),
    );
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        true,
        "a desktop write must bypass the ordinary one-hour deadline"
    );
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    // Writes after an observation starts cannot be certified by that
    // observation. They must coalesce into one additional poll.
    h.desktop_write(false);
    h.desktop_write(false);
    h.release.send(()).unwrap();
    h.wait_for_metadata_at(base + Duration::from_millis(300));
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_millis(400));
    assert!(matches!(
        h.entered.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(h.api.calls.load(Ordering::Relaxed), 1);

    h.desktop_write(true);
    h.manager.sessions[0].maintain_at(base + Duration::from_secs(1), true);
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false,
        "--no-watch must not schedule desktop-write polling"
    );
}

#[test]
fn desktop_write_during_pull_is_polled_immediately_after_publication() {
    let mut h = Harness::new();
    h.mirror_from_watch();
    h.api.block_meta.store(true, Ordering::Relaxed);
    let base = Instant::now() + Duration::from_secs(60);
    h.manager.sessions[0].poll_at(Duration::from_secs(3600), base);
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_millis(100));
    h.manager.sessions[0].request_pull(false, Duration::from_secs(3600));
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    h.desktop_write(false);
    h.manager.sessions[0].maintain_at(base + Duration::from_millis(200), true);
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false
    );
    h.release.send(()).unwrap();
    h.wait_for_metadata_at(base + Duration::from_millis(300));
    assert_eq!(h.manager.sessions[0].status().unwrap()["version"], "2");
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_millis(400));
}

#[test]
fn expedited_poll_retries_honor_metadata_retry_after_and_network_backoff() {
    for (error, seconds) in [
        (
            ApiError::RateLimited {
                retry_after: Duration::from_secs(90),
            },
            90,
        ),
        (ApiError::Network("synthetic offline peer".into()), 5),
    ] {
        let mut h = Harness::new();
        h.mirror_from_watch();
        h.api.block_meta.store(true, Ordering::Relaxed);
        *h.api.metadata_error.lock().unwrap() = Some(error);
        let base = Instant::now() + Duration::from_secs(60);
        h.desktop_write(false);
        h.manager.sessions[0].maintain_at(base, true);
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "metadata"
        );
        h.release.send(()).unwrap();
        h.finish_poll_at(base);
        // Rate limits also survive another mutation; a network failure
        // retries the original expedited request without another write.
        if seconds == 90 {
            h.desktop_write(false);
        }
        h.manager.sessions[0].maintain_at(base + Duration::from_secs(seconds - 1), true);
        assert_eq!(
            h.manager.sessions[0].status().unwrap()["pull"]["polling"],
            false
        );
        assert!(matches!(
            h.entered.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        h.wait_for_metadata_at(base + Duration::from_secs(seconds));
        h.release.send(()).unwrap();
        h.finish_poll_at(base + Duration::from_secs(seconds));
    }
}

#[test]
fn expedited_poll_preserves_pull_retry_after() {
    let mut h = Harness::new();
    h.mirror_from_watch();
    *h.api.response.lock().unwrap() = Err(ApiError::RateLimited {
        retry_after: Duration::from_secs(90),
    });
    h.api.block_meta.store(true, Ordering::Relaxed);
    let base = Instant::now() + Duration::from_secs(60);
    h.manager.sessions[0].request_pull(false, Duration::from_secs(3600));
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    h.release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.manager.sessions[0].status().unwrap()["pull"]["job"]["state"] != "failed" {
        h.manager.sessions[0].maintain_at(base, true);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    h.desktop_write(false);
    // Pull rate limits retain the existing max(interval, Retry-After)
    // cooldown, even when an expedited metadata request is pending.
    h.manager.sessions[0].maintain_at(base + Duration::from_secs(3599), true);
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false
    );
    h.wait_for_metadata_at(base + Duration::from_secs(3600));
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_secs(3600));
}

#[test]
fn images_retry_a_failed_first_mirror_coalescing_and_honoring_backoff() {
    for retry_after in [Duration::ZERO, Duration::from_secs(90)] {
        let mut h = Harness::new();
        *h.api.response.lock().unwrap() = Err(ApiError::RateLimited { retry_after });
        let args = json!({"file":"ABC1234567890", "ids":["0:0"]});
        let first = h.call("figmog_images", args.clone());
        assert_eq!(first["result"]["isError"], true);
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "fetch"
        );
        h.release.send(()).unwrap();
        let failed = h.wait_for("failed");
        assert_eq!(failed["pull"]["job"]["id"], 1);
        assert_eq!(
            h.value("figmog_files", json!({}))[0]["pull"]["job"]["state"],
            "failed",
            "inspection must not implicitly retry"
        );

        assert_eq!(
            h.call("figmog_images", args.clone())["result"]["isError"],
            true
        );
        let retry = h.value("figmog_status", json!({}));
        assert_eq!(
            retry["pull"]["job"]["id"], 2,
            "another image request must retry the failed mirror"
        );
        let expected = if retry_after.is_zero() {
            "running"
        } else {
            "queued"
        };
        assert_eq!(retry["pull"]["job"]["state"], expected);
        assert_eq!(h.call("figmog_images", args)["result"]["isError"], true);
        assert_eq!(
            h.value("figmog_status", json!({}))["pull"]["job"]["id"],
            2,
            "duplicate retries coalesce"
        );
        if retry_after.is_zero() {
            assert_eq!(
                h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
                "fetch"
            );
            h.release.send(()).unwrap();
            assert_eq!(h.wait_for("succeeded")["ready"], true);
        } else {
            assert!(matches!(
                h.entered.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
    }
}

#[test]
fn watch_cadence_does_not_skip_round_after_changed_poll_publishes() {
    let mut h = Harness::new();
    h.mirror_from_watch();
    h.api.block_meta.store(true, Ordering::Relaxed);
    let interval = Duration::from_secs(2);
    let mut due = Instant::now() + Duration::from_secs(60);
    let mut cursor = 0;
    for round in 0..3 {
        *h.api.metadata.lock().unwrap() = format!("edit-{round}");
        let (next, _) = watch_tick_at(&mut h.manager, &mut cursor, interval, due);
        assert_eq!(
            h.manager.sessions[0].status().unwrap()["pull"]["polling"],
            true,
            "round {round} after publishing a watched change was skipped"
        );
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "metadata"
        );
        h.release.send(()).unwrap();
        h.wait_for_event_at(due + Duration::from_millis(100), "fetch");
        h.release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.manager.sessions[0].status().unwrap()["pull"]["job"]["state"] != "succeeded" {
            h.manager.sessions[0].maintain_at(due + Duration::from_millis(200), true);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        due = next;
    }
    assert_eq!(h.api.calls.load(Ordering::Relaxed), 4);
}

mod desktop;
mod isolation;
mod offload;
mod watch_and_cache;

#[test]
fn successful_explicit_sync_preserves_metadata_retry_and_normal_round() {
    let mut h = Harness::new();
    h.mirror_from_watch();
    h.api.block_meta.store(true, Ordering::Relaxed);
    *h.api.metadata_error.lock().unwrap() =
        Some(ApiError::Network("temporary metadata failure".into()));
    let base = Instant::now() + Duration::from_secs(60);
    let interval = Duration::from_secs(3600);
    let mut watch = WatchScheduler::new(base);
    watch.service_at(&mut h.manager, interval, base);
    h.wait_for_metadata_at(base);
    h.release.send(()).unwrap();
    h.finish_poll_at(base);
    assert_eq!(
        watch.next_deadline(&h.manager),
        base + Duration::from_secs(5)
    );

    h.manager.sessions[0].request_pull(false, interval);
    h.wait_for_event_at(base + Duration::from_secs(1), "fetch");
    h.release.send(()).unwrap();
    h.finish_pull_at(base + Duration::from_secs(1));
    assert_eq!(
        watch.next_deadline(&h.manager),
        base + Duration::from_secs(5),
        "a successful sync must not cancel outstanding metadata recovery"
    );

    // An edit after the explicit fetch must still be detected at recovery.
    *h.api.metadata.lock().unwrap() = "after-sync".into();
    watch.service_at(&mut h.manager, interval, base + Duration::from_secs(4));
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false
    );
    *h.api.metadata_error.lock().unwrap() =
        Some(ApiError::Network("second metadata failure".into()));
    watch.service_at(&mut h.manager, interval, base + Duration::from_secs(5));
    h.wait_for_metadata_at(base + Duration::from_secs(5));
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_secs(5));
    assert_eq!(
        watch.next_deadline(&h.manager),
        base + Duration::from_secs(15),
        "pull success must not reset consecutive metadata failures' backoff"
    );
    watch.service_at(&mut h.manager, interval, base + Duration::from_secs(15));
    h.wait_for_metadata_at(base + Duration::from_secs(15));
    h.release.send(()).unwrap();
    h.wait_for_event_at(base + Duration::from_secs(15), "fetch");
    h.release.send(()).unwrap();
    h.finish_pull_at(base + Duration::from_secs(15));
    assert_eq!(
        watch.next_deadline(&h.manager),
        base + interval,
        "recovery must clear its retry without postponing the normal round"
    );
    watch.service_at(&mut h.manager, interval, base + interval);
    h.wait_for_metadata_at(base + interval);
    h.release.send(()).unwrap();
    h.finish_poll_at(base + interval);
    assert_eq!(h.api.calls.load(Ordering::Relaxed), 3);
}

#[test]
fn successful_explicit_sync_preserves_metadata_retry_after_and_expedited_reconciliation() {
    for (seconds, sync_finished, expedited) in [(90, 1, true), (90, 91, false), (7200, 1, true)] {
        let mut h = Harness::new();
        h.mirror_from_watch();
        h.api.block_meta.store(true, Ordering::Relaxed);
        *h.api.metadata_error.lock().unwrap() = Some(ApiError::RateLimited {
            retry_after: Duration::from_secs(seconds),
        });
        let base = Instant::now() + Duration::from_secs(60);
        let interval = Duration::from_secs(3600);
        let mut watch = WatchScheduler::new(base);
        watch.service_at(&mut h.manager, interval, base);
        h.wait_for_metadata_at(base);
        h.release.send(()).unwrap();
        h.finish_poll_at(base);

        h.manager.sessions[0].request_pull(false, interval);
        h.wait_for_event_at(base + Duration::from_secs(1), "fetch");
        if expedited {
            h.desktop_write(false);
        }
        h.manager.sessions[0].maintain_at(base + Duration::from_secs(sync_finished), true);
        assert_eq!(
            h.manager.sessions[0].status().unwrap()["pull"]["polling"],
            false,
            "metadata recovery must remain serialized behind the sync"
        );
        h.release.send(()).unwrap();
        h.finish_pull_at(base + Duration::from_secs(sync_finished));

        if seconds > 3600 {
            watch.service_at(&mut h.manager, interval, base + interval);
            assert_eq!(
                h.manager.sessions[0].status().unwrap()["pull"]["polling"],
                false,
                "a normal round must not bypass metadata Retry-After"
            );
        }
        assert_eq!(
            watch.next_deadline(&h.manager),
            base + Duration::from_secs(seconds.max(sync_finished)),
            "sync success must preserve even an overdue metadata retry"
        );
        if sync_finished < seconds {
            let before = base + Duration::from_secs(seconds - 1);
            h.manager.sessions[0].maintain_at(before, true);
            watch.service_at(&mut h.manager, interval, before);
            assert_eq!(
                h.manager.sessions[0].status().unwrap()["pull"]["polling"],
                false,
                "expedited reconciliation must honor metadata Retry-After"
            );
        }

        *h.api.metadata.lock().unwrap() = "after-sync".into();
        *h.api.response.lock().unwrap() = Ok(fixture("3"));
        let recovery = base + Duration::from_secs(seconds.max(sync_finished));
        if !expedited {
            watch.service_at(&mut h.manager, interval, recovery);
        }
        h.wait_for_metadata_at(recovery);
        h.release.send(()).unwrap();
        h.wait_for_event_at(recovery, "fetch");
        h.release.send(()).unwrap();
        h.finish_pull_at(recovery);
        assert_eq!(h.manager.sessions[0].status().unwrap()["version"], "3");
        h.manager.sessions[0].maintain_at(recovery + Duration::from_secs(1), true);
        assert!(matches!(
            h.entered.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(h.api.calls.load(Ordering::Relaxed), 3);
    }
}

#[test]
fn ordinary_metadata_failure_retries_at_backoff_before_next_hourly_round() {
    let mut h = Harness::new();
    h.mirror_from_watch();
    h.api.block_meta.store(true, Ordering::Relaxed);
    *h.api.metadata.lock().unwrap() = "t2".into();
    *h.api.metadata_error.lock().unwrap() =
        Some(ApiError::Network("temporary metadata failure".into()));
    let base = Instant::now() + Duration::from_secs(60);
    let mut watch = WatchScheduler::new(base);
    watch.service_at(&mut h.manager, Duration::from_secs(3600), base);
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.finish_poll_at(base);
    assert_eq!(
        watch.next_deadline(&h.manager),
        base + Duration::from_secs(5),
        "the server must wake for the failed file's backoff, not its hourly round"
    );
    watch.service_at(
        &mut h.manager,
        Duration::from_secs(3600),
        base + Duration::from_secs(4),
    );
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false
    );
    *h.api.metadata_error.lock().unwrap() =
        Some(ApiError::Network("second metadata failure".into()));
    watch.service_at(
        &mut h.manager,
        Duration::from_secs(3600),
        base + Duration::from_secs(5),
    );
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        true
    );
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_secs(5));
    assert_eq!(
        watch.next_deadline(&h.manager),
        base + Duration::from_secs(15),
        "a second failure must retain the ten-second exponential retry"
    );
    watch.service_at(
        &mut h.manager,
        Duration::from_secs(3600),
        base + Duration::from_secs(14),
    );
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false
    );
    watch.service_at(
        &mut h.manager,
        Duration::from_secs(3600),
        base + Duration::from_secs(15),
    );
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.wait_for_event_at(base + Duration::from_secs(15), "fetch");
    h.release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.manager.sessions[0].status().unwrap()["version"] != "2" {
        h.manager.sessions[0].maintain_at(base + Duration::from_secs(15), true);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    watch.service_at(
        &mut h.manager,
        Duration::from_secs(3600),
        base + Duration::from_secs(3600),
    );
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        true,
        "retry recovery must not postpone the next normal round by another hour"
    );
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.finish_poll_at(base + Duration::from_secs(3600));
}

#[test]
fn ordinary_watch_pull_failure_retries_at_backoff_before_next_hourly_round() {
    let mut h = Harness::new();
    h.mirror_from_watch();
    h.api.block_meta.store(true, Ordering::Relaxed);
    *h.api.metadata.lock().unwrap() = "t2".into();
    *h.api.response.lock().unwrap() = Err(ApiError::Network("temporary pull failure".into()));
    let base = Instant::now() + Duration::from_secs(60);
    let mut watch = WatchScheduler::new(base);
    watch.service_at(&mut h.manager, Duration::from_secs(3600), base);
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.wait_for_event_at(base, "fetch");
    h.release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.manager.sessions[0].status().unwrap()["pull"]["job"]["state"] != "failed" {
        h.manager.sessions[0].maintain_at(base, true);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(h.manager.sessions[0].status().unwrap()["version"], "1");
    assert_eq!(
        watch.next_deadline(&h.manager),
        base + Duration::from_secs(5),
        "a failed watched pull must not wait for the next hourly round"
    );
    watch.service_at(
        &mut h.manager,
        Duration::from_secs(3600),
        base + Duration::from_secs(4),
    );
    assert_eq!(
        h.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false
    );
    watch.service_at(
        &mut h.manager,
        Duration::from_secs(3600),
        base + Duration::from_secs(5),
    );
    assert_eq!(
        h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    h.release.send(()).unwrap();
    h.wait_for_event_at(base + Duration::from_secs(5), "fetch");
    h.release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.manager.sessions[0].status().unwrap()["version"] != "2" {
        h.manager.sessions[0].maintain_at(base + Duration::from_secs(5), true);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn ordinary_retries_share_rate_limit_and_do_not_starve_normal_rounds() {
    let mut first = Harness::new();
    first.mirror_from_watch();
    let mut second = Harness::new();
    second.mirror_from_watch();
    let mut other = second.manager.sessions.pop().unwrap();
    other.key = "OTHER123456789".into();
    first.manager.sessions.push(other);
    first.api.block_meta.store(true, Ordering::Relaxed);
    second.api.block_meta.store(true, Ordering::Relaxed);
    let base = Instant::now() + Duration::from_secs(60);
    let interval = Duration::from_secs(2);
    let mut watch = WatchScheduler::new(base);
    // Normal A, retry A, normal B, retry A: both queues progress even
    // when the configured normal cadence uses every available rate slot.
    for (seconds, index) in [(0, 0), (2, 0), (4, 1), (6, 0)] {
        if index == 0 {
            *first.api.metadata_error.lock().unwrap() = Some(ApiError::RateLimited {
                retry_after: Duration::ZERO,
            });
        }
        let now = base + Duration::from_secs(seconds);
        watch.service_at(&mut first.manager, interval, now);
        assert_eq!(
            first.manager.sessions[index].status().unwrap()["pull"]["polling"],
            true
        );
        let (entered, release) = if index == 0 {
            (&first.entered, &first.release)
        } else {
            (&second.entered, &second.release)
        };
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "metadata"
        );
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while first.manager.sessions[index].status().unwrap()["pull"]["polling"] == true {
            first.manager.sessions[index].maintain_at(now, true);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        watch.service_at(&mut first.manager, interval, now + Duration::from_secs(1));
        assert!(
            first
                .manager
                .sessions
                .iter()
                .all(|session| session.status().unwrap()["pull"]["polling"] == false),
            "ordinary retries must preserve the two-second global polling bound"
        );
        assert!(matches!(
            first.entered.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(
            second.entered.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }
    assert_eq!(first.api.calls.load(Ordering::Relaxed), 1);
    assert_eq!(second.api.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn ordinary_metadata_retries_preserve_retry_after_before_and_after_normal_round() {
    for seconds in [90, 7200] {
        let mut h = Harness::new();
        h.mirror_from_watch();
        h.api.block_meta.store(true, Ordering::Relaxed);
        *h.api.metadata_error.lock().unwrap() = Some(ApiError::RateLimited {
            retry_after: Duration::from_secs(seconds),
        });
        let base = Instant::now() + Duration::from_secs(60);
        let mut watch = WatchScheduler::new(base);
        let interval = Duration::from_secs(3600);
        watch.service_at(&mut h.manager, interval, base);
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "metadata"
        );
        h.release.send(()).unwrap();
        h.finish_poll_at(base);
        if seconds > 3600 {
            watch.service_at(&mut h.manager, interval, base + Duration::from_secs(3600));
            assert_eq!(
                h.manager.sessions[0].status().unwrap()["pull"]["polling"],
                false,
                "normal rounds cannot bypass a longer Retry-After"
            );
        }
        assert_eq!(
            watch.next_deadline(&h.manager),
            base + Duration::from_secs(seconds)
        );
        watch.service_at(
            &mut h.manager,
            interval,
            base + Duration::from_secs(seconds - 1),
        );
        assert_eq!(
            h.manager.sessions[0].status().unwrap()["pull"]["polling"],
            false
        );
        watch.service_at(
            &mut h.manager,
            interval,
            base + Duration::from_secs(seconds),
        );
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "metadata"
        );
        h.release.send(()).unwrap();
        h.finish_poll_at(base + Duration::from_secs(seconds));
    }
}
