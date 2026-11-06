//! The desktop worker: the one thread that owns the Figma desktop app's
//! upstream connection (NWS-2955), and the loop-side record of whether the
//! desktop is reachable and which tools it offers (NWS-2959).
//!
//! **Re-detection.** Serve probes the desktop once at startup (spec §8),
//! then keeps checking, always on this worker and never on the loop:
//!
//! - while it is missing, a full handshake every [`ProbeCadence::missing`]
//!   (30 s);
//! - while it is connected, a liveness `ping` and `tools/list` every
//!   [`ProbeCadence::live`] (60 s), falling back to a fresh handshake when
//!   the ping fails (a
//!   restarted desktop app does not know the old session);
//! - right away (at most once per [`MIN_PROBE_GAP`]) when a desktop call
//!   fails at the transport or protocol level, and when an agent calls a
//!   non-`figmog_*` tool while the desktop is missing.
//!
//! The desktop is marked missing only when a probe fails: a single failed
//! or timed-out call merely triggers that probe. Tool-level `isError`
//! results are successful forwarding and never trigger one. The loop
//! re-merges its registry and announces `notifications/tools/list_changed`
//! only when the upstream tool list it exposes actually changes, so a probe
//! that confirms the current state is silent.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{DeferredReply, ToolOutput};
use crate::proxy;
use crate::upstream::{UpstreamError, UpstreamMcp};

pub(super) const DESKTOP_SHUTDOWN: &str =
    "figmog serve is shutting down; the desktop call was not sent upstream";

/// Minimum spacing of probes triggered by calls (a failed desktop call, or
/// a desktop tool called while the desktop is missing), so a client
/// retrying in a tight loop cannot turn into a probe storm.
const MIN_PROBE_GAP: Duration = Duration::from_secs(1);
const MAX_QUEUED_CALLS: usize = 128;

/// How often the desktop is re-checked (module docs).
#[derive(Clone, Copy, Debug)]
pub(super) struct ProbeCadence {
    /// Between handshake attempts while the desktop is missing.
    pub(super) missing: Duration,
    /// Between liveness pings while it is connected.
    pub(super) live: Duration,
}

impl Default for ProbeCadence {
    fn default() -> Self {
        Self {
            missing: Duration::from_secs(30),
            live: Duration::from_secs(60),
        }
    }
}

/// One job for the desktop worker. Probes take priority over queued calls;
/// the upstream session's handshake state is never shared between threads.
pub(super) enum DesktopJob {
    /// Forward one `tools/call` verbatim and uncached (spec §8).
    Call {
        name: String,
        args: Value,
        reply: DeferredReply,
        cancel: Arc<AtomicBool>,
    },
    /// Check the desktop (module docs).
    Probe { call_triggered: bool },
}

/// What the worker tells the loop about the desktop.
struct Event {
    started: Instant,
    outcome: Result<Vec<Value>, String>,
}

#[derive(Default)]
struct Queue {
    calls: VecDeque<DesktopJob>,
    probe: Option<bool>,
    stopped: bool,
}

#[derive(Default)]
struct Work {
    queue: Mutex<Queue>,
    ready: Condvar,
}

/// The thread that owns the desktop upstream connection, plus the loop's
/// view of its state. Calls run one at a time, in arrival order, with
/// recovery probes ahead of queued calls. Dropping
/// it (or [`DesktopWorker::stop_taking_jobs`]) stops it taking jobs: a
/// call already in flight finishes within the upstream's own timeout and
/// answers; queued calls are answered with a shutdown error without being
/// sent (dropped if the loop has exited), and queued probes are skipped.
pub(super) struct DesktopWorker {
    jobs: Arc<Work>,
    stop: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
    connected: bool,
    /// The desktop's tools as of the last successful probe; only
    /// exposed while connected.
    tools: Vec<Value>,
    cadence: ProbeCadence,
    probing: bool,
    /// A probe was requested while one was in flight (which may have
    /// started before whatever prompted the request): probe again once it
    /// answers.
    recheck: bool,
    next_probe: Instant,
    last_probe: Option<Instant>,
    call_probe: bool,
}

impl DesktopWorker {
    /// A worker over an upstream whose startup handshake succeeded.
    #[cfg(test)]
    pub(super) fn spawn<U: UpstreamMcp + Send + 'static>(upstream: U) -> Self {
        Self::spawn_with(upstream, true, ProbeCadence::default())
    }

    /// A worker over `upstream`, connected or not per its startup
    /// handshake, re-checking it at `cadence`.
    pub(super) fn spawn_with<U: UpstreamMcp + Send + 'static>(
        mut upstream: U,
        connected: bool,
        cadence: ProbeCadence,
    ) -> Self {
        let tools = if connected {
            normalized_tools(upstream.tools().to_vec())
        } else {
            Vec::new()
        };
        let jobs = Arc::new(Work::default());
        let work = jobs.clone();
        let (events_tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::spawn(move || {
            let mut live = connected;
            let mut recover = false;
            let mut last_probe: Option<Instant> = None;
            loop {
                let job = {
                    let mut queue = work.queue.lock().unwrap();
                    loop {
                        if queue.stopped {
                            return;
                        }
                        if recover || queue.probe == Some(true) {
                            // A failed session is repaired before any queued
                            // calls, including those accepted before failure.
                            if let Some(last) = last_probe {
                                let delay = (last + MIN_PROBE_GAP)
                                    .saturating_duration_since(Instant::now());
                                if !delay.is_zero() {
                                    queue = work.ready.wait_timeout(queue, delay).unwrap().0;
                                    continue;
                                }
                            }
                            queue.probe = None;
                            break DesktopJob::Probe {
                                call_triggered: true,
                            };
                        }
                        if queue.probe.take().is_some() {
                            break DesktopJob::Probe {
                                call_triggered: false,
                            };
                        }
                        if let Some(job) = queue.calls.pop_front() {
                            break job;
                        }
                        queue = work.ready.wait(queue).unwrap();
                    }
                };
                match job {
                    DesktopJob::Call {
                        name,
                        args,
                        reply,
                        cancel,
                    } => {
                        // A socket/MCP request can contain a deeply nested
                        // argument. Drain it iteratively on every exit path,
                        // including cancellation and proxy panics.
                        let args = crate::deep_json::OwnedValue(args);
                        if stopped.load(Ordering::Relaxed) {
                            reply.send(Err(DESKTOP_SHUTDOWN.into()), false);
                            continue;
                        }
                        if cancel.load(Ordering::Relaxed) {
                            reply.send(
                                Err("desktop call cancelled before it was sent upstream".into()),
                                false,
                            );
                            continue;
                        }
                        if !live {
                            reply.send(
                                Err("Figma desktop is unreachable; call was not sent upstream"
                                    .into()),
                                false,
                            );
                            continue;
                        }
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            proxy::proxy_call(&mut upstream, &name, &args)
                        }));
                        match result {
                            Ok(Ok((value, write))) => reply.send(Ok(ToolOutput::Raw(value)), write),
                            Ok(Err(error)) => {
                                if matches!(error, UpstreamError::Unreachable(_)) {
                                    live = false;
                                }
                                // Publish the caller's answer before a probe can
                                // publish an exposed-tool-list change.
                                reply.send(Err(error.to_string()), false);
                                recover = true;
                            }
                            Err(_) => {
                                reply.send(Err(format!("desktop proxy worker panicked during {name}; inspect server stderr")), false);
                                live = false;
                                recover = true;
                            }
                        }
                    }
                    DesktopJob::Probe { .. } => {
                        last_probe = Some(Instant::now());
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                probe(&mut upstream, live)
                            }))
                            .unwrap_or_else(|_| Err("desktop probe panicked".into()));
                        recover = false;
                        live = outcome.is_ok();
                        let _ = events_tx.send(Event {
                            started: last_probe.unwrap(),
                            outcome,
                        });
                    }
                }
            }
        });
        let now = Instant::now();
        let next_probe = now
            + if connected {
                cadence.live
            } else {
                cadence.missing
            };
        Self {
            jobs,
            stop,
            events,
            connected,
            tools,
            cadence,
            probing: false,
            recheck: false,
            next_probe,
            last_probe: None,
            call_probe: false,
        }
    }

    /// `figmog_status`'s `upstream` value.
    pub(super) fn status(&self) -> &'static str {
        if self.connected {
            "connected"
        } else {
            "unreachable"
        }
    }

    /// The desktop tools to merge into `tools/list`: none while missing.
    pub(super) fn tools(&self) -> &[Value] {
        if self.connected { &self.tools } else { &[] }
    }

    /// Whether the desktop is currently connected.
    pub(super) fn connected(&self) -> bool {
        self.connected
    }

    /// Loop side, every iteration: apply probe outcomes and start a due
    /// probe. Returns whether the exposed tool list changed.
    pub(super) fn maintain(&mut self, now: Instant) -> bool {
        // Snapshot the exposed tools only when an event arrives: this runs
        // every loop iteration (as often as every 2 ms).
        let mut before = None;
        while let Ok(event) = self.events.try_recv() {
            before.get_or_insert_with(|| self.tools().to_vec());
            self.last_probe = Some(event.started);
            self.probing = false;
            self.apply_probe(event.outcome, now);
            if std::mem::take(&mut self.recheck) {
                self.request_probe(now);
            }
        }
        if !self.probing && now >= self.next_probe && !self.stop.load(Ordering::Relaxed) {
            self.probing = true;
            self.last_probe = Some(now);
            let call_triggered = std::mem::take(&mut self.call_probe);
            self.submit(DesktopJob::Probe { call_triggered });
        }
        before.is_some_and(|before| self.tools() != before.as_slice())
    }

    fn apply_probe(&mut self, outcome: Result<Vec<Value>, String>, now: Instant) {
        match outcome {
            Ok(tools) => {
                if !self.connected {
                    eprintln!(
                        "figmog: Figma desktop connected; serving its {} tool(s)",
                        tools.len()
                    );
                }
                self.connected = true;
                self.tools = normalized_tools(tools);
            }
            Err(error) => {
                if self.connected {
                    eprintln!(
                        "figmog: Figma desktop unreachable, serving local tools only: {error}"
                    );
                }
                self.connected = false;
                self.tools.clear();
            }
        }
        self.next_probe = now
            + if self.connected {
                self.cadence.live
            } else {
                self.cadence.missing
            };
    }

    /// Probe as soon as the call-triggered spacing allows; with a probe in
    /// flight, once it has answered.
    pub(super) fn request_probe(&mut self, now: Instant) {
        self.call_probe = true;
        if self.probing {
            self.recheck = true;
            return;
        }
        let earliest = self
            .last_probe
            .map_or(now, |last| (last + MIN_PROBE_GAP).max(now));
        self.next_probe = self.next_probe.min(earliest);
    }

    /// Shutdown: stop taking queued jobs. A call already in flight still
    /// finishes and answers; every queued one is answered with an error
    /// without reaching the upstream.
    pub(super) fn stop_taking_jobs(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let calls = {
            let mut queue = self.jobs.queue.lock().unwrap();
            queue.stopped = true;
            queue.probe = None;
            std::mem::take(&mut queue.calls)
        };
        self.jobs.ready.notify_all();
        for job in calls {
            reject_call(job, DESKTOP_SHUTDOWN);
        }
    }

    pub(super) fn submit(&self, job: DesktopJob) {
        let mut queue = self.jobs.queue.lock().unwrap();
        if queue.stopped {
            drop(queue);
            reject_call(job, DESKTOP_SHUTDOWN);
            return;
        }
        match job {
            DesktopJob::Probe { call_triggered } => {
                queue.probe = Some(queue.probe.unwrap_or(false) || call_triggered);
            }
            DesktopJob::Call { .. } if queue.calls.len() < MAX_QUEUED_CALLS => {
                queue.calls.push_back(job);
            }
            DesktopJob::Call { .. } => {
                drop(queue);
                reject_call(job, "desktop call queue is full; try again later");
                return;
            }
        }
        self.jobs.ready.notify_one();
    }
}

impl Drop for DesktopWorker {
    fn drop(&mut self) {
        self.stop_taking_jobs();
    }
}

/// Reject a queued call while preserving iterative cleanup of its arguments.
fn reject_call(job: DesktopJob, message: &str) {
    if let DesktopJob::Call { args, reply, .. } = job {
        let _args = crate::deep_json::OwnedValue(args);
        reply.send(Err(message.into()), false);
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        for job in self.calls.drain(..) {
            reject_call(job, DESKTOP_SHUTDOWN);
        }
    }
}

/// One probe on the worker: ping a session believed live, otherwise (or if
/// the ping fails) run a fresh handshake and report its tools.
fn probe<U: UpstreamMcp>(upstream: &mut U, live: bool) -> Result<Vec<Value>, String> {
    if live && upstream.ping().is_ok() && upstream.refresh_tools().is_ok() {
        return Ok(upstream.tools().to_vec());
    }
    upstream.initialize().map_err(|e| e.to_string())?;
    Ok(upstream.tools().to_vec())
}

/// Tool arrays have no semantic order. Sort by name, then the fields
/// `merge_registry` exposes, so ignored metadata cannot change tool order
/// or which definition wins a duplicate-name collision.
fn normalized_tools(mut tools: Vec<Value>) -> Vec<Value> {
    tools.sort_by_cached_key(|tool| {
        (
            tool.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tool.get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tool.get("inputSchema")
                .map_or_else(|| r#"{"type":"object"}"#.to_string(), Value::to_string),
        )
    });
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EXPECT: Duration = Duration::from_secs(3);

    struct Scripted {
        trace: mpsc::Sender<&'static str>,
        release: mpsc::Receiver<()>,
        calls: usize,
        block_probe: bool,
        fail_every_call: bool,
        tools: Vec<Value>,
    }

    impl UpstreamMcp for Scripted {
        fn initialize(&mut self) -> Result<(), UpstreamError> {
            self.trace.send("initialize").unwrap();
            if self.block_probe {
                self.release.recv_timeout(EXPECT).unwrap();
            }
            Ok(())
        }
        fn tools(&self) -> &[Value] {
            &self.tools
        }
        fn call(&mut self, _: &str, _: &Value) -> Result<Value, UpstreamError> {
            self.calls += 1;
            self.trace.send("call").unwrap();
            if self.calls == 1 {
                self.release.recv_timeout(EXPECT).unwrap();
                Err(UpstreamError::Protocol("stale session".into()))
            } else if self.fail_every_call {
                Err(UpstreamError::Protocol("stale session".into()))
            } else {
                Ok(json!({"content": [], "isError": false}))
            }
        }
        fn ping(&mut self) -> Result<(), UpstreamError> {
            self.trace.send("ping").unwrap();
            Err(UpstreamError::Protocol("stale session".into()))
        }
    }

    fn scripted(
        block_probe: bool,
    ) -> (
        DesktopWorker,
        mpsc::Receiver<&'static str>,
        mpsc::Sender<()>,
    ) {
        let (trace, events) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        (
            DesktopWorker::spawn_with(
                Scripted {
                    trace,
                    release: wait,
                    calls: 0,
                    block_probe,
                    fail_every_call: false,
                    tools: vec![],
                },
                true,
                ProbeCadence::default(),
            ),
            events,
            release,
        )
    }

    fn call(
        worker: &DesktopWorker,
        token: u64,
        cancel: Arc<AtomicBool>,
        done: &mpsc::Sender<super::super::Done>,
    ) {
        worker.submit(DesktopJob::Call {
            name: "get_code".into(),
            args: json!({}),
            reply: DeferredReply {
                token,
                done: Some(done.clone()),
            },
            cancel,
        });
    }

    #[test]
    fn recovery_runs_before_calls_already_queued_when_a_session_fails() {
        let (worker, trace, release) = scripted(false);
        let (done, answers) = mpsc::channel();
        call(&worker, 1, Arc::default(), &done);
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "call");
        call(&worker, 2, Arc::default(), &done);
        call(&worker, 3, Arc::default(), &done);
        release.send(()).unwrap();
        assert_eq!(answers.recv_timeout(EXPECT).unwrap().token, 1);
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "ping");
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "initialize");
        for token in [2, 3] {
            assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "call");
            let answer = answers.recv_timeout(EXPECT).unwrap();
            assert_eq!(answer.token, token);
            assert!(matches!(answer.answer, super::super::Answer::Output(Ok(_))));
        }
    }

    #[test]
    fn a_disconnected_call_is_never_forwarded_after_waiting_in_the_queue() {
        let (worker, trace, release) = scripted(false);
        let (done, answers) = mpsc::channel();
        call(&worker, 1, Arc::default(), &done);
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "call");
        let cancel = Arc::new(AtomicBool::new(false));
        call(&worker, 2, cancel.clone(), &done);
        cancel.store(true, Ordering::Relaxed);
        release.send(()).unwrap();
        assert_eq!(answers.recv_timeout(EXPECT).unwrap().token, 1);
        let answer = answers.recv_timeout(EXPECT).unwrap();
        assert_eq!(answer.token, 2);
        assert!(
            matches!(answer.answer, super::super::Answer::Output(Err(ref e)) if e.contains("cancelled"))
        );
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "ping");
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "initialize");
        assert!(trace.try_recv().is_err());
    }

    #[test]
    fn bounded_queue_and_stop_answer_immediately_even_with_a_blocked_probe() {
        let (worker, trace, release) = scripted(true);
        let (done, answers) = mpsc::channel();
        worker.submit(DesktopJob::Probe {
            call_triggered: false,
        });
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "ping");
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "initialize");
        for token in 0..MAX_QUEUED_CALLS as u64 {
            call(&worker, token, Arc::default(), &done);
        }
        call(&worker, 999, Arc::default(), &done);
        let rejected = answers.recv_timeout(EXPECT).unwrap();
        assert_eq!(rejected.token, 999);
        assert!(
            matches!(rejected.answer, super::super::Answer::Output(Err(ref e)) if e.contains("queue is full"))
        );
        worker.stop_taking_jobs();
        for token in 0..MAX_QUEUED_CALLS as u64 {
            let answer = answers.recv_timeout(EXPECT).unwrap();
            assert_eq!(answer.token, token);
            assert!(
                matches!(answer.answer, super::super::Answer::Output(Err(ref e)) if e == DESKTOP_SHUTDOWN)
            );
        }
        release.send(()).unwrap();
        assert!(trace.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn call_triggered_probe_requests_keep_the_one_second_minimum_gap() {
        let (mut worker, trace, _) = scripted(false);
        let now = Instant::now();
        worker.request_probe(now);
        worker.maintain(now);
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "ping");
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "initialize");
        // Wait for the event, without relying on scheduling sleeps.
        let event = worker.events.recv_timeout(EXPECT).unwrap();
        let Event { outcome, .. } = event;
        worker.probing = false;
        worker.apply_probe(outcome, now);
        worker.request_probe(now + Duration::from_millis(10));
        worker.maintain(now + MIN_PROBE_GAP - Duration::from_nanos(1));
        assert!(trace.try_recv().is_err());
        worker.maintain(now + MIN_PROBE_GAP);
        assert_eq!(trace.recv_timeout(EXPECT).unwrap(), "ping");
    }

    #[test]
    fn actual_worker_spaces_recovery_and_delayed_call_triggered_probes() {
        let (trace, _trace_rx) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = DesktopWorker::spawn_with(
            Scripted {
                trace,
                release: wait,
                calls: 0,
                block_probe: false,
                fail_every_call: true,
                tools: vec![],
            },
            true,
            ProbeCadence::default(),
        );
        let (done, answers) = mpsc::channel();
        call(&worker, 1, Arc::default(), &done);
        call(&worker, 2, Arc::default(), &done);
        release.send(()).unwrap();
        answers.recv_timeout(EXPECT).unwrap();
        let first = worker.events.recv_timeout(EXPECT).unwrap();
        answers.recv_timeout(EXPECT).unwrap();
        // Simulate the loop scheduling a call-triggered check while the
        // worker has already started its own recovery.
        worker.submit(DesktopJob::Probe {
            call_triggered: true,
        });
        let second = worker.events.recv_timeout(EXPECT).unwrap();
        assert!(second.started.duration_since(first.started) >= MIN_PROBE_GAP);
        // A delayed loop request submitted after that recovery is throttled
        // at the worker too, even without maintain applying either event.
        worker.submit(DesktopJob::Probe {
            call_triggered: true,
        });
        let third = worker.events.recv_timeout(EXPECT).unwrap();
        assert!(third.started.duration_since(second.started) >= MIN_PROBE_GAP);
    }

    #[test]
    fn a_live_probe_refreshes_tools_and_rehandshakes_if_refresh_fails() {
        struct Registry {
            tools: Vec<Value>,
            fail_refresh: bool,
            handshakes: usize,
        }
        impl UpstreamMcp for Registry {
            fn initialize(&mut self) -> Result<(), UpstreamError> {
                self.handshakes += 1;
                self.tools = vec![json!({"name":"after_handshake"})];
                Ok(())
            }
            fn tools(&self) -> &[Value] {
                &self.tools
            }
            fn call(&mut self, _: &str, _: &Value) -> Result<Value, UpstreamError> {
                unreachable!()
            }
            fn ping(&mut self) -> Result<(), UpstreamError> {
                Ok(())
            }
            fn refresh_tools(&mut self) -> Result<(), UpstreamError> {
                if self.fail_refresh {
                    return Err(UpstreamError::Protocol("stale session".into()));
                }
                self.tools = vec![json!({"name":"after_refresh"})];
                Ok(())
            }
        }
        let mut upstream = Registry {
            tools: vec![],
            fail_refresh: false,
            handshakes: 0,
        };
        assert_eq!(
            probe(&mut upstream, true).unwrap()[0]["name"],
            "after_refresh"
        );
        assert_eq!(upstream.handshakes, 0);
        upstream.fail_refresh = true;
        assert_eq!(
            probe(&mut upstream, true).unwrap()[0]["name"],
            "after_handshake"
        );
        assert_eq!(upstream.handshakes, 1);
    }

    #[test]
    fn deep_arguments_are_drained_on_cancel_rejection_and_queue_drop() {
        crate::test_support::small_stack(|| {
            fn job(token: u64, done: &mpsc::Sender<super::super::Done>) -> DesktopJob {
                let mut args = Value::Null;
                for _ in 0..10_000 {
                    args = Value::Array(vec![args]);
                }
                DesktopJob::Call {
                    name: "get_code".into(),
                    args,
                    reply: DeferredReply {
                        token,
                        done: Some(done.clone()),
                    },
                    cancel: Arc::new(AtomicBool::new(true)),
                }
            }
            let (worker, _, _) = scripted(false);
            let (done, answers) = mpsc::channel();
            worker.submit(job(1, &done));
            assert_eq!(answers.recv_timeout(EXPECT).unwrap().token, 1);
            worker.stop_taking_jobs();
            worker.submit(job(2, &done));
            assert_eq!(answers.recv_timeout(EXPECT).unwrap().token, 2);
            let mut queue = Queue::default();
            queue.calls.push_back(job(3, &done));
            drop(queue);
            assert_eq!(answers.recv_timeout(EXPECT).unwrap().token, 3);
        });
    }

    #[test]
    fn tool_order_does_not_change_the_exposed_registry() {
        let (mut worker, _, _) = scripted(false);
        let tools = vec![json!({"name":"z"}), json!({"name":"a"})];
        worker.apply_probe(Ok(tools.clone()), Instant::now());
        let before = worker.tools().to_vec();
        worker.apply_probe(Ok(tools.into_iter().rev().collect()), Instant::now());
        assert_eq!(worker.tools(), before);
    }

    #[test]
    fn unsupported_tool_metadata_does_not_change_duplicate_selection() {
        let tools = vec![
            json!({"name":"get_a", "description":"A"}),
            json!({"name":"get_z", "description":"A"}),
            json!({"name":"get_z", "description":"Z"}),
        ];
        let (mut worker, _, _) = scripted(false);
        worker.apply_probe(Ok(tools.clone()), Instant::now());
        let before = proxy::merge_registry(Vec::new(), worker.tools()).0;
        assert_eq!(before[1].description, "[via Figma desktop] A");

        let mut changed = tools;
        changed[2]["_meta"] = json!({"internal":true});
        changed.reverse();
        worker.apply_probe(Ok(changed), Instant::now());
        let after = proxy::merge_registry(Vec::new(), worker.tools()).0;
        assert_eq!(before, after);
    }
}
