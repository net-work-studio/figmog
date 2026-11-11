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
use std::time::{Duration, Instant};

use serde_json::json;

use crate::deep_json::OwnedValue;
use crate::mcp::{PendingCall, ToolOutput};
use crate::session_view::ReadLease;
use crate::sessions::{self, FilesRead, StatusRead};

use super::DeferredReply;

/// Only page-list and ancestor-path lookups stay inline. Even a point
/// `figmog_node` lookup may contain arbitrarily large raw geometry or a
/// wide direct-child list, so every node response runs on a read worker.
const INLINE_READS: &[&str] = &["figmog_pages", "figmog_path"];

/// Global queued-read bound, independent of connection/requester count.
pub(crate) const MAX_QUEUED_READS: usize = 128;

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
        args: OwnedValue,
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
/// Stopping it ([`ReadPool::shutdown`] or drop) skips queued jobs (each
/// answers with an error); a running read finishes and releases its lease
/// on its own thread.
#[derive(Default)]
pub(crate) struct ReadPool {
    jobs: Option<mpsc::SyncSender<ReadJob>>,
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
            match jobs.try_send(ReadJob { staged, call }) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(job)) => job.staged.reply.send(
                    Err("read workers are busy: too many queued reads; retry later".into()),
                    false,
                ),
                // A dead pool drops the job; its reply answers with an error.
                Err(mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
    }

    fn spawn(&mut self) {
        let (jobs, queue) = mpsc::sync_channel::<ReadJob>(MAX_QUEUED_READS);
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
                        let response = OwnedValue(call.finish(output));
                        let mut frame = crate::deep_json::to_string(&response.0).unwrap();
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

impl ReadPool {
    /// Shutdown: skip queued reads and wait until `deadline` for running
    /// ones. A read still running then is detached, not awaited: its thread
    /// keeps its own lease (and so its `Arc`'d view and store snapshot)
    /// alive until it finishes or the process exits. Nothing here closes,
    /// resets or reopens a store, so no store is touched under that view.
    pub(crate) fn shutdown(&mut self, deadline: Instant) {
        self.stop.store(true, Ordering::Relaxed);
        self.jobs.take();
        for worker in self.workers.drain(..) {
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
            // Otherwise dropping the handle detaches the worker.
        }
    }
}

impl Drop for ReadPool {
    /// Never blocks: queued reads are skipped and running ones detached
    /// (each finishes on its own, then its worker exits).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.jobs.take();
        self.workers.clear();
    }
}
