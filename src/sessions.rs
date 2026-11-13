//! Per-file background workers and routing for multi-file serve.
//!
//! Each worker owns the only writer. Dispatch reads a separately pinned
//! committed view, published only after fetch, flatten, sync and eviction
//! finish. Workers also own watch watermarks, so a metadata observation
//! cannot acknowledge a fetch that preceded it.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fold::pipeline::Push;
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{FigmaApi, UreqApi};
use crate::cli::{PullError, now_ms, pull_failure_wait};
use crate::dispatch;
use crate::flatten::flatten_file;
use crate::ident::parse_file_ref;
use crate::mcp::ToolOutput;
use crate::model::Id;
use crate::project::Layout;
use crate::session_view::{ReadGate, ReadLease, SessionView, SharedPipeline};
use crate::store::{self, Churn, collect_sweepable, collect_variable_ids};
use crate::watch::{BACKOFF_START, Tick, Watcher};

type SessionApi = Arc<dyn FigmaApi + Send + Sync>;

const WORKER_GONE: &str =
    "this file's worker stopped unexpectedly; inspect server stderr and restart figmog serve";

/// Image jobs waiting behind one file's active operation, independent of
/// how many requesters connect or disconnect while it runs.
pub(crate) const MAX_QUEUED_IMAGES: usize = 128;

/// Delivers one `figmog_images` result from the file's worker thread back
/// to whoever asked (NWS-2955: `serve` turns it into a deferred response).
/// Dropping it uncalled — a discarded queue on shutdown, or a worker that
/// died mid-request — is the deliverer's to report.
pub(crate) type ImageReply = Box<dyn FnOnce(Result<Vec<crate::images::ImageItem>, String>) + Send>;

/// One accepted image request, waiting for (or running on) the worker.
struct ImageJob {
    ids: Vec<String>,
    format: String,
    scale: Option<f64>,
    /// Render `ids` (`false`: only fetch their fill images, NWS-2952).
    renders: bool,
    /// How cached renders are treated (NWS-2958).
    policy: crate::images::RenderPolicy,
    reply: ImageReply,
}

enum Command {
    Pull {
        geometry: bool,
    },
    Poll {
        expedited: bool,
    },
    Images(ImageJob),
    /// A CLI writer operation (NWS-2963), or one step of it.
    Write(WriteWork),
}

enum Completion {
    Pull(Result<(PullOutcome, Arc<SessionView>), PullError>),
    /// A [`Command::Write`] finished (its waiters are answered on the loop).
    Write(Result<WriteResult, String>),
    Phase(PullPhase),
    Poll {
        tick: Tick,
        expedited: bool,
    },
    /// An image request finished (its reply already went to the caller).
    Images,
}

#[derive(Debug)]
struct PullOutcome {
    churn: Churn,
    name: String,
    version: String,
}

/// Delivers one CLI write's result (NWS-2963) from the loop back to whoever
/// asked: `Ok` only once the write committed *and* its view was published.
/// Dropping it uncalled is the deliverer's to report (as "outcome unknown").
pub(crate) type WriteReply = Box<dyn FnOnce(Result<Value, String>) + Send>;

/// Where a CLI pull's file response comes from.
pub(crate) enum PullSource {
    /// `GET /v1/files/:key` (and `variables/local`) with serve's credentials.
    Network,
    /// A saved `GET /v1/files/:key` response (`pull --from-file`), sent by
    /// the CLI as content.
    File(crate::deep_json::OwnedValue),
}

/// A writer operation the CLI routes through a running serve (NWS-2963).
/// Each one is applied by the file's worker, the store's only writer;
/// distinct operations are never merged, except identical plain pulls
/// still waiting to start.
pub(crate) enum WriteOp {
    /// `figmog pull [--fresh] [--from-file] [--geometry]`.
    Pull {
        source: PullSource,
        fresh: bool,
        geometry: bool,
    },
    /// `figmog import-variables`: the export's raw bytes (their digest is
    /// the snapshot's) and the name to report validation errors under.
    Import {
        content: Vec<u8>,
        label: String,
        allow_empty: bool,
        dry_run: bool,
    },
}

impl WriteOp {
    fn is_plain_pull(&self, with_geometry: bool) -> bool {
        matches!(self, WriteOp::Pull { source: PullSource::Network, fresh: false, geometry }
            if *geometry == with_geometry)
    }

    fn into_work(self) -> WriteWork {
        match self {
            WriteOp::Pull {
                source,
                fresh,
                geometry,
            } => WriteWork::Pull {
                source,
                fresh,
                geometry,
            },
            WriteOp::Import {
                content,
                label,
                allow_empty,
                dry_run,
            } => WriteWork::Import {
                content,
                label,
                allow_empty,
                dry_run,
            },
        }
    }
}

/// What the worker runs for a [`WriteOp`]. A fresh pull is two steps:
/// `Pull { fresh: true }` only validates (fetch/parse/flatten, touching no
/// store) and returns the [`Prepared`] data; the loop then resets the store
/// behind the per-store reset barrier and the reopened session's worker
/// runs `Apply`.
enum WriteWork {
    Pull {
        source: PullSource,
        fresh: bool,
        geometry: bool,
    },
    Apply(Prepared),
    Import {
        content: Vec<u8>,
        label: String,
        allow_empty: bool,
        dry_run: bool,
    },
}

/// The answer to a CLI write still queued when serve shuts down
/// ([`FileSession::begin_drain`]): it never started, so nothing was applied
/// and the requester may send it again.
pub(crate) const WRITE_NOT_STARTED: &str = "not accepted: figmog serve is shutting down and did not start this write; nothing was applied — reconnect and retry";

/// A validated pull, not yet applied to any store.
pub(crate) struct Prepared {
    flattened: crate::flatten::Flattened,
    has_vars: bool,
    /// The geometry flag to persist (OR'd with the store's at apply time).
    geometry: bool,
    /// Fetched from Figma: a successful apply acknowledges a pending watch
    /// observation, like any network pull.
    network: bool,
}

enum WriteResult {
    Applied(Applied),
    Prepared(Prepared),
}

struct Applied {
    /// The CLI's result: pull churn, or the import summary.
    value: Value,
    view: Arc<SessionView>,
    mirrored: bool,
    geometry: bool,
    outcome: Option<PullOutcome>,
}

/// Everyone waiting on one write (identical plain pulls share one).
struct Waiters {
    /// The first waiter's operation id, recorded in `last_success`.
    operation: String,
    replies: Vec<WriteReply>,
}

impl Waiters {
    fn answer(self, result: &Result<Value, String>) {
        for reply in self.replies {
            reply(result.clone());
        }
    }
}

/// An accepted write waiting for the worker.
struct PendingWrite {
    op: WriteOp,
    waiters: Waiters,
}

/// Accepted work and watch scheduling a store reset carries from the
/// closed session to its reopened one.
pub(crate) struct Carry {
    fresh: Option<(Prepared, Waiters)>,
    writes: VecDeque<PendingWrite>,
    draining: bool,
    job: Option<Job>,
    queued: Option<Job>,
    next_job: u64,
    poll_owed: bool,
    backoff: Duration,
    retry_at: Instant,
    next_poll: Instant,
    poll_retry_at: Option<Instant>,
    watch_retry_at: Option<Instant>,
    expedited_poll: bool,
    interval: Duration,
}

impl Carry {
    fn fail(self, error: &str) {
        let error = Err(error.to_string());
        if let Some((_, waiters)) = self.fresh {
            waiters.answer(&error);
        }
        for write in self.writes {
            write.waiters.answer(&error);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum PullPhase {
    Fetching,
    Flattening,
    Applying,
    Publishing,
}

impl PullPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fetching => "fetching",
            Self::Flattening => "flattening",
            Self::Applying => "applying",
            Self::Publishing => "publishing",
        }
    }
}

#[derive(Clone)]
struct Job {
    id: u64,
    state: JobState,
    geometry: bool,
    error: Option<String>,
    phase: Option<PullPhase>,
}

impl Job {
    fn json(&self) -> Value {
        json!({"id": self.id, "state": self.state, "geometry": self.geometry, "error": self.error, "phase": self.phase})
    }
}

/// A session's control plane. No local read waits for the worker or locks
/// its pipeline, and nothing here awaits a worker reply: image requests run
/// on the worker too and answer through their [`ImageReply`].
pub(crate) struct FileSession {
    pub(crate) key: String,
    pub(crate) mirrored: bool,
    pub(crate) path: PathBuf,
    fresh_applying: bool,
    view: Arc<SessionView>,
    commands: Option<mpsc::SyncSender<Command>>,
    completions: mpsc::Receiver<Completion>,
    /// Returns the closed store, still owned, for shutdown's compaction.
    worker: Option<JoinHandle<crate::cli::Compaction>>,
    busy: bool,
    polling: bool,
    /// The worker is busy with an image request (not a pull or poll).
    imaging: bool,
    /// Image requests accepted while another one runs, in arrival order,
    /// bounded by `MAX_QUEUED_IMAGES` even after callers disconnect.
    image_queue: VecDeque<ImageJob>,
    /// A watch poll came due while an image request held the worker.
    poll_owed: bool,
    job: Option<Job>,
    queued: Option<Job>,
    last_completed: Option<Value>,
    last_success: Option<Value>,
    next_job: u64,
    geometry: bool,
    backoff: Duration,
    retry_at: Instant,
    next_poll: Instant,
    poll_retry_at: Option<Instant>,
    // Watch recovery for pull failures; metadata recovery is independent.
    watch_retry_at: Option<Instant>,
    expedited_poll: bool,
    interval: Duration,
    /// Admission for off-loop reads of `view` (NWS-2962 reset barrier).
    gate: Arc<ReadGate>,
    /// A close/reset waiting for this store's worker reads to drain; while
    /// set, no new read is admitted and no worker command starts.
    reset: Option<StoreReset>,
    /// Reopens this store at the same path with the same key, credentials
    /// and hooks — the reset barrier's step 4. `true` clears the store's
    /// contents first (a fresh pull).
    reopen: Reopen,
    /// CLI writes accepted and not yet started, in arrival order (NWS-2963).
    writes: VecDeque<PendingWrite>,
    /// The waiters of the write the worker is running.
    writing: Option<Waiters>,
    /// A validated fresh pull waiting for this store's reset barrier.
    fresh: Option<(Prepared, Waiters)>,
    /// Shutdown's drain ([`Self::begin_drain`]): in-flight work finishes
    /// and publishes, nothing new starts.
    draining: bool,
}

type Reopen = Arc<dyn Fn(bool) -> Result<FileSession, String> + Send + Sync>;

/// A store-level operation that must not run underneath live views; it
/// waits behind the per-store reset barrier ([`ReadGate`]). Append-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreReset {
    /// Close the store and reopen it from disk, publishing a new session
    /// (fresh job history, like a reopened mirror).
    #[cfg_attr(not(test), allow(dead_code))] // only tests reopen without a write
    Reopen,
    /// A fresh pull (NWS-2963): close the store, clear its contents (the
    /// ownership anchor stays), reopen it empty and apply the session's
    /// prepared pull there.
    Fresh,
}

/// A status read captured on the loop and finished on a read worker: the
/// lifecycle is the loop's state when the request arrived; the store part
/// comes from the leased published view.
pub(crate) struct StatusRead {
    mirrored: bool,
    lifecycle: Value,
    lease: Option<ReadLease>,
}

impl StatusRead {
    pub(crate) fn run(&self) -> Result<Value, String> {
        status_value(
            self.mirrored,
            self.lifecycle.clone(),
            self.lease.as_ref().map(ReadLease::view),
        )
    }
}

/// `figmog_files` captured on the loop, finished on a read worker.
pub(crate) struct FilesRead(Vec<(String, bool, StatusRead)>);

impl FilesRead {
    pub(crate) fn run(&self) -> Value {
        json!(self.0.iter().map(|(key, default, read)| {
            let status = read.run().unwrap_or(Value::Null);
            json!({"key": key, "name": status["name"], "version": status["version"], "nodes": status["nodes"],
                "last_synced": status["synced_at_unix_ms"], "default": default,
                "ready": read.mirrored, "pull": read.lifecycle})
        }).collect::<Vec<_>>())
    }
}

/// Answer one local read tool from a published view.
pub(crate) fn read_tool(
    view: &SessionView,
    name: &str,
    args: &Value,
) -> Result<ToolOutput, String> {
    // Only status reads the node count; don't count for anything else.
    let node_count = (name == "figmog_status").then(|| view.node_count());
    view.with_dependent(|_, readers| {
        dispatch::dispatch_read_tool(name, args, "", readers, node_count)
            .ok_or_else(|| format!("unknown tool: {name}"))?
            .map(ToolOutput::Json)
    })
}

/// A session's status JSON. `view` is required for a mirrored session; a
/// missing one (its store is resetting) is an error. A not-mirrored
/// session still reads its view when it has one: the store may already
/// hold an imported variable snapshot (SPEC §9 "Import-only stores"), and
/// the shared status query then answers with null file fields plus the
/// `variables` summary. Readiness stays false either way — a snapshot is
/// not a mirrored file. With no variable state the query reports "no
/// mirror here" (and a resetting store has no view), so the bare shape
/// stands.
fn status_value(
    mirrored: bool,
    lifecycle: Value,
    view: Option<&SessionView>,
) -> Result<Value, String> {
    let status_json = |view| match read_tool(view, "figmog_status", &json!({}))? {
        ToolOutput::Json(value) => Ok::<_, String>(value),
        ToolOutput::Raw(_) | ToolOutput::Deferred(_) => {
            unreachable!("local read tools return JSON")
        }
    };
    let mut value = match (mirrored, view) {
        (true, Some(view)) => status_json(view)?,
        (true, None) => return Err(RESETTING.to_string()),
        (false, view) => view
            .and_then(|view| status_json(view).ok())
            .unwrap_or_else(|| {
                json!({"name": null, "version": null, "last_modified": null,
                "synced_at_unix_ms": null, "nodes": 0,
                "variables": view.map(|view| view.with_dependent(
                    |_, (_, _, _, _, variables, collections, _, _, _, _, _, snapshot, ..)| {
                        crate::query::variable_snapshot(variables, collections, snapshot)
                    }
                ))})
            }),
    };
    value["ready"] = json!(mirrored);
    value["pull"] = lifecycle;
    Ok(value)
}

const RESETTING: &str = "this mirror's store is being reset; retry shortly";

impl Drop for FileSession {
    fn drop(&mut self) {
        // Graceful shutdown drains the one running command. A queued
        // follow-up has not started and is discarded. Client disconnects
        // never drop a session, so they do not cancel accepted work.
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl FileSession {
    /// Drain and close this session (as dropping it does), returning its
    /// closed but still-owned store once the session — and its published
    /// view — is gone. `None` if the worker panicked.
    fn close_until(mut self, deadline: Instant) -> Option<crate::cli::Compaction> {
        self.commands.take();
        let worker = self.worker.take()?;
        while !worker.is_finished() {
            if Instant::now() >= deadline {
                // The process is exiting. Detach the worker with its store
                // ownership intact rather than blocking past the drain budget.
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        worker.join().ok()
    }

    #[cfg(test)]
    pub(crate) fn before_reopen(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        let reopen = self.reopen.clone();
        self.reopen = Arc::new(move |fresh| {
            hook();
            reopen(fresh)
        });
    }

    pub(crate) fn lifecycle(&self) -> Value {
        json!({
            "job": self.job.as_ref().map(Job::json),
            "queued": self.queued.as_ref().map(Job::json),
            "last_completed": self.last_completed,
            "last_success": self.last_success,
            "polling": self.polling,
        })
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> Result<Value, String> {
        self.status_read().run()
    }

    /// Capture a status read for a read worker. The lease goes through the
    /// reset barrier even when not mirrored (an import-only store still
    /// reports its variable snapshot). A session whose store is resetting
    /// captures no view: a mirrored status read then fails, a not-mirrored
    /// one falls back to the bare shape.
    pub(crate) fn status_read(&self) -> StatusRead {
        StatusRead {
            mirrored: self.mirrored || self.fresh_applying,
            lifecycle: self.lifecycle(),
            lease: self.gate.admit(&self.view),
        }
    }

    /// The not-ready and resetting refusals every data read shares.
    fn check_readable(&self, name: &str) -> Result<(), String> {
        if self.gate.is_closed() {
            return Err(format!("mirror {}: {RESETTING}", self.key));
        }
        if !(self.mirrored || name == "figmog_vars" && self.has_variable_state()) {
            return Err(format!(
                "mirror {} is not ready; inspect figmog_status or figmog_files for pull progress and errors",
                self.key
            ));
        }
        if self.gate.is_closed() {
            return Err(format!("mirror {}: {RESETTING}", self.key));
        }
        Ok(())
    }

    /// Admit one off-loop data read of the published view (NWS-2962).
    #[cfg(test)]
    pub(crate) fn lease(&self) -> Result<ReadLease, String> {
        self.lease_for("")
    }

    pub(crate) fn lease_for(&self, name: &str) -> Result<ReadLease, String> {
        self.check_readable(name)?;
        self.gate
            .admit(&self.view)
            .ok_or_else(|| format!("mirror {}: {RESETTING}", self.key))
    }

    pub(crate) fn dispatch(&self, name: &str, args: &Value) -> Result<ToolOutput, String> {
        self.check_readable(name)?;
        read_tool(&self.view, name, args)
    }

    fn variable_snapshot(&self) -> Value {
        self.view.with_dependent(
            |_, (_, _, _, _, variables, collections, _, _, _, _, _, snapshot, ..)| {
                crate::query::variable_snapshot(variables, collections, snapshot)
            },
        )
    }

    pub(crate) fn has_variable_state(&self) -> bool {
        self.variable_snapshot()["state"] != "none"
    }

    fn new_job(&mut self) -> Job {
        let job = Job {
            id: self.next_job,
            state: JobState::Queued,
            geometry: self.geometry,
            error: None,
            phase: None,
        };
        self.next_job += 1;
        job
    }

    /// Repeated requests share the active/queued job. The sole follow-up
    /// case is a geometry upgrade that the running fetch cannot satisfy.
    pub(crate) fn request_pull(&mut self, geometry: bool, interval: Duration) -> Value {
        self.interval = interval;
        self.geometry |= geometry;
        if let Some(queued) = &mut self.queued {
            queued.geometry |= self.geometry;
            let id = queued.id;
            return self.accepted(id);
        }
        if let Some(job) = &mut self.job
            && matches!(job.state, JobState::Queued | JobState::Running)
        {
            if job.state == JobState::Queued {
                job.geometry |= self.geometry;
                let id = job.id;
                return self.accepted(id);
            }
            if !self.geometry || job.geometry {
                let id = job.id;
                return self.accepted(id);
            }
            let queued = self.new_job();
            let id = queued.id;
            self.queued = Some(queued);
            return self.accepted(id);
        }
        self.job = Some(self.new_job());
        self.start_if_ready();
        self.accepted(self.job.as_ref().unwrap().id)
    }

    fn accepted(&self, id: u64) -> Value {
        let job = self
            .queued
            .as_ref()
            .filter(|j| j.id == id)
            .or_else(|| self.job.as_ref().filter(|j| j.id == id))
            .unwrap();
        json!({"accepted": true, "key": self.key, "job_id": id, "state": job.state})
    }

    fn start_if_ready(&mut self) {
        self.start_if_ready_at(Instant::now());
    }

    /// Start the queued pull job, if the worker is free. Never while
    /// draining (NWS-2963): shutdown's barrier sits here, at the one
    /// dispatch point every pull request reaches — an explicit sync, a
    /// queued follow-up, and a completed watch poll's `Changed` alike.
    /// Pending CLI writes keep the next free slot even when a completed
    /// poll requests its follow-up before the completion loop finishes.
    fn start_if_ready_at(&mut self, now: Instant) {
        if self.draining
            || self.busy
            || self.reset.is_some()
            || !self.writes.is_empty()
            || now < self.retry_at
        {
            return;
        }
        let Some(geometry) = self
            .job
            .as_ref()
            .filter(|job| job.state == JobState::Queued)
            .map(|job| job.geometry)
        else {
            return;
        };
        let sent = self.send(Command::Pull { geometry }).is_ok();
        let job = self.job.as_mut().expect("checked above");
        if sent {
            job.state = JobState::Running;
            self.busy = true;
        } else {
            job.state = JobState::Failed;
            job.error = Some(WORKER_GONE.to_string());
            self.last_completed = Some(job.json());
            if let Some(queued) = self.queued.take() {
                self.job = Some(queued);
            }
        }
    }

    /// Hand one command to the worker. The bounded slot is free whenever
    /// `busy` is false, so this never blocks; it fails only if the worker
    /// thread is gone, and hands the command back.
    fn send(&self, command: Command) -> Result<(), Command> {
        match &self.commands {
            Some(commands) => commands.send(command).map_err(|e| e.0),
            None => Err(command),
        }
    }

    /// Apply completions in dispatch order, exactly once. Workers run
    /// serially per file, so neither an old poll nor an old pull can
    /// overwrite a newer publication. `watch` is false under `--no-watch`:
    /// completions and queued explicit pulls and image requests still
    /// advance, but no watch poll or retry is dispatched (spec §6).
    pub(crate) fn maintain(&mut self, watch: bool) -> bool {
        self.maintain_at(Instant::now(), watch)
    }

    pub(crate) fn maintain_at(&mut self, now: Instant, watch: bool) -> bool {
        let mut published = false;
        while let Ok(completion) = self.completions.try_recv() {
            if let Completion::Phase(phase) = completion {
                if let Some(job) = &mut self.job {
                    job.phase = Some(phase);
                }
                continue;
            }
            self.busy = false;
            match completion {
                Completion::Phase(_) => unreachable!("handled above"),
                Completion::Images => self.imaging = false,
                Completion::Poll { tick, expedited } => {
                    self.polling = false;
                    match tick {
                        Tick::Changed => {
                            self.request_pull(false, self.interval);
                        }
                        // Ordinary cadence is anchored to dispatch, just
                        // like the global round-robin tick. Completion
                        // latency must not make the next round ineligible.
                        Tick::Unchanged => {}
                        Tick::Wait { after } => {
                            self.next_poll = now + after;
                            self.poll_retry_at = Some(self.next_poll);
                            // A failed expedited observation still needs
                            // reconciliation once its retry delay expires.
                            self.expedited_poll |= expedited;
                        }
                    }
                }
                Completion::Write(result) => {
                    let waiters = self.writing.take().expect("write has waiters");
                    match result {
                        Ok(WriteResult::Applied(applied)) => {
                            self.view = applied.view;
                            if self.fresh_applying {
                                self.fresh_applying = false;
                                self.gate = Arc::default();
                            }
                            self.mirrored = applied.mirrored;
                            self.geometry |= applied.geometry;
                            if let Some(outcome) = applied.outcome {
                                self.last_success = Some(json!({"job_id": null,
                                    "operation": waiters.operation, "name": outcome.name,
                                    "version": outcome.version, "churn": outcome.churn}));
                                self.backoff = BACKOFF_START;
                                self.retry_at = now;
                                // A failed lifecycle pull defers both its
                                // retry and ordinary watch rounds. Retire
                                // only that deferral; metadata rate limits
                                // still apply through `poll_retry_at`.
                                if self
                                    .watch_retry_at
                                    .take()
                                    .is_some_and(|retry| self.next_poll == retry)
                                {
                                    self.next_poll = now;
                                }
                                published = true;
                            }
                            // Committed and published: only now answer.
                            waiters.answer(&Ok(applied.value));
                        }
                        Ok(WriteResult::Prepared(prepared)) => {
                            // Reset barrier steps 1–2; `service_resets`
                            // clears and reopens once reads have drained.
                            self.gate.close();
                            self.reset = Some(StoreReset::Fresh);
                            self.fresh = Some((prepared, waiters));
                        }
                        Err(error) => {
                            let error = if self.fresh_applying {
                                self.fresh_applying = false;
                                self.gate = Arc::default();
                                format!(
                                    "outcome unknown: fresh pull cleared the store before apply failed: {error}"
                                )
                            } else {
                                error
                            };
                            waiters.answer(&Err(error));
                        }
                    }
                }
                Completion::Pull(result) => {
                    let job = self.job.as_mut().expect("pull has an accepted job");
                    match result {
                        Ok((outcome, view)) => {
                            self.view = view;
                            self.mirrored = true;
                            job.state = JobState::Succeeded;
                            self.last_success =
                                Some(json!({"job_id": job.id, "name": outcome.name,
                                "version": outcome.version, "churn": outcome.churn}));
                            self.backoff = BACKOFF_START;
                            self.retry_at = now;
                            self.watch_retry_at = None;
                            // Publishing a pull must not postpone the
                            // next round beyond its last poll dispatch.
                            published = true;
                        }
                        Err(error) => {
                            let wait = pull_failure_wait(&error, &mut self.backoff, self.interval);
                            job.state = JobState::Failed;
                            job.error = Some(error.to_string());
                            self.retry_at = now + wait;
                            self.next_poll = self.retry_at;
                            self.watch_retry_at = Some(self.retry_at);
                        }
                    }
                    job.phase = None;
                    self.last_completed = Some(job.json());
                    if let Some(queued) = self.queued.take() {
                        self.job = Some(queued);
                    }
                }
            }
        }
        // CLI writes go first: their callers are waiting on them.
        self.start_write();
        self.start_if_ready_at(now);
        if watch && self.expedited_poll {
            self.poll_at(self.interval, now);
        }
        // Fairness with queued images: a watch poll the scheduler could not
        // dispatch because images held the worker, or a due watch retry
        // (which the scheduler skips while the worker is busy), gets the
        // worker before the next queued image request. Under `--no-watch`
        // there are no watch polls or retries to be fair to.
        if watch && !self.busy && self.poll_owed {
            self.poll_at(self.interval, now);
        }
        if watch
            && !self.busy
            && !self.image_queue.is_empty()
            && self.watch_retry_deadline().is_some_and(|due| due <= now)
        {
            self.retry_poll_at(self.interval, now);
        }
        if self.busy && !self.imaging && !self.polling {
            // A pull took the worker: queued image requests get the prompt
            // busy answer a new one would, instead of waiting behind it.
            // A (short) metadata poll keeps them queued.
            for job in std::mem::take(&mut self.image_queue) {
                (job.reply)(Err(self.busy_message()));
            }
        } else if !self.busy
            && self.reset.is_none()
            && !self.draining
            && let Some(job) = self.image_queue.pop_front()
        {
            self.start_images(job);
        }
        published
    }

    /// Accept one CLI write (NWS-2963); `reply` receives its result once it
    /// committed and was published, or failed (validation failures touch
    /// no store). Writes run on the worker in arrival order, ahead of
    /// queued watch/sync pulls, and — like a direct `figmog pull` — do not
    /// wait out a failed pull's backoff. An identical plain pull that has
    /// not started yet is shared only when it is the last queued write:
    /// joining an earlier one would reorder this pull ahead of whatever was
    /// queued after it (an import, a saved-file pull, a fresh or
    /// differently configured pull), so its result would not reflect the
    /// store that write left behind. Nothing else is merged.
    pub(crate) fn write(&mut self, operation: String, op: WriteOp, reply: WriteReply) {
        if self.draining {
            reply(Err(WRITE_NOT_STARTED.to_string()));
            return;
        }
        if let WriteOp::Pull {
            source: PullSource::Network,
            fresh: false,
            geometry,
        } = &op
            && let Some(queued) = self.writes.back_mut()
            && queued.op.is_plain_pull(*geometry)
        {
            queued.waiters.replies.push(reply);
            return;
        }
        self.writes.push_back(PendingWrite {
            op,
            waiters: Waiters {
                operation,
                replies: vec![reply],
            },
        });
        self.start_write();
    }

    /// Whether a CLI write is queued, running or waiting for its reset —
    /// a not-yet-mirrored store then queues no implicit first pull.
    pub(crate) fn is_busy(&self) -> bool {
        self.busy || self.reset.is_some()
    }

    pub(crate) fn has_writes(&self) -> bool {
        !self.writes.is_empty() || self.writing.is_some() || self.fresh.is_some()
    }

    fn start_write(&mut self) {
        if self.busy || self.reset.is_some() {
            return;
        }
        let Some(PendingWrite { op, waiters }) = self.writes.pop_front() else {
            return;
        };
        self.run_write(op.into_work(), waiters);
    }

    fn run_write(&mut self, work: WriteWork, waiters: Waiters) {
        match self.send(Command::Write(work)) {
            Ok(()) => {
                self.busy = true;
                self.writing = Some(waiters);
            }
            Err(_) => waiters.answer(&Err(WORKER_GONE.to_string())),
        }
    }

    /// The writes a reset carries over to the reopened session.
    fn take_carry(&mut self) -> Carry {
        Carry {
            fresh: self.fresh.take(),
            writes: std::mem::take(&mut self.writes),
            draining: self.draining,
            job: self.job.take(),
            queued: self.queued.take(),
            next_job: self.next_job,
            poll_owed: self.poll_owed,
            backoff: self.backoff,
            retry_at: self.retry_at,
            next_poll: self.next_poll,
            poll_retry_at: self.poll_retry_at,
            watch_retry_at: self.watch_retry_at,
            expedited_poll: self.expedited_poll,
            interval: self.interval,
        }
    }

    /// Continue carried writes on this (reopened) session: the prepared
    /// fresh pull first, then the queue.
    fn adopt(&mut self, carry: Carry) {
        self.writes = carry.writes;
        self.draining = carry.draining;
        self.job = carry.job;
        self.queued = carry.queued;
        self.next_job = carry.next_job;
        self.poll_owed = carry.poll_owed;
        self.backoff = carry.backoff;
        self.retry_at = carry.retry_at;
        self.next_poll = carry.next_poll;
        self.poll_retry_at = carry.poll_retry_at;
        self.watch_retry_at = carry.watch_retry_at;
        self.expedited_poll = carry.expedited_poll;
        self.interval = carry.interval;
        match carry.fresh {
            Some((prepared, waiters)) => {
                self.fresh_applying = true;
                self.gate.close();
                self.run_write(WriteWork::Apply(prepared), waiters);
            }
            None => self.start_write(),
        }
    }

    /// Desktop context cannot identify a mirror authoritatively. Its
    /// write requests one coalesced observation per known session.
    pub(crate) fn expedite_poll(&mut self, interval: Duration) {
        self.interval = interval;
        self.expedited_poll = true;
    }

    /// Retry work is independent of the ordinary round, but unavailable
    /// workers and explicitly queued pulls must keep their serialization.
    pub(crate) fn watch_retry_deadline(&self) -> Option<Instant> {
        if self.busy
            || self
                .job
                .as_ref()
                .is_some_and(|job| job.state == JobState::Queued)
        {
            return None;
        }
        self.watch_retry_at
            .into_iter()
            .chain(self.poll_retry_at)
            .max()
            .map(|retry| retry.max(self.retry_at).max(self.next_poll))
    }

    /// Extra retry work must not push an already scheduled normal visit
    /// past its eligibility boundary. The scheduler still rate-limits it.
    pub(crate) fn retry_poll_at(&mut self, interval: Duration, now: Instant) -> bool {
        if self.poll_at(interval, now) {
            self.next_poll = now;
            return true;
        }
        false
    }

    pub(crate) fn poll_at(&mut self, interval: Duration, now: Instant) -> bool {
        self.interval = interval;
        let eligible = (self.expedited_poll || now >= self.next_poll)
            && now >= self.retry_at
            && self.poll_retry_at.is_none_or(|retry| now >= retry)
            && self
                .job
                .as_ref()
                .is_none_or(|j| j.state != JobState::Queued);
        if eligible && self.busy && self.imaging {
            // Owed: dispatched by `maintain_at` as soon as the running
            // image request finishes, ahead of any queued one.
            self.poll_owed = true;
        }
        if !self.busy && self.reset.is_none() && !self.draining && eligible {
            let expedited = std::mem::take(&mut self.expedited_poll);
            if self.send(Command::Poll { expedited }).is_err() {
                return false;
            }
            self.poll_owed = false;
            self.next_poll = now + interval;
            self.poll_retry_at = None;
            self.watch_retry_at = None;
            self.busy = true;
            self.polling = true;
            return true;
        }
        false
    }

    /// Shutdown's drain (NWS-2963): answer every not-yet-started image
    /// request and CLI write with an error instead of running it — a write
    /// with [`WRITE_NOT_STARTED`], which its requester may safely retry —
    /// and start no new pull or poll from then on, even one a completed
    /// poll's `Changed` asks for (the barrier is enforced where pulls and
    /// polls are dispatched). Work already running (a write, its fresh
    /// pull's reset and apply, a pull, an image request) still finishes and
    /// publishes as [`Self::maintain`] processes its completion.
    pub(crate) fn begin_drain(&mut self) {
        self.draining = true;
        self.abandon_queued_images();
        let not_started = Err(WRITE_NOT_STARTED.to_string());
        for write in std::mem::take(&mut self.writes) {
            write.waiters.answer(&not_started);
        }
    }

    /// Shutdown: answer every not-yet-started image request with an error
    /// instead of running it. A running one still finishes.
    fn abandon_queued_images(&mut self) {
        for job in std::mem::take(&mut self.image_queue) {
            (job.reply)(Err(
                "figmog serve is shutting down; retry figmog_images".into()
            ));
        }
    }

    fn busy_message(&self) -> String {
        format!(
            "mirror {} has a pull or metadata poll in progress; retry figmog_images after figmog_status reports it complete",
            self.key
        )
    }

    /// Accept one image request; `reply` receives its result later, from
    /// the worker. Never waits on the worker: an idle worker starts it
    /// now, an image request already running queues it behind (FIFO, up to
    /// [`MAX_QUEUED_IMAGES`] queued jobs), and a
    /// running pull or metadata poll refuses it with a prompt busy error
    /// (spec §6), even for a cache hit. A watch poll or retry that comes due
    /// while images run gets the worker before the next queued image; a
    /// pull that takes the worker answers every still queued image request
    /// with that same busy error. Cache writes stay on the worker, the
    /// file's only writer.
    pub(crate) fn images(
        &mut self,
        ids: &[String],
        format: &str,
        scale: Option<f64>,
        renders: bool,
        policy: crate::images::RenderPolicy,
        reply: ImageReply,
    ) -> Result<(), String> {
        if !self.mirrored {
            return Err(format!(
                "mirror {} is not ready; inspect figmog_status",
                self.key
            ));
        }
        if self.busy && !self.imaging {
            return Err(self.busy_message());
        }
        if self.reset.is_some() {
            return Err(format!("mirror {}: {RESETTING}", self.key));
        }
        if self.image_queue.len() >= MAX_QUEUED_IMAGES {
            return Err(format!(
                "mirror {} image queue is full (limit {MAX_QUEUED_IMAGES}); retry figmog_images later",
                self.key
            ));
        }
        let job = ImageJob {
            ids: ids.to_vec(),
            format: format.to_string(),
            scale,
            renders,
            policy,
            reply,
        };
        if self.busy {
            self.image_queue.push_back(job);
        } else {
            self.start_images(job);
        }
        Ok(())
    }

    fn start_images(&mut self, job: ImageJob) {
        match self.send(Command::Images(job)) {
            Ok(()) => {
                self.busy = true;
                self.imaging = true;
            }
            Err(Command::Images(job)) => (job.reply)(Err(WORKER_GONE.to_string())),
            Err(_) => unreachable!("send hands back the command it was given"),
        }
    }
}

/// Where serve's sessions keep their stores (spec §16).
#[derive(Debug, Clone)]
pub(crate) enum StoreRoot {
    /// Every store at `<root>/<key>/db` (`--figmog-root`, `--db` serves,
    /// tests); `current` stays the cwd-relative `.figmog/current`.
    Fixed(PathBuf),
    /// The discovered project: the active-store rules of
    /// [`Layout::active_store`] pick each key's store.
    Project(Layout),
}

impl From<PathBuf> for StoreRoot {
    fn from(root: PathBuf) -> Self {
        StoreRoot::Fixed(root)
    }
}

impl StoreRoot {
    /// The store `key` opens. An old per-checkout mirror is used where it
    /// is (with a stderr note); two copies are an error.
    pub(crate) fn store_path(&self, key: &str) -> Result<PathBuf, String> {
        self.store_target(key).map(|(path, _)| path)
    }

    /// [`Self::store_path`] plus, for an old per-checkout store, what
    /// opening it must re-check ([`crate::cli::open_store_resolved`]).
    fn store_target(
        &self,
        key: &str,
    ) -> Result<(PathBuf, Option<crate::project::LegacyStore>), String> {
        match self {
            StoreRoot::Fixed(root) => Ok((root.join(key).join("db"), None)),
            StoreRoot::Project(layout) => {
                let store = layout.active_store(key)?;
                if store.migration_pending {
                    eprintln!("{}", crate::project::migration_pending_warning(key, &store));
                }
                let legacy = layout.legacy_store(key, &store.path);
                Ok((store.path, legacy))
            }
        }
    }

    /// The directory holding the `current` pointer.
    pub(crate) fn local_root(&self) -> PathBuf {
        match self {
            StoreRoot::Fixed(_) => PathBuf::from(crate::project::LOCAL_DIR),
            StoreRoot::Project(layout) => layout.local_root.clone(),
        }
    }
}

#[cfg(test)]
pub(crate) fn open_session(
    root: &StoreRoot,
    key: &str,
    api_token: Option<&str>,
) -> Result<FileSession, String> {
    let (path, legacy) = root.store_target(key)?;
    let api =
        api_token.map(|token| Arc::new(UreqApi::for_sessions(token.to_string())) as SessionApi);
    open_session_inner(
        path,
        legacy,
        key.to_string(),
        Some(key),
        api,
        WorkerHooks::default(),
        false,
        true,
    )
}

pub(crate) fn open_session_at(
    path: PathBuf,
    key: String,
    network_key: Option<&str>,
    api_token: Option<&str>,
) -> Result<FileSession, String> {
    let api =
        api_token.map(|token| Arc::new(UreqApi::for_sessions(token.to_string())) as SessionApi);
    open_session_with_api(path, key, network_key, api)
}

/// Injection at the REST boundary keeps real flattening, indexing and
/// snapshot publication exercised by synthetic tests.
#[cfg(test)]
type PhaseHook = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct WorkerHooks {
    #[cfg(test)]
    pub(crate) on_phase: Option<PhaseHook>,
}

impl WorkerHooks {
    fn phase(&self, _phase: &str) {
        #[cfg(test)]
        if let Some(hook) = &self.on_phase {
            hook(_phase);
        }
    }
}

pub(crate) fn open_session_with_api(
    path: PathBuf,
    key: String,
    network_key: Option<&str>,
    api: Option<SessionApi>,
) -> Result<FileSession, String> {
    open_session_with_hooks(path, key, network_key, api, WorkerHooks::default())
}

pub(crate) fn open_session_with_hooks(
    path: PathBuf,
    key: String,
    network_key: Option<&str>,
    api: Option<SessionApi>,
    hooks: WorkerHooks,
) -> Result<FileSession, String> {
    open_session_inner(path, None, key, network_key, api, hooks, false, true)
}

/// Open (with `fresh`: clear, then open) one session's store and start its
/// worker.
#[allow(clippy::too_many_arguments)]
fn open_session_inner(
    path: PathBuf,
    legacy: Option<crate::project::LegacyStore>,
    key: String,
    network_key: Option<&str>,
    api: Option<SessionApi>,
    hooks: WorkerHooks,
    fresh: bool,
    create: bool,
) -> Result<FileSession, String> {
    let reopen: Reopen = {
        let (path, key, network_key) = (path.clone(), key.clone(), network_key.map(str::to_string));
        let (legacy, api, hooks) = (legacy.clone(), api.clone(), hooks.clone());
        Arc::new(move |fresh| {
            open_session_inner(
                path.clone(),
                legacy.clone(),
                key.clone(),
                network_key.as_deref(),
                api.clone(),
                hooks.clone(),
                fresh,
                create,
            )
        })
    };
    let network_key = network_key.map(str::to_string);
    let pipeline = Arc::new(Mutex::new(crate::figmog_pipeline!()));
    let open = |path: &std::path::Path| {
        fold::stream::KeyedStream::<Id, crate::model::Rec, _>::new(
            path,
            SharedPipeline(pipeline.clone()),
        )
    };
    let mut st = if fresh {
        crate::cli::open_store_cleared(&path, legacy.as_ref(), open)?
    } else if create {
        crate::cli::open_store_resolved(&path, legacy.as_ref(), open)?
    } else {
        crate::cli::open_store_existing_resolved(&path, legacy.as_ref(), open)?
    };
    let make_view = move |snapshot, node_count| {
        Arc::new(SessionView::new(
            snapshot,
            |snapshot| pipeline.lock().unwrap().reader(snapshot),
            node_count,
        ))
    };
    let view = make_view(st.rtx(|(snapshot, _)| snapshot.clone()), None);
    let mirrored = view
        .with_dependent(|_, (_, _, _, _, _, _, meta, _, _, _, _, _, _, _)| meta.get(&0).is_some());
    let geometry =
        view.with_dependent(|_, (.., config, _, _, _, _, _)| store::read_geometry(config));
    let (commands, work) = mpsc::sync_channel(1);
    let (done, completions) = mpsc::channel();
    let opened = view.clone();
    let session_path = path.clone();
    let worker = std::thread::spawn(move || {
        // Count the opened mirror's nodes here, once, so the first status
        // poll doesn't pay for it (NWS-2942). A read racing this waits for
        // the same count rather than starting its own. Kept current so a
        // view published by a write that leaves `nodes` alone (a variable
        // import) carries it too.
        let mut node_count = opened.node_count();
        drop(opened);
        let mut watcher = Watcher::new(None);
        while let Ok(command) = work.recv() {
            // Agent/watch pulls (`Command::Pull`, a lifecycle job) and CLI
            // writes (`Command::Write`) share one apply path below.
            let (work, job) = match command {
                Command::Poll { expedited } => {
                    let tick = match (&api, &network_key) {
                        (Some(api), Some(key)) => watcher.tick(&**api, key),
                        _ => Tick::Wait {
                            after: Duration::from_secs(300),
                        },
                    };
                    if done.send(Completion::Poll { tick, expedited }).is_err() {
                        break;
                    }
                    continue;
                }
                Command::Images(ImageJob {
                    ids,
                    format,
                    scale,
                    renders,
                    policy,
                    reply,
                }) => {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let node_ids = crate::images::normalize_ids(&ids);
                        let render_ids: &[String] = if renders { &node_ids } else { &[] };
                        let scanned = st.rtx(|(_, readers)| {
                            crate::with_scan_readers!(readers, |r| crate::images::scan_split(
                                &r,
                                render_ids,
                                &node_ids,
                                &format,
                                crate::images::scale_milli(scale),
                                policy,
                            ))
                        });
                        crate::images::resolve(
                            &mut st,
                            scanned,
                            api.as_deref().map(|a| a as &dyn FigmaApi),
                            &crate::api::download_bytes,
                            network_key.as_deref(),
                            render_ids,
                            &format,
                            scale,
                        )
                    }))
                    .map_err(|_| {
                        "image worker panicked; retry figmog_images and inspect server stderr"
                            .to_string()
                    });
                    // Free the worker before answering, so a follow-up
                    // request is never refused by a stale busy flag.
                    let stop = done.send(Completion::Images).is_err();
                    reply(result);
                    if stop {
                        break;
                    }
                    continue;
                }
                Command::Pull { geometry } => (
                    WriteWork::Pull {
                        source: PullSource::Network,
                        fresh: false,
                        geometry,
                    },
                    true,
                ),
                Command::Write(work) => (work, false),
            };
            let network_write = !job
                && matches!(
                    &work,
                    WriteWork::Pull {
                        source: PullSource::Network,
                        ..
                    }
                );
            // Lifecycle phases belong to jobs only.
            let phase = |name: PullPhase| {
                if job {
                    let _ = done.send(Completion::Phase(name));
                }
                hooks.phase(name.as_str());
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<WriteResult, PullError> {
                    let prepared = match work {
                        WriteWork::Import { content, label, allow_empty, dry_run } => {
                            // Validated in full before any write, exactly
                            // like the direct `import-variables`.
                            let invalid = |e: String| {
                                PullError::from(format!("invalid variables file {label}: {e}"))
                            };
                            let v: Value = serde_json::from_slice(&content)
                                .map_err(|e| invalid(e.to_string()))?;
                            crate::vars::check_import_entries(&v)
                                .map_err(|e| invalid(e.to_string()))?;
                            let parsed = crate::vars::parse_variables_snapshot(
                                &v,
                                crate::vars::SOURCE_FILE,
                                crate::vars::snapshot_digest(&content),
                            )
                            .map_err(|e| invalid(e.to_string()))?;
                            let prior = st.rtx(|(_, (_, _, _, _, vars, collections, ..))| {
                                collect_variable_ids(&vars, &collections)
                            });
                            let imported = parsed.recs.iter().filter(|(id, _)| matches!(id, Id::Variable(_))).count();
                            let live: BTreeSet<_> = parsed.recs.iter().map(|(id, _)| id).collect();
                            let removed_ids: Vec<_> = prior.iter().filter_map(|id| match id {
                                Id::Variable(id_string) if !live.contains(id) => Some(id_string.clone()),
                                _ => None,
                            }).collect();
                            if !dry_run && imported == 0 && !allow_empty && prior.iter().any(|id| matches!(id, Id::Variable(_))) {
                                return Err(invalid("empty export would remove stored variables; pass --allow-empty to replace them".to_string()));
                            }
                            if !dry_run {
                                store::replace_variable_snapshot(&mut st, &prior, &parsed.recs, parsed.snapshot);
                            }
                            let value = if dry_run {
                                json!({"imported": imported, "removed": removed_ids.len(), "removed_ids": removed_ids})
                            } else { json!({"imported": imported, "removed": removed_ids.len()}) };
                            let (mirrored, geometry) =
                                st.rtx(|(_, (_, _, _, _, _, _, meta, _, config, ..))| {
                                    (meta.get(&0).is_some(), store::read_geometry(&config))
                                },
                            );
                            let view = make_view(
                                st.rtx(|(snapshot, _)| snapshot.clone()),
                                Some(node_count),
                            );
                            return Ok(WriteResult::Applied(Applied {
                                value,
                                view,
                                mirrored,
                                geometry,
                                outcome: None,
                            }));
                        }
                        WriteWork::Apply(prepared) => prepared,
                        WriteWork::Pull {
                            source,
                            fresh,
                            geometry,
                        } => {
                            // A fresh pull ignores the stored config (the
                            // documented way to turn geometry back off).
                            let stored = !fresh
                                && st.rtx(|(_, (.., config, _, _, _, _, _))| {
                                    store::read_geometry(&config)
                                });
                            let geometry = store::effective_geometry(geometry, stored);
                            let (response, vars, network) = match source {
                                PullSource::Network => {
                                    let key = network_key.as_deref().ok_or_else(|| {
                                        PullError::from(
                                            "no file key: pass a file key or figma.com URL"
                                                .to_string(),
                                        )
                                    })?;
                                    let api = api.as_ref().ok_or_else(|| {
                                        PullError::from(crate::auth::missing_token("network pulls"))
                                    })?;
                                    phase(PullPhase::Fetching);
                                    let response = crate::deep_json::OwnedValue(api.file(key, geometry)?);
                                    let vars = api.variables_local(key)?;
                                    (response, vars, true)
                                }
                                PullSource::File(response) => (response, None, false),
                            };
                            phase(PullPhase::Flattening);
                            let mut flattened = flatten_file(&response)
                                .map_err(|e| PullError::from(e.to_string()))?;
                            if let Some(vars) = &vars {
                                flattened.recs.extend(
                                    crate::vars::enterprise_snapshot_recs(vars)
                                        .map_err(|e| PullError::from(e.to_string()))?,
                                );
                            }
                            let prepared = Prepared {
                                flattened,
                                has_vars: vars.is_some(),
                                geometry,
                                network,
                            };
                            if fresh {
                                // Nothing touched yet: the loop resets the
                                // store behind its barrier, then applies.
                                return Ok(WriteResult::Prepared(prepared));
                            }
                            prepared
                        }
                    };
                    let Prepared {
                        flattened,
                        has_vars,
                        geometry,
                        network,
                    } = prepared;
                    phase(PullPhase::Applying);
                    let mut prior: BTreeSet<Id> =
                        st.rtx(|(_, ((nodes, ..), components, sets, styles, ..))| {
                            collect_sweepable(&nodes, &components, &sets, &styles)
                        });
                    if has_vars {
                        prior.extend(st.rtx(|(_, (_, _, _, _, vars, collections, ..))| {
                            collect_variable_ids(&vars, &collections)
                        }));
                    }
                    let prior_meta =
                        st.rtx(|(_, (_, _, _, _, _, _, meta, ..))| meta.get(&0));
                    let version = flattened.file.version.clone();
                    let version_changed = prior_meta.as_ref().map(|meta| meta.version.as_str())
                        != Some(version.as_str());
                    // Publish file state and invalidate images atomically,
                    // matching direct pulls even if the worker is interrupted.
                    let churn = store::sync_with_cache_eviction(
                        &mut st,
                        &prior,
                        &flattened,
                        now_ms(),
                        |tx, churn| {
                            let snapshot_changed = prior_meta.as_ref().is_some_and(|meta| {
                                meta.last_modified != flattened.file.last_modified
                            }) || churn.added > 0
                                || churn.changed > 0
                                || churn.removed > 0;
                            if !version_changed && !snapshot_changed {
                                return Vec::new();
                            }
                            tx.rtx(|(_, (_, _, _, _, _, _, _, cache, _, images, pins, ..))| {
                                if version_changed {
                                    let mut ids = store::stale_cache_ids(&cache, &version);
                                    ids.extend(store::stale_image_ids(
                                        &images, &pins, &version,
                                    ));
                                    ids
                                } else {
                                    store::all_image_ids(&images, &pins)
                                }
                            })
                        },
                    );
                    // The final flattened node count belongs to the same
                    // committed publication, without a second table scan.
                    node_count = store::flattened_node_count(&flattened);
                    // Sticky geometry (spec §4): this pull's flag OR what
                    // the store already holds (nothing, after a fresh reset).
                    let stored_now =
                        st.rtx(|(_, (.., config, _, _, _, _, _))| store::read_geometry(&config));
                    let geometry = store::effective_geometry(geometry, stored_now);
                    store::upsert_mirror_config(&mut st, geometry);
                    phase(PullPhase::Publishing);
                    let view =
                        make_view(st.rtx(|(snapshot, _)| snapshot.clone()), Some(node_count));
                    if network {
                        watcher.note_pull_success();
                    }
                    Ok(WriteResult::Applied(Applied {
                        value: serde_json::to_value(&churn).expect("churn serializes"),
                        view,
                        mirrored: true,
                        geometry,
                        outcome: Some(PullOutcome {
                            churn,
                            name: flattened.file.name,
                            version,
                        }),
                    }))
                },
            ))
            .unwrap_or_else(|_| {
                Err(PullError::from(
                    "pull worker panicked; retry the pull and inspect server stderr".to_string(),
                ))
            });
            let completion = if job {
                Completion::Pull(result.map(|result| match result {
                    WriteResult::Applied(Applied {
                        view,
                        outcome: Some(outcome),
                        ..
                    }) => (outcome, view),
                    _ => unreachable!("a lifecycle pull applies a network pull"),
                }))
            } else {
                Completion::Write(result.map_err(|e| if network_write { format!("{e}; this routed pull uses credentials read by the running figmog serve at startup; restart it after auth login or use --no-socket with FIGMA_TOKEN") } else { e.to_string() }))
            };
            if done.send(completion).is_err() {
                break;
            }
        }
        // Closed with ownership kept: shutdown compacts the store once
        // every view is gone (NWS-2941); a reset just releases it.
        st.close_for_compaction()
    });
    Ok(FileSession {
        key,
        path: session_path,
        fresh_applying: false,
        mirrored,
        view,
        commands: Some(commands),
        completions,
        worker: Some(worker),
        busy: false,
        polling: false,
        imaging: false,
        image_queue: VecDeque::new(),
        poll_owed: false,
        job: None,
        queued: None,
        last_completed: None,
        last_success: None,
        next_job: 1,
        geometry,
        backoff: BACKOFF_START,
        retry_at: Instant::now(),
        next_poll: Instant::now(),
        poll_retry_at: None,
        watch_retry_at: None,
        expedited_poll: false,
        interval: Duration::from_secs(10),
        gate: Arc::default(),
        reset: None,
        reopen,
        writes: VecDeque::new(),
        writing: None,
        fresh: None,
        draining: false,
    })
}

/// Sessions remain in open order; startup selection is independent of it.
pub(crate) struct SessionManager {
    pub(crate) sessions: Vec<FileSession>,
    pub(crate) root: StoreRoot,
    pub(crate) token: Option<String>,
    pub(crate) default_key: Option<String>,
    /// Whether the one open session is the default when `default_key` is
    /// `None` — an in-process serve's rule. Never the shared background
    /// owner's (NWS-2953): its sessions are every worktree's files, so a
    /// request without a file must not inherit another worktree's.
    pub(crate) sole_default: bool,
    #[cfg(test)]
    pub(crate) api: Option<SessionApi>,
}

pub(crate) const NO_DEFAULT_FILE_MSG: &str = "no file specified and no default mirrored file — pass a `file` argument, mirror one with figmog_open, or see figmog_files for the current list";

/// Agent references may start an initial mirror; CLI reads only admit
/// stores that already exist.
#[derive(Clone, Copy)]
pub(crate) enum SessionResolution {
    ExistingOnly,
    AutoOpen,
}

impl SessionManager {
    pub(crate) fn open(&mut self, file_ref: &str) -> Result<&mut FileSession, String> {
        self.open_mode(file_ref, true)
    }

    fn open_mode(&mut self, file_ref: &str, create: bool) -> Result<&mut FileSession, String> {
        let key = parse_file_ref(file_ref)
            .ok_or_else(|| format!("not a Figma file key or URL: {file_ref}"))?;
        if let Some(pos) = self.sessions.iter().position(|s| s.key == key) {
            return Ok(&mut self.sessions[pos]);
        }
        let (path, legacy) = self.root.store_target(&key)?;
        #[cfg(test)]
        let injected = self.api.clone();
        #[cfg(not(test))]
        let injected: Option<SessionApi> = None;
        let api = injected.or_else(|| {
            self.token
                .as_ref()
                .map(|token| Arc::new(UreqApi::for_sessions(token.clone())) as SessionApi)
        });
        let session = open_session_inner(
            path,
            legacy,
            key.clone(),
            Some(&key),
            api,
            WorkerHooks::default(),
            false,
            create,
        )?;
        self.sessions.push(session);
        Ok(self.sessions.last_mut().unwrap())
    }

    pub(crate) fn effective_default_key(&self) -> Option<String> {
        self.default_key.clone().or_else(|| {
            if self.sole_default && self.sessions.len() == 1 {
                Some(self.sessions[0].key.clone())
            } else {
                None
            }
        })
    }

    pub(crate) fn resolve(
        &mut self,
        file: Option<&str>,
        interval: Duration,
        resolution: SessionResolution,
    ) -> Result<&mut FileSession, String> {
        let key = file
            .map(|file| {
                parse_file_ref(file).ok_or_else(|| format!("not a Figma file key or URL: {file}"))
            })
            .transpose()?
            .or_else(|| self.effective_default_key())
            .ok_or_else(|| NO_DEFAULT_FILE_MSG.to_string())?;
        let session = if file.is_some() {
            match resolution {
                SessionResolution::AutoOpen => self.open(&key)?,
                SessionResolution::ExistingOnly => {
                    if !self.sessions.iter().any(|session| session.key == key)
                        && !self.root.store_path(&key)?.is_dir()
                    {
                        return Err(format!(
                            "no mirror here for {key}; run figmog pull or figmog_open first"
                        ));
                    }
                    self.open_mode(&key, false)?
                }
            }
        } else {
            self.sessions
                .iter_mut()
                .find(|s| s.key == key)
                .ok_or_else(|| NO_DEFAULT_FILE_MSG.to_string())?
        };
        if !session.mirrored
            && session.job.is_none()
            && !session.has_writes()
            && !session.has_variable_state()
        {
            session.request_pull(false, interval);
        }
        Ok(session)
    }

    pub(crate) fn expedite_polls(&mut self, interval: Duration) {
        for session in &mut self.sessions {
            session.expedite_poll(interval);
        }
    }

    #[cfg(test)]
    pub(crate) fn list(&self) -> Value {
        self.files_read().run()
    }

    /// Capture `figmog_files` for a read worker: each session's lifecycle
    /// now, and a lease on its published view (none while it resets, which
    /// lists that file's metadata as null).
    pub(crate) fn files_read(&self) -> FilesRead {
        let default = self.effective_default_key();
        FilesRead(
            self.sessions
                .iter()
                .map(|s| {
                    (
                        s.key.clone(),
                        default.as_deref() == Some(&s.key),
                        s.status_read(),
                    )
                })
                .collect(),
        )
    }

    /// Reset barrier steps 1–2 (NWS-2962): validate, then stop admitting
    /// reads for `key`'s store only. Steps 3–5 run in
    /// [`service_resets`](Self::service_resets) once its worker reads have
    /// released their views and its worker is idle.
    /// (A fresh pull closes its gate the same way from `maintain_at`, once
    /// its worker has validated it.)
    #[cfg_attr(not(test), allow(dead_code))] // no tool resets without a write
    pub(crate) fn begin_reset(&mut self, file_ref: &str, reset: StoreReset) -> Result<(), String> {
        let key = parse_file_ref(file_ref)
            .ok_or_else(|| format!("not a Figma file key or URL: {file_ref}"))?;
        let session = self
            .sessions
            .iter_mut()
            .find(|s| s.key == key)
            .ok_or_else(|| format!("no open mirror {key}; see figmog_files"))?;
        if session.reset.is_some() {
            return Err(format!("mirror {key} is already being reset"));
        }
        session.gate.close();
        session.reset = Some(reset);
        Ok(())
    }

    /// Reset barrier steps 3–5, without blocking: every resetting store
    /// whose admitted reads have all released their views and whose worker
    /// is idle is closed, reset and reopened, and the new session (with an
    /// open gate) published in the same position. Returns each finished
    /// reset's key and outcome; a store that fails to reopen is dropped
    /// from the session list (a later reference opens it again).
    pub(crate) fn service_resets(&mut self) -> Vec<(String, Result<(), String>)> {
        let mut finished = Vec::new();
        let mut index = 0;
        while index < self.sessions.len() {
            let session = &self.sessions[index];
            let ready = session.reset.is_some() && !session.busy && session.gate.outstanding() == 0;
            if !ready {
                index += 1;
                continue;
            }
            let key = session.key.clone();
            let reopen = session.reopen.clone();
            let fresh = session.reset == Some(StoreReset::Fresh);
            // Closing the old session joins its idle worker and releases
            // the store before it is reopened. Its CLI writes carry over.
            let mut closed = self.sessions.remove(index);
            let carry = closed.take_carry();
            drop(closed);
            match reopen(fresh) {
                Ok(mut session) => {
                    session.adopt(carry);
                    self.sessions.insert(index, session);
                    index += 1;
                    finished.push((key, Ok(())));
                }
                Err(error) => {
                    carry.fail(&if fresh { format!("outcome unknown: fresh pull may have cleared the store before reopen failed: {error}") } else { error.clone() });
                    finished.push((key, Err(error)));
                }
            }
        }
        finished
    }

    /// Shutdown's last step, after [`close_reads`](Self::close_reads):
    /// close every session, then compact each store (NWS-2941) while its
    /// ownership is still held. A store a detached read still holds a view
    /// of fails fjall's own lock check and is left as it is.
    pub(crate) fn close_and_compact(&mut self, deadline: Instant) {
        let closed: Vec<_> = self
            .sessions
            .drain(..)
            .filter_map(|session| session.close_until(deadline))
            .collect();
        for compaction in closed {
            if Instant::now() >= deadline {
                break;
            }
            let worker = std::thread::spawn(move || compaction.run());
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }

    /// Shutdown's close barrier: stop admitting reads on every store and
    /// wait (until `deadline`) for admitted ones to release their views
    /// before the sessions — and their stores — are dropped.
    pub(crate) fn close_reads(&mut self, deadline: Instant) {
        for session in &self.sessions {
            session.gate.close();
        }
        for session in &self.sessions {
            if !session.gate.wait_drained(deadline) {
                // A detached read still owns its SessionView. Fold's
                // TableReaders retain the fjall Keyspace/LockedFileGuard,
                // so releasing OwnedStore's .figmog-lock cannot let another
                // open (and thus a reset) succeed until that view drops.
                // Waiting here would defeat the bounded shutdown deadline.
                debug_assert!(session.gate.is_closed());
            }
        }
    }
}
