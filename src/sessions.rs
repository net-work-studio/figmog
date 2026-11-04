//! Per-file background workers and routing for multi-file serve.
//!
//! Each worker owns the only writer. Dispatch reads a separately pinned
//! committed view, published only after fetch, flatten, sync and eviction
//! finish. Workers also own watch watermarks, so a metadata observation
//! cannot acknowledge a fetch that preceded it.

use std::collections::BTreeSet;
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
use crate::session_view::{SessionView, SharedPipeline};
use crate::store::{self, Churn, collect_sweepable, collect_variable_ids};
use crate::watch::{BACKOFF_START, Tick, Watcher};

type SessionApi = Arc<dyn FigmaApi + Send + Sync>;

enum Command {
    Pull {
        geometry: bool,
    },
    Poll {
        expedited: bool,
    },
    Images {
        ids: Vec<String>,
        format: String,
        scale: Option<f64>,
        reply: mpsc::SyncSender<Result<Vec<crate::images::ImageItem>, String>>,
    },
}

enum Completion {
    Pull(Result<(PullOutcome, Arc<SessionView>), PullError>),
    Phase(PullPhase),
    Poll { tick: Tick, expedited: bool },
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
/// its pipeline. Only image operations (outside the pull scheduling scope)
/// synchronously await their worker reply.
pub(crate) struct FileSession {
    pub(crate) key: String,
    pub(crate) mirrored: bool,
    view: Arc<SessionView>,
    commands: Option<mpsc::SyncSender<Command>>,
    completions: mpsc::Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
    busy: bool,
    polling: bool,
    job: Option<Job>,
    queued: Option<Job>,
    last_completed: Option<Value>,
    last_success: Option<Value>,
    next_job: u64,
    geometry: bool,
    backoff: Duration,
    retry_at: Instant,
    next_poll: Instant,
    poll_retry_at: Instant,
    expedited_poll: bool,
    interval: Duration,
}

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

    pub(crate) fn status(&self) -> Result<Value, String> {
        let mut value = if self.mirrored {
            match self.dispatch("figmog_status", &json!({}))? {
                ToolOutput::Json(value) => value,
                ToolOutput::Raw(_) => unreachable!("local read tools return JSON"),
            }
        } else {
            json!({"name": null, "version": null, "last_modified": null,
                "synced_at_unix_ms": null, "nodes": 0})
        };
        value["ready"] = json!(self.mirrored);
        value["pull"] = self.lifecycle();
        Ok(value)
    }

    pub(crate) fn dispatch(&self, name: &str, args: &Value) -> Result<ToolOutput, String> {
        if !self.mirrored && name != "figmog_status" {
            return Err(format!(
                "mirror {} is not ready; inspect figmog_status or figmog_files for pull progress and errors",
                self.key
            ));
        }
        self.view.with_dependent(|_, readers| {
            dispatch::dispatch_read_tool(name, args, "", readers)
                .ok_or_else(|| format!("unknown tool: {name}"))?
                .map(ToolOutput::Json)
        })
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
        if self.busy || now < self.retry_at {
            return;
        }
        if let Some(job) = &mut self.job
            && job.state == JobState::Queued
        {
            self.commands
                .as_ref()
                .unwrap()
                .send(Command::Pull {
                    geometry: job.geometry,
                })
                .unwrap();
            job.state = JobState::Running;
            self.busy = true;
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
                            self.poll_retry_at = now + after;
                            self.next_poll = self.poll_retry_at;
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
        published
    }

    /// Desktop context cannot identify a mirror authoritatively. Its
    /// write requests one coalesced observation per known session.
    pub(crate) fn expedite_poll(&mut self, interval: Duration) {
        self.interval = interval;
        self.expedited_poll = true;
    }

    pub(crate) fn poll_at(&mut self, interval: Duration, now: Instant) {
        self.interval = interval;
        if !self.busy
            && (self.expedited_poll || now >= self.next_poll)
            && now >= self.retry_at
            && now >= self.poll_retry_at
            && self
                .job
                .as_ref()
                .is_none_or(|j| j.state != JobState::Queued)
        {
            let expedited = std::mem::take(&mut self.expedited_poll);
            self.commands
                .as_ref()
                .unwrap()
                .send(Command::Poll { expedited })
                .unwrap();
            self.next_poll = now + interval;
            self.busy = true;
            self.polling = true;
        }
    }

    pub(crate) fn images(
        &mut self,
        ids: &[String],
        format: &str,
        scale: Option<f64>,
    ) -> Result<Vec<crate::images::ImageItem>, String> {
        if !self.mirrored {
            return Err(format!(
                "mirror {} is not ready; inspect figmog_status",
                self.key
            ));
        }
        // Never wait behind a pull/poll, even for an image cache hit.
        // Cache-miss networking keeps its existing synchronous contract
        // when the worker is idle, independently of pull scheduling.
        if self.busy {
            return Err(format!(
                "mirror {} has a pull or metadata poll in progress; retry figmog_images after figmog_status reports it complete",
                self.key
            ));
        }
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .unwrap()
            .send(Command::Images {
                ids: ids.to_vec(),
                format: format.to_string(),
                scale,
                reply,
            })
            .map_err(|e| e.to_string())?;
        result.recv().map_err(|e| e.to_string())?
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

#[derive(Default)]
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
        view.with_dependent(|_, (_, _, _, _, _, _, meta, _, _, _)| meta.get(&0).is_some());
    let geometry = view.with_dependent(|_, (.., config, _)| store::read_geometry(config));
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
                        let stored = st.rtx(|(_, (.., config, _))| store::read_geometry(&config));
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
                                crate::vars::parse_variables_export(&vars)
                                    .map_err(|e| PullError::from(e.to_string()))?,
                            );
                            prior.extend(st.rtx(|(_, (_, _, _, _, vars, collections, ..))| {
                                collect_variable_ids(&vars, &collections)
                            }));
                        }
                        phase(PullPhase::Applying);
                        let prior_version = st.rtx(|(_, (_, _, _, _, _, _, meta, _, _, _))| {
                            meta.get(&0).map(|meta| meta.version.clone())
                        });
                        let churn = store::sync(&mut st, &prior, &flattened, now_ms());
                        let version = flattened.file.version.clone();
                        // Match direct pulls: unchanged versions avoid decoding
                        // potentially large image blobs merely to evict none.
                        if prior_version.as_deref() != Some(version.as_str()) {
                        let mut stale = st.rtx(|(_, (_, _, _, _, _, _, _, cache, _, images))| {
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
                Command::Images {
                    ids,
                    format,
                    scale,
                    reply,
                } => {
                    let result = (|| {
                        let key = network_key.as_deref().ok_or_else(|| {
                            "no file key: pass a file key or figma.com URL".to_string()
                        })?;
                        let node_ids = crate::images::normalize_ids(&ids);
                        let scanned =
                            st.rtx(|(_, ((nodes, ..), _, _, _, _, _, meta, _, _, images))| {
                                crate::images::scan(
                                    &nodes,
                                    &meta,
                                    &images,
                                    &node_ids,
                                    &format,
                                    crate::images::scale_milli(scale),
                                )
                            });
                        Ok(crate::images::resolve(
                            &mut st,
                            scanned,
                            api.as_deref().map(|a| a as &dyn FigmaApi),
                            &crate::api::download_bytes,
                            Some(key),
                            &node_ids,
                            &format,
                            scale,
                        ))
                    })();
                    let _ = reply.send(result);
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
        job: None,
        queued: None,
        last_completed: None,
        last_success: None,
        next_job: 1,
        geometry,
        backoff: BACKOFF_START,
        retry_at: Instant::now(),
        next_poll: Instant::now(),
        poll_retry_at: Instant::now(),
        expedited_poll: false,
        interval: Duration::from_secs(10),
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

    pub(crate) fn list(&self) -> Value {
        let default = self.effective_default_key();
        json!(self.sessions.iter().map(|s| {
            let status = s.status().unwrap_or(Value::Null);
            json!({"key": s.key, "name": status["name"], "version": status["version"], "nodes": status["nodes"],
                "last_synced": status["synced_at_unix_ms"], "default": default.as_deref() == Some(&s.key),
                "ready": s.mirrored, "pull": s.lifecycle()})
        }).collect::<Vec<_>>())
    }
}
