//! Per-file background workers and routing for multi-file serve.
//!
//! Each worker owns the only writer. Dispatch reads a separately pinned
//! committed view, published only after fetch, flatten, sync and eviction
//! finish. Workers also own watch watermarks, so a metadata observation
//! cannot acknowledge a fetch that preceded it.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fold::pipeline::Push;
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{FigmaApi, UreqApi};
use crate::cli::{PullError, now_ms, open_store_checked, pull_failure_wait};
use crate::dispatch;
use crate::flatten::flatten_file;
use crate::ident::parse_file_ref;
use crate::mcp::ToolOutput;
use crate::model::Id;
use crate::session_view::{ReadGate, ReadLease, SessionView, SharedPipeline};
use crate::store::{self, Churn, collect_sweepable, collect_variable_ids};
use crate::watch::{BACKOFF_START, Tick, Watcher};

type SessionApi = Arc<dyn FigmaApi + Send + Sync>;

const WORKER_GONE: &str =
    "this file's worker stopped unexpectedly; inspect server stderr and restart figmog serve";

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
    reply: ImageReply,
}

enum Command {
    Pull { geometry: bool },
    Poll { expedited: bool },
    Images(ImageJob),
}

enum Completion {
    Pull(Result<(PullOutcome, Arc<SessionView>), PullError>),
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
    view: Arc<SessionView>,
    commands: Option<mpsc::SyncSender<Command>>,
    completions: mpsc::Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
    busy: bool,
    polling: bool,
    /// The worker is busy with an image request (not a pull or poll).
    imaging: bool,
    /// Image requests accepted while another one runs, in arrival order.
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
    /// and hooks — the reset barrier's step 4.
    reopen: Reopen,
}

type Reopen = Arc<dyn Fn() -> Result<FileSession, String> + Send + Sync>;

/// A store-level operation that must not run underneath live views; it
/// waits behind the per-store reset barrier ([`ReadGate`]). Append-only:
/// NWS-2963 adds its own variants (e.g. a fresh re-pull) here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))] // first constructed by NWS-2963's reset tool
pub(crate) enum StoreReset {
    /// Close the store and reopen it from disk, publishing a new session
    /// (fresh job history, like a reopened mirror).
    Reopen,
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
    view.with_dependent(|_, readers| {
        dispatch::dispatch_read_tool(name, args, "", readers)
            .ok_or_else(|| format!("unknown tool: {name}"))?
            .map(ToolOutput::Json)
    })
}

/// A session's status JSON. `view` is required for a mirrored session; a
/// missing one (its store is resetting) is an error.
fn status_value(
    mirrored: bool,
    lifecycle: Value,
    view: Option<&SessionView>,
) -> Result<Value, String> {
    let mut value = match (mirrored, view) {
        (true, Some(view)) => match read_tool(view, "figmog_status", &json!({}))? {
            ToolOutput::Json(value) => value,
            ToolOutput::Raw(_) | ToolOutput::Deferred(_) => {
                unreachable!("local read tools return JSON")
            }
        },
        (true, None) => return Err(RESETTING.to_string()),
        (false, _) => json!({"name": null, "version": null, "last_modified": null,
            "synced_at_unix_ms": null, "nodes": 0}),
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

    /// Capture a status read for a read worker. A mirrored session whose
    /// store is resetting captures no view; its status read then fails.
    pub(crate) fn status_read(&self) -> StatusRead {
        StatusRead {
            mirrored: self.mirrored,
            lifecycle: self.lifecycle(),
            lease: self.mirrored.then(|| self.gate.admit(&self.view)).flatten(),
        }
    }

    /// The not-ready and resetting refusals every data read shares.
    pub(crate) fn check_readable(&self) -> Result<(), String> {
        if !self.mirrored {
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
    pub(crate) fn lease(&self) -> Result<ReadLease, String> {
        self.check_readable()?;
        self.gate
            .admit(&self.view)
            .ok_or_else(|| format!("mirror {}: {RESETTING}", self.key))
    }

    pub(crate) fn dispatch(&self, name: &str, args: &Value) -> Result<ToolOutput, String> {
        self.check_readable()?;
        read_tool(&self.view, name, args)
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

    fn start_if_ready_at(&mut self, now: Instant) {
        if self.busy || self.reset.is_some() || now < self.retry_at {
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
    /// overwrite a newer publication.
    pub(crate) fn maintain(&mut self) -> bool {
        self.maintain_at(Instant::now())
    }

    pub(crate) fn maintain_at(&mut self, now: Instant) -> bool {
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
        self.start_if_ready_at(now);
        if self.expedited_poll {
            self.poll_at(self.interval, now);
        }
        // Fairness with queued images: a watch poll the scheduler could not
        // dispatch because images held the worker, or a due watch retry
        // (which the scheduler skips while the worker is busy), gets the
        // worker before the next queued image request.
        if !self.busy && self.poll_owed {
            self.poll_at(self.interval, now);
        }
        if !self.busy
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
            && let Some(job) = self.image_queue.pop_front()
        {
            self.start_images(job);
        }
        published
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
        if !self.busy && self.reset.is_none() && eligible {
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

    /// Shutdown: answer every not-yet-started image request with an error
    /// instead of running it. A running one still finishes.
    pub(crate) fn abandon_queued_images(&mut self) {
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
    /// now, an image request already running queues it behind (FIFO), and a
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
        let job = ImageJob {
            ids: ids.to_vec(),
            format: format.to_string(),
            scale,
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

pub(crate) fn open_session(
    root: &Path,
    key: &str,
    api_token: Option<&str>,
) -> Result<FileSession, String> {
    open_session_at(
        root.join(key).join("db"),
        key.to_string(),
        Some(key),
        api_token,
    )
}

pub(crate) fn open_session_at(
    path: PathBuf,
    key: String,
    network_key: Option<&str>,
    api_token: Option<&str>,
) -> Result<FileSession, String> {
    let api = api_token.map(|token| Arc::new(UreqApi::new(token.to_string())) as SessionApi);
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
    let reopen: Reopen = {
        let (path, key, network_key) = (path.clone(), key.clone(), network_key.map(str::to_string));
        let (api, hooks) = (api.clone(), hooks.clone());
        Arc::new(move || {
            open_session_with_hooks(
                path.clone(),
                key.clone(),
                network_key.as_deref(),
                api.clone(),
                hooks.clone(),
            )
        })
    };
    let network_key = network_key.map(str::to_string);
    let pipeline = Arc::new(Mutex::new(crate::figmog_pipeline!()));
    let mut st = open_store_checked(&path, |path| {
        fold::stream::KeyedStream::<Id, crate::model::Rec, _>::new(
            path,
            SharedPipeline(pipeline.clone()),
        )
    })?;
    let make_view = move |snapshot| {
        Arc::new(SessionView::new(snapshot, |snapshot| {
            pipeline.lock().unwrap().reader(snapshot)
        }))
    };
    let view = make_view(st.rtx(|(snapshot, _)| snapshot.clone()));
    let mirrored =
        view.with_dependent(|_, (_, _, _, _, _, _, meta, _, _, _, _, _)| meta.get(&0).is_some());
    let geometry = view.with_dependent(|_, (.., config, _, _, _)| store::read_geometry(config));
    let (commands, work) = mpsc::sync_channel(1);
    let (done, completions) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut watcher = Watcher::new(None);
        while let Ok(command) = work.recv() {
            match command {
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
                }
                Command::Pull { geometry } => {
                    let phase = |name| {
                        let _ = done.send(Completion::Phase(name));
                        hooks.phase(name.as_str());
                    };
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let key = network_key.as_deref().ok_or_else(|| {
                            PullError::from(
                                "no file key: pass a file key or figma.com URL".to_string(),
                            )
                        })?;
                        let api = api.as_ref().ok_or_else(|| {
                            PullError::from(crate::auth::missing_token("network pulls"))
                        })?;
                        let stored =
                            st.rtx(|(_, (.., config, _, _, _))| store::read_geometry(&config));
                        let geometry = store::effective_geometry(geometry, stored);
                        phase(PullPhase::Fetching);
                        let response = api.file(key, geometry)?;
                        let vars = api.variables_local(key)?;
                        phase(PullPhase::Flattening);
                        let mut flattened =
                            flatten_file(&response).map_err(|e| PullError::from(e.to_string()))?;
                        let mut prior: BTreeSet<Id> =
                            st.rtx(|(_, ((nodes, ..), components, sets, styles, ..))| {
                                collect_sweepable(&nodes, &components, &sets, &styles)
                            });
                        if let Some(vars) = vars {
                            flattened.recs.extend(
                                crate::vars::enterprise_snapshot_recs(&vars)
                                    .map_err(|e| PullError::from(e.to_string()))?,
                            );
                            prior.extend(st.rtx(|(_, (_, _, _, _, vars, collections, ..))| {
                                collect_variable_ids(&vars, &collections)
                            }));
                        }
                        phase(PullPhase::Applying);
                        let prior_version =
                            st.rtx(|(_, (_, _, _, _, _, _, meta, _, _, _, _, _))| {
                                meta.get(&0).map(|meta| meta.version.clone())
                            });
                        let churn = store::sync(&mut st, &prior, &flattened, now_ms());
                        let version = flattened.file.version.clone();
                        // Match direct pulls: unchanged versions avoid decoding
                        // potentially large image blobs merely to evict none.
                        if prior_version.as_deref() != Some(version.as_str()) {
                            let mut stale =
                                st.rtx(|(_, (_, _, _, _, _, _, _, cache, _, images, _, _))| {
                                    let mut ids = store::stale_cache_ids(&cache, &version);
                                    ids.extend(store::stale_image_ids(&images, &version));
                                    ids
                                });
                            if !stale.is_empty() {
                                stale.sort();
                                store::evict_stale_cache(&mut st, &stale);
                            }
                        }
                        store::upsert_mirror_config(&mut st, geometry);
                        phase(PullPhase::Publishing);
                        let view = make_view(st.rtx(|(snapshot, _)| snapshot.clone()));
                        watcher.note_pull_success();
                        Ok((
                            PullOutcome {
                                churn,
                                name: flattened.file.name,
                                version,
                            },
                            view,
                        ))
                    }))
                    .unwrap_or_else(|_| {
                        Err(PullError::from(
                            "pull worker panicked; retry the pull and inspect server stderr"
                                .to_string(),
                        ))
                    });
                    if done.send(Completion::Pull(result)).is_err() {
                        break;
                    }
                }
                Command::Images(ImageJob {
                    ids,
                    format,
                    scale,
                    reply,
                }) => {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let node_ids = crate::images::normalize_ids(&ids);
                        let scanned = st.rtx(
                            |(_, ((nodes, ..), _, _, _, _, _, meta, _, _, images, pins, _))| {
                                crate::images::scan(
                                    &nodes,
                                    &meta,
                                    &images,
                                    &pins,
                                    &node_ids,
                                    &format,
                                    crate::images::scale_milli(scale),
                                )
                            },
                        );
                        crate::images::resolve(
                            &mut st,
                            scanned,
                            api.as_deref().map(|a| a as &dyn FigmaApi),
                            &crate::api::download_bytes,
                            network_key.as_deref(),
                            &node_ids,
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
                }
            }
        }
    });
    Ok(FileSession {
        key,
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
    })
}

/// Sessions remain in open order; startup selection is independent of it.
pub(crate) struct SessionManager {
    pub(crate) sessions: Vec<FileSession>,
    pub(crate) root: PathBuf,
    pub(crate) token: Option<String>,
    pub(crate) default_key: Option<String>,
    #[cfg(test)]
    pub(crate) api: Option<SessionApi>,
}

const NO_DEFAULT_FILE_MSG: &str = "no file specified and no default mirrored file — pass a `file` argument, mirror one with figmog_open, or see figmog_files for the current list";

impl SessionManager {
    pub(crate) fn open(&mut self, file_ref: &str) -> Result<&mut FileSession, String> {
        let key = parse_file_ref(file_ref)
            .ok_or_else(|| format!("not a Figma file key or URL: {file_ref}"))?;
        if let Some(pos) = self.sessions.iter().position(|s| s.key == key) {
            return Ok(&mut self.sessions[pos]);
        }
        #[cfg(test)]
        let session = match &self.api {
            Some(api) => open_session_with_api(
                self.root.join(&key).join("db"),
                key.clone(),
                Some(&key),
                Some(api.clone()),
            )?,
            None => open_session(&self.root, &key, self.token.as_deref())?,
        };
        #[cfg(not(test))]
        let session = open_session(&self.root, &key, self.token.as_deref())?;
        self.sessions.push(session);
        Ok(self.sessions.last_mut().unwrap())
    }

    pub(crate) fn effective_default_key(&self) -> Option<String> {
        self.default_key.clone().or_else(|| {
            if self.sessions.len() == 1 {
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
    ) -> Result<&mut FileSession, String> {
        let key = file
            .map(str::to_string)
            .or_else(|| self.effective_default_key())
            .ok_or_else(|| NO_DEFAULT_FILE_MSG.to_string())?;
        let session = if file.is_some() {
            self.open(&key)?
        } else {
            self.sessions
                .iter_mut()
                .find(|s| s.key == key)
                .ok_or_else(|| NO_DEFAULT_FILE_MSG.to_string())?
        };
        if !session.mirrored && session.job.is_none() {
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
    #[cfg_attr(not(test), allow(dead_code))] // first caller: NWS-2963's reset tool
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
            // Closing the old session joins its idle worker and releases
            // the store before it is reopened.
            drop(self.sessions.remove(index));
            match reopen() {
                Ok(session) => {
                    self.sessions.insert(index, session);
                    index += 1;
                    finished.push((key, Ok(())));
                }
                Err(error) => finished.push((key, Err(error))),
            }
        }
        finished
    }

    /// Shutdown's close barrier: stop admitting reads on every store and
    /// wait (until `deadline`) for admitted ones to release their views
    /// before the sessions — and their stores — are dropped.
    pub(crate) fn close_reads(&mut self, deadline: Instant) {
        for session in &self.sessions {
            session.gate.close();
        }
        for session in &self.sessions {
            session.gate.wait_drained(deadline);
        }
    }
}
