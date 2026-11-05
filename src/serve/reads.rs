//! Heavy local reads on worker threads (NWS-2962).
//!
//! A read that scans the whole mirror (stats, a large subtree, search …)
//! must not hold the serve loop: it runs on a small pool of read workers
//! against a leased, immutable published [`SessionView`](crate::session_view::SessionView),
//! needing no store lock, and answers through the deferred-response
//! mechanism (NWS-2955). The worker also frames and serializes the
//! response, so even a 20 MB answer costs the loop nothing but a queue
//! push. Cheap point lookups stay inline on the loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use serde_json::{Value, json};

use crate::mcp::{PendingCall, ToolOutput};
use crate::session_view::ReadLease;
use crate::sessions::{self, FilesRead, StatusRead};

use super::DeferredReply;

/// Local reads answered inline on the loop: point lookups whose cost is
/// bounded by one node and its direct children (measured at well under a
/// millisecond on a 100k-node mirror), plus the component list. Every
/// other local read — and `figmog_status`/`figmog_files`, whose node count
/// scans the mirror — runs on a read worker.
const INLINE_READS: &[&str] = &[
    "figmog_pages",
    "figmog_node",
    "figmog_path",
    "figmog_components",
];

pub(crate) fn is_inline_read(name: &str) -> bool {
    INLINE_READS.contains(&name)
}

/// Read workers: `min(4, available_parallelism)`.
fn pool_size() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 4)
}

/// What a read worker computes.
pub(crate) enum ReadTask {
    /// One local read tool on a leased view. `explicit_key`/`url_key` feed
    /// the file-disagreement note on a failed call (spec §2b).
    Tool {
        lease: ReadLease,
        name: String,
        args: Value,
        explicit_key: Option<String>,
        url_key: Option<String>,
    },
    Status {
        read: StatusRead,
        upstream: &'static str,
    },
    Files(FilesRead),
}

impl ReadTask {
    /// Runs the read; every lease is released when this returns.
    fn run(self) -> Result<ToolOutput, String> {
        match self {
            ReadTask::Tool {
                lease,
                name,
                args,
                explicit_key,
                url_key,
            } => sessions::read_tool(lease.view(), &name, &args).map_err(|msg| {
                super::file_mismatch_note(msg, explicit_key.as_deref(), url_key.as_deref())
            }),
            ReadTask::Status { read, upstream } => {
                let mut status = read.run()?;
                status["upstream"] = json!(upstream);
                Ok(ToolOutput::Json(status))
            }
            ReadTask::Files(read) => Ok(ToolOutput::Json(read.run())),
        }
    }

    #[cfg(test)]
    fn name(&self) -> &str {
        match self {
            ReadTask::Tool { name, .. } => name,
            ReadTask::Status { .. } => "figmog_status",
            ReadTask::Files(_) => "figmog_files",
        }
    }
}

/// A read accepted by the handler, waiting for its [`PendingCall`] (known
/// only once `dispatch_message` returns) before it is submitted.
pub(crate) struct StagedRead {
    pub(crate) token: u64,
    pub(crate) task: ReadTask,
    pub(crate) reply: DeferredReply,
    /// The requester's cancel flag: a read for a closed or dropped
    /// connection is skipped, not computed.
    pub(crate) cancel: Arc<AtomicBool>,
}

struct ReadJob {
    staged: StagedRead,
    call: PendingCall,
}

#[cfg(test)]
pub(crate) type ReadHook = Arc<dyn Fn(&str) + Send + Sync>;

/// The read worker pool, spawned on first use. Jobs run in arrival order.
/// Dropping it skips queued jobs (each answers with an error) and joins the
/// workers after their running read finishes, releasing every lease.
#[derive(Default)]
pub(crate) struct ReadPool {
    jobs: Option<mpsc::Sender<ReadJob>>,
    workers: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// Test hook run on the worker, lease held, before each read.
    #[cfg(test)]
    pub(crate) before_read: Option<ReadHook>,
}

impl ReadPool {
    pub(crate) fn submit(&mut self, staged: StagedRead, call: PendingCall) {
        if self.jobs.is_none() {
            self.spawn();
        }
        if let Some(jobs) = &self.jobs {
            // A dead pool drops the job; its reply answers with an error.
            let _ = jobs.send(ReadJob { staged, call });
        }
    }

    fn spawn(&mut self) {
        let (jobs, queue) = mpsc::channel::<ReadJob>();
        let queue = Arc::new(Mutex::new(queue));
        for n in 0..pool_size() {
            let queue = queue.clone();
            let stop = self.stop.clone();
            #[cfg(test)]
            let hook = self.before_read.clone();
            let worker = std::thread::Builder::new()
                .name(format!("figmog-read-{n}"))
                .spawn(move || {
                    loop {
                        let job = queue.lock().unwrap().recv();
                        let Ok(ReadJob { staged, call }) = job else {
                            return;
                        };
                        if stop.load(Ordering::Relaxed) {
                            continue;
                        }
                        let StagedRead {
                            task,
                            reply,
                            cancel,
                            ..
                        } = staged;
                        if cancel.load(Ordering::Relaxed) {
                            reply.send(
                                Err("request cancelled: its connection closed".into()),
                                false,
                            );
                            continue;
                        }
                        #[cfg(test)]
                        if let Some(hook) = &hook {
                            hook(task.name());
                        }
                        let output =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task.run()))
                                .unwrap_or_else(|_| {
                                    Err("read worker panicked; retry and inspect server stderr"
                                        .into())
                                });
                        let mut frame = call.finish(output).to_string();
                        frame.push('\n');
                        reply.send_frame(frame);
                    }
                });
            match worker {
                Ok(worker) => self.workers.push(worker),
                Err(error) => eprintln!("figmog: could not start a read worker: {error}"),
            }
        }
        if !self.workers.is_empty() {
            self.jobs = Some(jobs);
        }
    }
}

impl Drop for ReadPool {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.jobs.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
