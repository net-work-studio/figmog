//! NWS-2963: CLI writers as operations of a running serve — through the
//! real loop, the real socket acceptor and the CLI's own socket client,
//! against the fake Figma API.
use super::*;
use crate::serve::writes::{IMPORT_VARIABLES, PULL};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};

const KEY: &str = "ABC1234567890";

/// A serve loop listening on a real socket (no stdio client).
struct Live {
    tx: mpsc::Sender<Incoming>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Live {
    fn start(manager: SessionManager, offload: Offload) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("fw-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = crate::serve::socket_path(dir.path());
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, rx) = mpsc::channel();
        let registry: ConnRegistry = Arc::default();
        spawn_socket_acceptor(listener, tx.clone(), registry.clone());
        let thread = std::thread::spawn(move || {
            serve_loop_with_offload(
                manager,
                offload,
                true,
                false,
                Duration::from_secs(10),
                Instant::now() + Duration::from_secs(3600),
                rx,
                registry,
                |_: &str| true,
            )
        });
        Self {
            tx,
            thread: Some(thread),
            socket,
            _dir: dir,
        }
    }

    /// Run one write as the CLI does, on its own connection and thread.
    fn write(
        &self,
        tool: &'static str,
        args: Value,
    ) -> std::thread::JoinHandle<Result<Value, String>> {
        let stream = UnixStream::connect(&self.socket).unwrap();
        std::thread::spawn(move || crate::cli::write_over_stream(stream, tool, args))
    }

    /// Run one write exactly as the CLI does against an in-process serve:
    /// through its local-only route, reconnecting through it.
    fn write_local(
        &self,
        tool: &'static str,
        args: Value,
    ) -> std::thread::JoinHandle<Result<Value, String>> {
        let root = self.socket.parent().unwrap().to_path_buf();
        std::thread::spawn(move || crate::cli::write_over_local(&root, tool, args))
    }

    /// One plain `tools/call` on a fresh connection; the response frame.
    fn call(&self, name: &str, args: Value) -> std::thread::JoinHandle<Value> {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}});
        writeln!(stream, "{line}").unwrap();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut frame = String::new();
            reader.read_line(&mut frame).unwrap();
            serde_json::from_str(&frame).unwrap()
        })
    }

    fn stop(mut self) {
        self.tx.send(Incoming::StdinClosed).unwrap();
        self.thread.take().unwrap().join().unwrap().unwrap();
    }
}

/// Let in-flight requests reach the loop (it polls every 20ms).
fn settle() {
    std::thread::sleep(Duration::from_millis(150));
}

fn churn(value: &Value) -> String {
    serde_json::to_string_pretty(&value["result"]).unwrap()
}

/// Plain and `--geometry` network pulls and a variable import through the
/// loop: each answers only once committed and published, with the direct
/// path's output; identical plain pulls queued together share one fetch
/// (each keeps its operation id), while distinct writes — an import, a
/// geometry pull — run separately, in arrival order.
#[test]
fn routed_pulls_and_imports_match_direct_and_only_identical_plain_pulls_coalesce() {
    let h = Harness::new();
    let Harness {
        release,
        entered,
        api,
        manager,
        _dir: dir,
        ..
    } = h;
    let live = Live::start(manager, Offload::new(None));

    let first = live.write(PULL, json!({"file": KEY}));
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    // Queued while the first runs: two identical plain pulls, an import
    // and a geometry pull.
    let second = live.write(PULL, json!({"file": KEY}));
    settle();
    let third = live.write(PULL, json!({"file": KEY}));
    settle();
    let vars = std::str::from_utf8(include_bytes!(
        "../../../tests/fixtures/variables-export.json"
    ))
    .unwrap();
    let import = live.write(
        IMPORT_VARIABLES,
        json!({"file": KEY, "content": vars, "name": "vars.json"}),
    );
    settle();
    let geometry = live.write(PULL, json!({"file": KEY, "geometry": true}));
    settle();
    assert!(!first.is_finished(), "answered before it committed");
    release.send(()).unwrap();
    let first = first.join().unwrap().unwrap();

    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    release.send(()).unwrap();
    let (second, third) = (
        second.join().unwrap().unwrap(),
        third.join().unwrap().unwrap(),
    );
    let import = import.join().unwrap().unwrap();
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    release.send(()).unwrap();
    let geometry = geometry.join().unwrap().unwrap();

    assert_eq!(
        api.calls.load(Ordering::Relaxed),
        3,
        "only the plain pair shared"
    );
    assert_eq!(*api.geometry.lock().unwrap(), vec![false, false, true]);
    let ids: BTreeSet<_> = [&first, &second, &third, &import, &geometry]
        .iter()
        .map(|done| done["operation"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 5, "every write has its own operation id");
    assert_eq!(second["result"], third["result"]);
    assert_eq!(import["result"], json!({"imported": 3, "removed": 0}));

    // The direct path on an identical store prints the same churn.
    let direct = dir.path().join("direct/db");
    for (routed, response, geometry) in [
        (&first, fixture("1"), false),
        (&second, fixture("2"), false),
        (&geometry, fixture("2"), true),
    ] {
        let churn_direct = crate::cli::apply_pull_at(&direct, &response, false, geometry).unwrap();
        assert_eq!(
            churn(routed),
            serde_json::to_string_pretty(&churn_direct).unwrap()
        );
    }
    live.stop();
}

/// A plain pull joins only a plain pull queued directly before it (NWS-2963):
/// one queued behind an intervening write — here an import — must run after
/// that write, not share an earlier pull's result from before it.
#[test]
fn a_plain_pull_does_not_coalesce_across_an_intervening_write() {
    let h = Harness::new();
    let Harness {
        release,
        entered,
        api,
        manager,
        ..
    } = h;
    let live = Live::start(manager, Offload::new(None));

    let running = live.write(PULL, json!({"file": KEY}));
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    let early = live.write(PULL, json!({"file": KEY}));
    settle();
    let vars = std::str::from_utf8(include_bytes!(
        "../../../tests/fixtures/variables-export.json"
    ))
    .unwrap();
    let import = live.write(
        IMPORT_VARIABLES,
        json!({"file": KEY, "content": vars, "name": "vars.json"}),
    );
    settle();
    let late = live.write(PULL, json!({"file": KEY}));
    settle();
    release.send(()).unwrap();
    running.join().unwrap().unwrap();

    // The early pull's fetch; the late one is not riding on it.
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    release.send(()).unwrap();
    let early = early.join().unwrap().unwrap();
    // The import runs next, then the late pull fetches on its own.
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    settle();
    assert!(import.is_finished(), "the import ran after the late pull");
    assert!(!late.is_finished(), "the late pull shared an earlier fetch");
    release.send(()).unwrap();
    let late = late.join().unwrap().unwrap();
    import.join().unwrap().unwrap();
    assert_eq!(api.calls.load(Ordering::Relaxed), 3);
    assert_ne!(early["operation"], late["operation"]);
    // The late pull ran against the store the early pull left: no churn.
    assert_eq!(
        late["result"],
        json!({"added": 0, "changed": 0, "removed": 0, "unchanged": 1})
    );
    live.stop();
}

/// `pull --fresh` through serve: validation (the fetch) touches nothing;
/// then the per-store reset barrier waits for a worker read still holding
/// the old view, which answers from it; only then is the store cleared,
/// reopened and the pull applied — turning sticky geometry off.
#[test]
fn routed_fresh_pull_waits_for_the_reset_barrier_and_turns_geometry_off() {
    let h = Harness::new();
    let Harness {
        release,
        entered,
        api,
        manager,
        ..
    } = h;
    let (seen_tx, seen) = mpsc::channel();
    let (read_release, read_gate) = mpsc::channel::<()>();
    let read_gate = Arc::new(Mutex::new(read_gate));
    let mut offload = Offload::new(None);
    offload.reads.before_read = Some(Arc::new(move |name: &str| {
        if name == "figmog_stats" {
            let _ = seen_tx.send(());
            read_gate
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    }));
    let live = Live::start(manager, offload);

    let seed = live.write(PULL, json!({"file": KEY, "geometry": true}));
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(()).unwrap();
    assert_eq!(seed.join().unwrap().unwrap()["result"]["added"], 1);

    let stats = live.call("figmog_stats", json!({"file": KEY}));
    seen.recv_timeout(Duration::from_secs(5)).unwrap();
    let fresh = live.write(PULL, json!({"file": KEY, "fresh": true}));
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    release.send(()).unwrap();
    settle();
    assert!(!fresh.is_finished(), "reset ran under a live view");
    // Reads of this store are refused while it waits.
    let refused = live.call("figmog_node", json!({"file": KEY, "id": "0:0"}));
    let refused = refused.join().unwrap();
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("being reset"),
        "{refused}"
    );
    read_release.send(()).unwrap();
    let stats = stats.join().unwrap();
    assert!(stats.get("result").is_some(), "{stats}");
    let fresh = fresh.join().unwrap().unwrap();
    // A fresh store: everything is added again.
    assert_eq!(
        fresh["result"],
        json!({"added": 1, "changed": 0, "removed": 0, "unchanged": 0})
    );

    let plain = live.write(PULL, json!({"file": KEY}));
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(()).unwrap();
    plain.join().unwrap().unwrap();
    assert_eq!(*api.geometry.lock().unwrap(), vec![true, false, false]);

    let node = live.call("figmog_node", json!({"file": KEY, "id": "0:0"}));
    assert!(node.join().unwrap().get("result").is_some());
    live.stop();
}

/// Stdin EOF while routed writes are in flight (NWS-2963): the running
/// write — a fresh pull, whose reset and apply continue after its fetch —
/// still commits, publishes and answers its CLI during the shutdown drain,
/// while a write still queued is answered "not accepted" (nothing applied,
/// safe to retry) instead of being started or left "outcome unknown".
#[test]
fn shutdown_finishes_a_running_write_and_refuses_a_queued_one() {
    let h = Harness::new();
    let Harness {
        release,
        entered,
        api,
        manager,
        ..
    } = h;
    let mut live = Live::start(manager, Offload::new(None));
    let seed = live.write(PULL, json!({"file": KEY}));
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(()).unwrap();
    seed.join().unwrap().unwrap();

    let fresh = live.write(PULL, json!({"file": KEY, "fresh": true}));
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    let queued = live.write(PULL, json!({"file": KEY, "geometry": true}));
    settle();
    live.tx.send(Incoming::StdinClosed).unwrap();
    settle();
    assert!(!fresh.is_finished(), "answered before it committed");
    release.send(()).unwrap();
    // The loop exits once the drain answered everything (or its deadline
    // passed); a client left unanswered would then wait out its timeout.
    live.thread.take().unwrap().join().unwrap().unwrap();
    let answered = Instant::now() + Duration::from_secs(5);
    while !(fresh.is_finished() && queued.is_finished()) && Instant::now() < answered {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(fresh.is_finished(), "the running write was never answered");
    assert!(queued.is_finished(), "the queued write was never answered");
    let fresh = fresh.join().unwrap().unwrap();
    assert_eq!(
        fresh["result"],
        json!({"added": 1, "changed": 0, "removed": 0, "unchanged": 0})
    );
    let queued = queued.join().unwrap().unwrap_err();
    assert!(
        queued.contains("before accepting this write; nothing was applied"),
        "{queued}"
    );
    assert_eq!(api.calls.load(Ordering::Relaxed), 2, "the queued write ran");
}

/// A write queued in an in-process serve that then drains (stdin EOF,
/// NWS-2963) is refused "not accepted" while that serve's socket still
/// listens: the CLI reports it not applied at once instead of resending it
/// to the same exiting process — which never reads it, leaving the retry
/// unanswered and, once the process exits, "outcome unknown".
#[test]
fn a_draining_local_serve_refusal_is_reported_unapplied_not_retried() {
    let h = Harness::new();
    let Harness {
        release,
        entered,
        manager,
        ..
    } = h;
    let mut live = Live::start(manager, Offload::new(None));
    let running = live.write_local(PULL, json!({"file": KEY}));
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    let queued = live.write_local(PULL, json!({"file": KEY, "geometry": true}));
    settle();
    live.tx.send(Incoming::StdinClosed).unwrap();
    // The refusal arrives while the running pull still holds the drain
    // open (and the socket keeps listening).
    let answered = Instant::now() + Duration::from_secs(5);
    while !queued.is_finished() && Instant::now() < answered {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        queued.is_finished(),
        "the refused write was retried and hung"
    );
    let queued = queued.join().unwrap().unwrap_err();
    assert!(
        queued.contains("before accepting this write; nothing was applied")
            && !queued.contains("outcome unknown"),
        "{queued}"
    );
    release.send(()).unwrap();
    live.thread.take().unwrap().join().unwrap().unwrap();
    running.join().unwrap().unwrap();
}

/// The CLI's side when the owner goes away mid-write: it reports "outcome
/// unknown" with the operation id serve announced, and sends nothing again.
#[test]
fn a_vanished_owner_leaves_the_outcome_unknown_and_is_not_retried() {
    let (client, server) = UnixStream::pair().unwrap();
    let owner = std::thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut out = server;
        writeln!(out, "{}", crate::serve::writes::notification("feed-7")).unwrap();
        out.shutdown(std::net::Shutdown::Write).unwrap();
        // Nothing else arrives: no retry on this connection.
        let mut rest = String::new();
        reader.read_line(&mut rest).unwrap();
        (request, rest)
    });
    let error = crate::cli::write_over_stream(client, PULL, json!({"file": KEY})).unwrap_err();
    assert!(
        error.starts_with("outcome unknown") && error.contains("operation feed-7"),
        "{error}"
    );
    let (request, rest) = owner.join().unwrap();
    assert!(request.contains(PULL));
    assert!(rest.is_empty(), "the write was sent again: {rest}");
}
