//! The desktop worker: the one thread that owns the Figma desktop app's
//! upstream connection (NWS-2955), and the loop-side record of whether the
//! desktop is reachable and which tools it offers (NWS-2959).
//!
//! **Re-detection.** Serve probes the desktop once at startup (spec §8),
//! then keeps checking, always on this worker and never on the loop:
//!
//! - while it is missing, a full handshake every [`ProbeCadence::missing`]
//!   (30 s);
//! - while it is connected, a liveness `ping` every [`ProbeCadence::live`]
//!   (60 s), falling back to a fresh handshake when the ping fails (a
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
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

/// One job for the desktop worker. Calls and probes share one queue, so
/// the upstream session's handshake state is never shared between threads.
pub(super) enum DesktopJob {
    /// Forward one `tools/call` verbatim and uncached (spec §8).
    Call {
        name: String,
        args: Value,
        reply: DeferredReply,
    },
    /// Check the desktop (module docs).
    Probe,
}

/// What the worker tells the loop about the desktop.
enum Event {
    /// A probe finished: `Ok(None)` means the live session answered a ping
    /// (tools unchanged); `Ok(Some(tools))` a fresh handshake succeeded.
    Probed(Result<Option<Vec<Value>>, String>),
    /// A forwarded call failed below the tool level: worth a probe.
    CallFailed,
}

/// The thread that owns the desktop upstream connection, plus the loop's
/// view of its state. Jobs run one at a time, in arrival order. Dropping
/// it (or [`DesktopWorker::stop_taking_jobs`]) stops it taking jobs: a
/// call already in flight finishes within the upstream's own timeout and
/// answers; queued calls are answered with a shutdown error without being
/// sent (dropped if the loop has exited), and queued probes are skipped.
pub(super) struct DesktopWorker {
    jobs: mpsc::Sender<DesktopJob>,
    stop: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
    connected: bool,
    /// The desktop's tools as of the last successful handshake; only
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
            upstream.tools().to_vec()
        } else {
            Vec::new()
        };
        let (jobs, work) = mpsc::channel::<DesktopJob>();
        let (events_tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::spawn(move || {
            // Whether the session from the last handshake is believed
            // alive: a probe then only pings it.
            let mut live = connected;
            while let Ok(job) = work.recv() {
                if stopped.load(Ordering::Relaxed) {
                    // Shutdown (spec §6a): a queued call is never sent
                    // upstream — it may be a write — and its caller gets
                    // an error instead of silence.
                    if let DesktopJob::Call { reply, .. } = job {
                        reply.send(Err(DESKTOP_SHUTDOWN.into()), false);
                    }
                    continue;
                }
                match job {
                    DesktopJob::Call { name, args, reply } => {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            proxy::proxy_call(&mut upstream, &name, &args)
                        }));
                        match result {
                            // A proxied result is already a complete MCP
                            // `CallToolResult` from the upstream — emit it
                            // verbatim (spec §11/§12; see
                            // `mcp::ToolOutput::Raw`'s doc comment) rather
                            // than re-wrapping it as figmog's own text-block
                            // shape.
                            Ok(Ok((value, write))) => reply.send(Ok(ToolOutput::Raw(value)), write),
                            Ok(Err(error)) => {
                                if matches!(error, UpstreamError::Unreachable(_)) {
                                    live = false;
                                }
                                let _ = events_tx.send(Event::CallFailed);
                                reply.send(Err(error.to_string()), false);
                            }
                            Err(_) => reply.send(
                                Err(format!(
                                    "desktop proxy worker panicked during {name}; inspect server stderr"
                                )),
                                false,
                            ),
                        }
                    }
                    DesktopJob::Probe => {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                probe(&mut upstream, live)
                            }))
                            .unwrap_or_else(|_| Err("desktop probe panicked".into()));
                        live = outcome.is_ok();
                        let _ = events_tx.send(Event::Probed(outcome));
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
            match event {
                Event::Probed(outcome) => {
                    self.probing = false;
                    self.apply_probe(outcome, now);
                    if std::mem::take(&mut self.recheck) {
                        self.request_probe(now);
                    }
                }
                Event::CallFailed => self.request_probe(now),
            }
        }
        if !self.probing && now >= self.next_probe && !self.stop.load(Ordering::Relaxed) {
            self.probing = true;
            self.last_probe = Some(now);
            self.submit(DesktopJob::Probe);
        }
        before.is_some_and(|before| self.tools() != before.as_slice())
    }

    fn apply_probe(&mut self, outcome: Result<Option<Vec<Value>>, String>, now: Instant) {
        match outcome {
            Ok(None) => {}
            Ok(Some(tools)) => {
                if !self.connected {
                    eprintln!(
                        "figmog: Figma desktop connected; serving its {} tool(s)",
                        tools.len()
                    );
                }
                self.connected = true;
                self.tools = tools;
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
    }

    pub(super) fn submit(&self, job: DesktopJob) {
        // A dead worker hands the job back; dropping it answers the
        // caller with an error.
        let _ = self.jobs.send(job);
    }
}

impl Drop for DesktopWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One probe on the worker: ping a session believed live, otherwise (or if
/// the ping fails) run a fresh handshake and report its tools.
fn probe<U: UpstreamMcp>(upstream: &mut U, live: bool) -> Result<Option<Vec<Value>>, String> {
    if live && upstream.ping().is_ok() {
        return Ok(None);
    }
    upstream.initialize().map_err(|e| e.to_string())?;
    Ok(Some(upstream.tools().to_vec()))
}
