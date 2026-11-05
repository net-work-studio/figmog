//! `figmog serve` — an MCP stdio server with the sync loop built in.
//!
//! One process owns every mirrored file's store (build design §11,
//! extended by spec §14's multi-file serve): a reader thread turns stdin
//! lines into an `mpsc` channel; the main loop owns a [`sessions::SessionManager`]
//! — one [`sessions::FileSession`] per mirrored file — and answers JSON-RPC
//! requests between poll ticks. Every local `figmog_*` tool is a thin
//! wrapper over the same `query::*` functions the CLI prints — one source
//! of truth for every answer.
//!
//! Every local tool accepts file routing (explicit file, node URL, then
//! startup default or the sole session). First references queue a pull;
//! open/sync acknowledge a job immediately, and status/files expose its
//! progress. Each file's worker owns its writer and metadata polling.
//! Requests use its last published committed snapshot while the worker
//! fetches, flattens, applies indexes and evicts caches.
//! `figmog_images` and desktop proxy calls are answered later: their
//! network work runs on the file's worker or the desktop worker, and the
//! loop writes each response to its requester when it completes (NWS-2955).
//! Heavy local reads run on a read-worker pool against a leased published
//! view, and every response is written by a per-requester writer thread
//! with a bounded queue and a per-response deadline, so neither a slow
//! client nor a whole-mirror scan holds the loop (NWS-2962).
//!
//! **Desktop context:** the upstream answers for the file active in Figma,
//! independently of the mirrors. It supplies no authoritative file/revision
//! context, so proxied calls always forward uncached, even with an explicit
//! node id or `file` argument. A mirror's version cannot validate a response
//! from an unrelated desktop file (see [`proxy::proxy_call`]).
//!
//! **v3 (build design §12):** unless `--no-upstream`, figmog also probes
//! Figma's native desktop MCP server at startup and becomes the *only*
//! Figma MCP an agent needs — `tools/list` merges the 21 local `figmog_*`
//! tools with every upstream tool verbatim (`proxy::merge_registry`), and
//! `tools/call` routes by the namespace rule (`proxy::is_local_tool`).
//! Upstream routing is global, not per-session: the desktop server serves
//! whatever file is open in the Figma app, independent of any mirror this
//! process manages — spec §14's documented caveat. The desktop is
//! re-detected after startup (NWS-2959): the desktop worker keeps probing
//! it, and when its tools appear or disappear the registry is re-merged
//! and clients that sent `initialize` get `notifications/tools/list_changed`
//! (see `desktop.rs`).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::dispatch;
use crate::ident::parse_file_ref;
#[cfg(test)]
use crate::mcp::FnHandler;
use crate::mcp::{self, PendingCall, Reply, ToolErrorMode, ToolOutput};
use crate::proxy;
use crate::sessions::{self, SessionManager, StoreRoot};

mod desktop;
mod output;
mod reads;

use crate::upstream::{HttpUpstream, UpstreamMcp};
use desktop::{DesktopJob, DesktopWorker, ProbeCadence};
use output::{ConnRegistry, StdioWriter};
use reads::{ReadPool, ReadTask, StagedRead};

/// Default streamable-HTTP URL of Figma desktop app's Dev Mode MCP server
/// (build design §12).
pub const DEFAULT_UPSTREAM_URL: &str = "http://127.0.0.1:3845/mcp";

/// Floor for the round-robin watch tick's per-session deadline (spec §14:
/// `max(interval / session_count, 2s)`).
const MIN_TICK_DEADLINE: Duration = Duration::from_secs(2);

/// Ceiling for `--interval`, in seconds (one day) — generous for any real
/// poll cadence, but bounded: this module repeatedly adds `interval` to an
/// `Instant::now()` (directly, and via [`tick_deadline`]), and an
/// unclamped user-supplied `u64` (a typo, or an intentionally huge value)
/// risks overflowing `Instant`'s internal representation and panicking the
/// first time it's added, rather than failing gracefully.
const MAX_INTERVAL_SECS: u64 = 86_400;

/// Clamp a raw `--interval` (seconds) to [`MAX_INTERVAL_SECS`] before it's
/// ever added to an `Instant`.
fn clamp_interval(interval: u64) -> Duration {
    Duration::from_secs(interval.min(MAX_INTERVAL_SECS))
}

// ---- unix-socket control plane (v0.0.2 spec §1) ----
//
// `figmog serve` additionally listens on `<figmog-root>/serve.sock`: a
// listener thread accepts connections, and each connection's own reader
// thread forwards newline-delimited JSON-RPC frames into the *same* `mpsc`
// channel the stdin reader already feeds — tagged with a connection id
// (`Incoming::Socket`) so the single-threaded main loop below can route
// each response back to the connection that asked for it. The main loop
// handles routing, scheduling and committed read views; per-file workers
// own all writes and blocking pull/poll work; read workers run heavy
// reads; writer threads (`output.rs`) write every response.

/// One message the serve loop can receive from either its stdin reader
/// thread or a socket connection's reader thread.
#[derive(Debug)]
enum Incoming {
    /// One line of MCP stdio input.
    Stdio(String),
    /// Stdin hit EOF (or a read error): the reader thread is done and the
    /// loop should exit. An explicit sentinel — rather than relying on
    /// every `tx` clone being dropped, the pre-socket exit signal — because
    /// once socket connections hold their own long-lived `tx` clones, the
    /// channel no longer reliably disconnects just because stdin closed.
    StdinClosed,
    /// One line from socket connection `conn_id`.
    Socket(u64, String),
}

/// `<figmog_root>/serve.sock` — must match `cli::socket`'s own derivation
/// exactly, since that's how the CLI finds this process.
pub(crate) fn socket_path(figmog_root: &Path) -> PathBuf {
    figmog_root.join("serve.sock")
}

/// Outcome of probing a pre-existing socket file at startup (spec §1).
#[derive(Debug, PartialEq, Eq)]
enum StaleProbe {
    /// A connect succeeded: some other process is actively listening — this
    /// instance must not touch the file.
    Owned,
    /// A connect failed with a valid address: nothing is actually listening (a leftover file
    /// from an unclean exit, or garbage that was never a socket at all) —
    /// safe to unlink and rebind.
    Stale,
}

/// Pure classification of a connect attempt against a pre-existing socket
/// path, split from the actual `UnixStream::connect` call so the decision
/// itself is unit-testable without a real socket. Spec §1 only names the
/// two outcomes that matter in practice — refused (stale) vs. success
/// (owned). Invalid addresses (including an overlong AF_UNIX address) do
/// not establish staleness and must leave the file untouched. Other connect
/// failures are treated as stale: a path that
/// exists but isn't a live listening socket at all (garbage left by
/// something else, or a filesystem error) means this instance can't reach
/// a live owner through it either way, so it's safe to reclaim.
fn classify_probe(connect_result: std::io::Result<()>) -> std::io::Result<StaleProbe> {
    match connect_result {
        Ok(()) => Ok(StaleProbe::Owned),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Err(error),
        Err(_) => Ok(StaleProbe::Stale),
    }
}

/// Generous but bounded: probing a pre-existing socket should resolve
/// near-instantly (either the connect succeeds against a live listener or
/// fails against a dead one) — this only guards against a pathological
/// hang on a socket that accepts but never proceeds.
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Bind the control-plane socket at `<figmog_root>/serve.sock`, handling
/// the startup stale-probe (spec §1): no file yet ⇒ bind directly; a file
/// present ⇒ probe it via [`classify_probe`], unlinking and rebinding on
/// [`StaleProbe::Stale`] or returning the clean "another serve owns this
/// root" error on [`StaleProbe::Owned`] (`run_serve` surfaces this as an
/// ordinary `Err`, which `cli::run`'s top-level handler turns into the
/// standard `{"error": ...}` exit-1 JSON — spec §1: "exit with a clean JSON
/// error"). Sets the socket file's mode to 0600 after binding, and
/// `figmog_root` itself to 0700 (review m4): macOS in particular ignores a
/// unix-domain socket *file's* mode for connect permission checks — the
/// containing directory's mode is the real access boundary there, so
/// tightening only the socket file would be closer to cosmetic than
/// enforced on that platform. `figmog_root` may already exist (a prior
/// session's store, or a re-bind) — the mode is set unconditionally either
/// way, since every path that reaches this point is this process's own
/// root to run as its owner sees fit.
fn bind_socket(figmog_root: &Path) -> Result<(UnixListener, PathBuf), String> {
    let figmog_root = crate::private_dir::ensure(figmog_root)?;

    let path = socket_path(&figmog_root);
    // AF_UNIX limits the supplied address string, not the resolved filesystem
    // path. When cwd is an ancestor of the verified root, it shares that
    // checked, non-replaceable chain and securely anchors this short address.
    // Keep the absolute path for chmod, unlink, and the cleanup guard.
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let address = match figmog_root.strip_prefix(&cwd) {
        Ok(relative_root) => socket_path(relative_root),
        Err(_) => path.clone(),
    };

    if path.exists() {
        let connect_result = UnixStream::connect(&address).and_then(|stream| {
            stream.set_read_timeout(Some(SOCKET_PROBE_TIMEOUT))?;
            Ok(())
        });
        match classify_probe(connect_result)
            .map_err(|e| format!("probing socket {}: {e}", address.display()))?
        {
            StaleProbe::Owned => {
                return Err("another figmog serve owns this root".to_string());
            }
            StaleProbe::Stale => {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("removing stale socket {}: {e}", path.display()))?;
            }
        }
    }

    let listener = UnixListener::bind(&address)
        .map_err(|e| format!("binding socket {}: {e}", address.display()))?;
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(&path, perms).map_err(|e| e.to_string())?;
    Ok((listener, path))
}

/// Unlinks the control-plane socket file on drop — but only if the file at
/// `path` is still the *same inode* this process bound (review m3): a
/// racer, or an operator manually `rm`-ing and recreating the path while
/// this process ran, could otherwise cause this guard to delete a live
/// socket that isn't this process's own. `(dev, ino)` is recorded right
/// after a successful bind and re-checked at drop time via a fresh
/// `stat` — cheap, and the only reliable way to answer "is this still my
/// file" on a plain path (there's no unlink-by-fd primitive for a named
/// unix socket). A `stat` failure at drop time (the path is already gone)
/// is treated as nothing-to-do, not an error.
///
/// Every one of `run_serve`'s return points — a clean stdin-EOF exit, a
/// watch-loop disconnect, a future early error after the socket was bound
/// — drops this implicitly (it's held in a local binding for the rest of
/// the function's scope), so "unlinked on clean exit" (spec §1) is
/// structural rather than a set of hand-maintained cleanup calls at each
/// return site. Never constructed for a probe that found another serve
/// owning the root: [`bind_socket`] returns `Err` before creating one in
/// that case, and that socket file is not this process's to remove.
///
/// Best-effort, not a hard guarantee: on filesystems that recycle inode
/// numbers aggressively (ext4, notably — see the two-live-files-then-
/// `rename(2)` construction the `socket_guard_does_not_unlink_a_file_
/// replaced_at_the_same_path` test needs to reliably prove the *positive*
/// case on Linux), an unlink immediately followed by an unrelated create at
/// the exact same path could in principle land on the same `(dev, ino)`
/// this guard bound, purely by chance — no attacker or racer required.
/// This is an accepted, narrow gap in an already best-effort safety net
/// (spec §1 has no auth story for the socket beyond filesystem
/// permissions to begin with), not a design this crate tries to close
/// further.
struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketGuard {
    fn new(path: PathBuf) -> Result<Self, String> {
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        Ok(SocketGuard {
            path,
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A single frame's hard ceiling (review m4): a client sending more than
/// this without a newline is misbehaving (or hostile) and is disconnected
/// rather than let grow this connection's read buffer without bound. Any
/// real figmog request/response — including a large `figmog_subtree` dump
/// — is orders of magnitude smaller than this.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Read one newline-delimited frame from `reader`, capped at
/// [`MAX_FRAME_BYTES`] (review m4). `Ok(None)` on a clean EOF with no
/// partial data; `Err` on an oversized frame (no newline found within the
/// cap) or any underlying I/O error — both mean "disconnect this
/// connection", exactly like every other read failure in
/// [`spawn_socket_acceptor`].
fn read_bounded_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    // Driven directly off `fill_buf`/`consume` rather than `Take` wrapping
    // `read_until`: a chunk can (and, with a client that pipelines several
    // frames back-to-back, will) contain bytes past the newline — consuming
    // only up through it, never the whole chunk, is what leaves the rest
    // buffered for the *next* call to pick up as the start of the next
    // frame, instead of dropping it.
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break; // clean EOF
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            break;
        }
        buf.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if buf.len() > MAX_FRAME_BYTES {
            return Err(std::io::Error::other(format!(
                "frame exceeds {MAX_FRAME_BYTES} bytes with no newline"
            )));
        }
    }
    if buf.is_empty() {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Accept loop for the control-plane socket, on its own thread: each
/// connection gets its own reader thread forwarding newline-delimited
/// frames (bounded per [`read_bounded_line`]) into `tx` tagged with a
/// fresh connection id, with its write half registered (and its writer
/// thread started, [`output::Connections::register`]) first — so a request
/// that arrives and is answered before the *next* line is even read still
/// has somewhere to route its response. An accept, clone, or registration
/// failure for one connection is skipped rather than tearing down the
/// whole listener — a transient per-connection hiccup shouldn't take the
/// control plane down.
fn spawn_socket_acceptor(
    listener: UnixListener,
    tx: mpsc::Sender<Incoming>,
    registry: ConnRegistry,
) {
    std::thread::spawn(move || {
        let next_id = AtomicU64::new(0);
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok(write_half) = stream.try_clone() else {
                continue;
            };
            let conn_id = next_id.fetch_add(1, Ordering::Relaxed);
            if registry.register(conn_id, write_half).is_err() {
                continue;
            }

            let tx = tx.clone();
            let registry = registry.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                loop {
                    match read_bounded_line(&mut reader) {
                        Ok(None) => break,
                        Ok(Some(l)) => {
                            if tx.send(Incoming::Socket(conn_id, l)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // The client disconnected, sent something unreadable, or
                // was dropped by its writer: a response computed after this
                // point finds no entry, and its pending reads are skipped.
                registry.unregister(conn_id);
            });
        }
    });
}

/// How long to wait before the next watch tick, given how many sessions
/// are being round-robin polled: the full `interval` split evenly across
/// them (so each file gets, on average, one Tier-3 poll per `interval`),
/// floored at [`MIN_TICK_DEADLINE`] so total poll spend stays bounded even
/// with many mirrored files. Zero sessions: idle at the full interval,
/// still floored at [`MIN_TICK_DEADLINE`] — `--interval 0` is accepted,
/// and an unfloored zero idle wait would hand the request loop's
/// `recv_timeout` a zero timeout forever, busy-looping an empty server
/// (NWS-2938).
fn tick_deadline(interval: Duration, session_count: usize) -> Duration {
    if session_count == 0 {
        return interval.max(MIN_TICK_DEADLINE);
    }
    (interval / session_count as u32).max(MIN_TICK_DEADLINE)
}

/// Run the MCP stdio server (plus, unless `no_socket`, the unix-socket
/// control plane — spec §1). `db_override` is the CLI's legacy `--db
/// <path>` escape hatch (pre-v4, single-session semantics preserved
/// exactly — see `cli::dispatch`'s note on this branch); when absent,
/// `files` (zero or more, `--figmog-root`-rooted) are each mirrored at
/// startup (pulled if their store is empty and `!no_watch`), the first
/// one becoming the default. Unless `no_upstream`, also attaches Figma's
/// native desktop MCP server at `upstream_url` as an uncached proxy (build
/// design §12); a failed probe degrades to local-only tools with one
/// stderr line, never a hard error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_serve(
    db_override: Option<PathBuf>,
    files: Vec<String>,
    interval: u64,
    no_watch: bool,
    upstream_url: String,
    no_upstream: bool,
    figmog_root: Option<PathBuf>,
    layout: Option<crate::project::Layout>,
    no_socket: bool,
) -> Result<(), String> {
    let interval_dur = clamp_interval(interval);
    let token = crate::auth::token()?;
    // `--figmog-root` pins both the socket and every store to one fixed
    // directory; otherwise the socket stays in the project's per-checkout
    // `.figmog` and stores follow the active-store rules (spec §16).
    // `--db` (no layout) keeps the cwd-relative `.figmog` socket.
    let socket_root = figmog_root
        .clone()
        .or_else(|| layout.as_ref().map(|l| l.local_root.clone()))
        .unwrap_or_else(|| PathBuf::from(crate::project::LOCAL_DIR));
    let store_root = match (figmog_root, layout) {
        (Some(root), _) => StoreRoot::Fixed(root),
        (None, Some(layout)) => StoreRoot::Project(layout),
        (None, None) => StoreRoot::Fixed(PathBuf::from(crate::project::LOCAL_DIR)),
    };

    // Bind the control-plane socket (spec §1) before anything else touches
    // the filesystem for this run: a stale-probe conflict ("another figmog
    // serve owns this root") should fail fast and unambiguously, never
    // masked by an unrelated fjall store-lock panic from a session this
    // process would otherwise go on to open first. `_socket_guard` is held
    // for the rest of this function purely for its `Drop` impl (unlinking
    // the socket file) — every return point below drops it implicitly.
    let (tx, rx) = mpsc::channel::<Incoming>();
    let registry: ConnRegistry = Arc::default();
    let _socket_guard: Option<SocketGuard> = if no_socket {
        None
    } else {
        let (listener, path) = bind_socket(&socket_root)?;
        let guard = SocketGuard::new(path)?;
        spawn_socket_acceptor(listener, tx.clone(), registry.clone());
        Some(guard)
    };

    // Reader thread: stdin lines -> mpsc, tagged `Incoming::Stdio` (see
    // `Incoming`'s doc comment for why EOF now sends an explicit
    // `StdinClosed` sentinel rather than relying on every `tx` clone being
    // dropped).
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(l) => {
                        if tx.send(Incoming::Stdio(l)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(Incoming::StdinClosed);
        });
    }

    let (manager, track_current) = build_sessions(
        db_override,
        &files,
        no_watch,
        &token,
        store_root,
        interval_dur,
    )?;

    // Upstream probe: once here, so the first `tools/list` already has the
    // desktop's tools when it is running; the desktop worker re-checks it
    // from then on (NWS-2959). Global, not per-session (see this module's
    // doc comment). `--no-upstream` starts no worker and never probes.
    let desktop: Option<DesktopWorker> = if no_upstream {
        None
    } else {
        let mut client = HttpUpstream::new(upstream_url);
        let connected = match client.initialize() {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "figmog: upstream unreachable, serving local tools only (re-checking every {}s): {e}",
                    ProbeCadence::default().missing.as_secs()
                );
                false
            }
        };
        Some(DesktopWorker::spawn_with(
            client,
            connected,
            ProbeCadence::default(),
        ))
    };
    let upstream_status = desktop.as_ref().map_or("disabled", DesktopWorker::status);

    eprintln!(
        "{} serving {} file(s) (watch {}, upstream {upstream_status}, socket {})",
        mcp::SERVER_NAME,
        manager.sessions.len(),
        if no_watch { "off" } else { "on" },
        if no_socket { "off" } else { "on" }
    );

    serve_loop(
        manager,
        desktop,
        no_watch,
        track_current,
        interval_dur,
        rx,
        registry,
    )
}

/// Dispatch requests using real stdio output. The loop accepts an output
/// adapter separately so tests can exercise stdio frames without sharing
/// the test process's stdout.
#[allow(clippy::too_many_arguments)]
fn serve_loop(
    manager: SessionManager,
    desktop: Option<DesktopWorker>,
    no_watch: bool,
    track_current: bool,
    interval_dur: Duration,
    rx: mpsc::Receiver<Incoming>,
    registry: ConnRegistry,
) -> Result<(), String> {
    let first_deadline = Instant::now() + tick_deadline(interval_dur, manager.sessions.len());
    serve_loop_with_output(
        manager,
        desktop,
        no_watch,
        track_current,
        interval_dur,
        first_deadline,
        rx,
        registry,
        |frame| {
            let mut stdout = std::io::stdout();
            writeln!(stdout, "{frame}").is_ok() && stdout.flush().is_ok()
        },
    )
}

// ---- off-loop work (NWS-2955, NWS-2962) ----
//
// Image requests run on their file's worker, desktop proxy calls on one
// desktop worker thread, and heavy local reads on the read pool
// (`reads.rs`). A handler that hands work off answers
// `ToolOutput::Deferred(token)`; the loop parks the call's `PendingCall`
// with where to answer it (stdio or a socket connection id) and writes the
// response when the token's `Done` arrives on the completion channel, which
// it drains every iteration exactly like session completions. JSON-RPC
// matches responses by id, so a deferred response may overtake or trail
// other responses on the same stream; the CLI's socket client sends one
// request per connection, so it never sees reordering. Two connections may
// reuse the same id: routing is by token, never by JSON-RPC id.
//
// Nothing on the loop writes to a client: every response frame is queued
// on its requester's writer thread (`output.rs`).

/// What a deferred call produced.
enum Answer {
    /// A tool result the loop frames with its parked `PendingCall`.
    Output(Result<ToolOutput, String>),
    /// A complete newline-terminated response frame, framed and
    /// serialized off the loop by a read worker.
    Frame(String),
}

/// A deferred tool call's result, sent to the main loop's completion
/// channel.
struct Done {
    token: u64,
    answer: Answer,
    /// A successful likely-write desktop call (spec §8): expedite every
    /// session's metadata poll on delivery.
    expedite_polls: bool,
}

/// Answers one deferred call exactly once. Dropping it unsent — a worker
/// that died mid-request, or queued work discarded at shutdown — still
/// answers, with an error, so no caller waits for a response that never
/// comes. A send after the loop exited is silently dropped.
pub(crate) struct DeferredReply {
    token: u64,
    done: Option<mpsc::Sender<Done>>,
}

impl DeferredReply {
    fn answer(&mut self, answer: Answer, expedite_polls: bool) {
        if let Some(done) = self.done.take() {
            let _ = done.send(Done {
                token: self.token,
                answer,
                expedite_polls,
            });
        }
    }

    fn send(mut self, output: Result<ToolOutput, String>, expedite_polls: bool) {
        self.answer(Answer::Output(output), expedite_polls);
    }

    fn send_frame(mut self, frame: String) {
        self.answer(Answer::Frame(frame), false);
    }
}

impl Drop for DeferredReply {
    fn drop(&mut self) {
        self.answer(
            Answer::Output(Err(
                "request abandoned: its worker stopped before answering".into(),
            )),
            false,
        );
    }
}

/// Where a deferred call's response is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ReplyTo {
    Stdio,
    Socket(u64),
}

/// Calls one requester may have pending (deferred and unanswered) at once.
/// Beyond it, a further `tools/call` from that requester is refused
/// immediately with a tool error rather than queued without bound.
const MAX_PENDING_PER_REQUESTER: usize = 32;

/// Deferred calls awaiting their `Done`, by token, with a per-requester
/// count. Ordered maps: nothing here ever iterates a `HashMap` toward
/// output.
#[derive(Default)]
struct Pending {
    calls: BTreeMap<u64, (ReplyTo, PendingCall)>,
    per_requester: BTreeMap<ReplyTo, usize>,
}

impl Pending {
    fn insert(&mut self, to: ReplyTo, call: PendingCall) {
        *self.per_requester.entry(to).or_default() += 1;
        self.calls.insert(call.token(), (to, call));
    }

    fn remove(&mut self, token: u64) -> Option<(ReplyTo, PendingCall)> {
        let (to, call) = self.calls.remove(&token)?;
        if let Some(count) = self.per_requester.get_mut(&to) {
            *count -= 1;
            if *count == 0 {
                self.per_requester.remove(&to);
            }
        }
        Some((to, call))
    }

    fn count(&self, to: ReplyTo) -> usize {
        self.per_requester.get(&to).copied().unwrap_or(0)
    }

    fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// The main loop's side of off-loop work: the completion channel, token
/// allocation, the read pool, and the desktop worker (if an upstream is
/// attached).
struct Offload {
    done_tx: mpsc::Sender<Done>,
    done_rx: mpsc::Receiver<Done>,
    next_token: u64,
    desktop: Option<DesktopWorker>,
    reads: ReadPool,
    /// A read the handler accepted, submitted once its `PendingCall` is
    /// known ([`Offload::park`]).
    staged: Option<StagedRead>,
    /// Tokens of reads submitted to the pool and not yet answered: while
    /// any is outstanding the loop polls for completions every
    /// [`READ_POLL`] instead of every 20ms.
    reads_in_flight: std::collections::BTreeSet<u64>,
    /// How long shutdown may take: [`EXIT_DRAIN`] (tests shorten it).
    exit_drain: Duration,
}

/// Completion polling interval while worker reads are outstanding, so a
/// read answered off the loop gains at most this much latency.
const READ_POLL: Duration = Duration::from_millis(2);

impl Offload {
    fn new(desktop: Option<DesktopWorker>) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            done_tx,
            done_rx,
            next_token: 0,
            desktop,
            reads: ReadPool::default(),
            staged: None,
            reads_in_flight: std::collections::BTreeSet::new(),
            exit_drain: EXIT_DRAIN,
        }
    }

    /// A fresh token (never reused, so a stale result can't match a later
    /// call) and the reply handle that answers it.
    fn reply(&mut self) -> (u64, DeferredReply) {
        let token = self.next_token;
        self.next_token += 1;
        (
            token,
            DeferredReply {
                token,
                done: Some(self.done_tx.clone()),
            },
        )
    }

    /// Accept `task` for the read pool. A requester that is already gone
    /// gets nothing computed.
    fn defer_read(
        &mut self,
        cancel: &Arc<AtomicBool>,
        task: ReadTask,
    ) -> Result<ToolOutput, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("request cancelled: its connection closed".into());
        }
        let (token, reply) = self.reply();
        self.staged = Some(StagedRead {
            token,
            task,
            reply,
            cancel: cancel.clone(),
        });
        Ok(ToolOutput::Deferred(token))
    }

    /// Hand a staged read its `PendingCall` and submit it to the pool.
    fn park(&mut self, call: &PendingCall) {
        // A stale stage (another token) is dropped and answers an error
        // to a token nobody parked.
        if let Some(staged) = self.staged.take()
            && staged.token == call.token()
        {
            self.reads_in_flight.insert(staged.token);
            self.reads.submit(staged, call.clone());
        }
    }
}

/// Apply a deferred result's loop-side effects and return its answer.
fn apply_done(
    manager: &mut SessionManager,
    no_watch: bool,
    interval: Duration,
    next_deadline: &mut Instant,
    done: Done,
) -> Answer {
    if done.expedite_polls && !no_watch {
        manager.expedite_polls(interval);
        *next_deadline = Instant::now();
    }
    done.answer
}

/// One response as a newline-terminated frame (compact JSON, as always).
fn frame(response: &Value) -> String {
    let mut frame = response.to_string();
    frame.push('\n');
    frame
}

/// The response frame for a deferred call's answer.
fn answer_frame(answer: Answer, call: PendingCall) -> String {
    match answer {
        Answer::Frame(frame) => frame,
        Answer::Output(output) => frame(&call.finish(output)),
    }
}

/// Queue one response frame for its requester. `false` means the stdio
/// writer is gone, which shuts the server down cleanly.
fn deliver(to: ReplyTo, frame: String, registry: &ConnRegistry, stdio: &StdioWriter) -> bool {
    match to {
        ReplyTo::Stdio => stdio.send(frame),
        ReplyTo::Socket(conn_id) => {
            registry.send(conn_id, frame);
            true
        }
    }
}

/// How long stdin EOF waits for deferred responses still owed (and then
/// for writers to flush them). Covers a desktop call (10s upstream
/// timeout) with margin; an image request that runs longer loses its
/// response (its file's worker still finishes it before exit, like a
/// running pull).
const EXIT_DRAIN: Duration = Duration::from_secs(15);

/// Shutdown: answer the deferred calls already accepted before exiting, so
/// `printf '<request>\n' | figmog serve` still gets its response. Accepts
/// no new requests and starts no new work (queued image requests and
/// queued desktop calls are answered with an error; running ones finish
/// and answer); waits until `deadline`. A vanished stdout or closed socket
/// just drops that response.
fn drain_on_exit(
    manager: &mut SessionManager,
    offload: &Offload,
    pending: &mut Pending,
    registry: &ConnRegistry,
    stdio: &StdioWriter,
    deadline: Instant,
) {
    // Before anything waits: the desktop worker must not pick up a queued
    // (possibly mutating) call while in-flight work drains.
    if let Some(desktop) = &offload.desktop {
        desktop.stop_taking_jobs();
    }
    for session in &mut manager.sessions {
        session.abandon_queued_images();
    }
    while !pending.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(done) = offload.done_rx.recv_timeout(left) else {
            return;
        };
        let Some((to, call)) = pending.remove(done.token) else {
            continue;
        };
        // No poll expedite: the server is exiting.
        if !deliver(to, answer_frame(done.answer, call), registry, stdio) {
            return;
        }
    }
}

/// Why the request loop returned.
enum LoopExit {
    /// Stdin EOF or the request channel closed: drain owed responses.
    Drain,
    /// Stdout is gone: nobody to drain to on stdio.
    StdoutGone,
}

#[allow(clippy::too_many_arguments)]
fn serve_loop_with_output(
    manager: SessionManager,
    desktop: Option<DesktopWorker>,
    no_watch: bool,
    track_current: bool,
    interval_dur: Duration,
    first_deadline: Instant,
    rx: mpsc::Receiver<Incoming>,
    registry: ConnRegistry,
    write_stdio: impl FnMut(&str) -> bool + Send + 'static,
) -> Result<(), String> {
    serve_loop_with_offload(
        manager,
        Offload::new(desktop),
        no_watch,
        track_current,
        interval_dur,
        first_deadline,
        rx,
        registry,
        write_stdio,
    )
}

/// [`serve_loop_with_output`] with a caller-built [`Offload`] (tests set
/// read-pool hooks on it).
#[allow(clippy::too_many_arguments)]
fn serve_loop_with_offload(
    mut manager: SessionManager,
    mut offload: Offload,
    no_watch: bool,
    track_current: bool,
    interval_dur: Duration,
    first_deadline: Instant,
    rx: mpsc::Receiver<Incoming>,
    registry: ConnRegistry,
    write_stdio: impl FnMut(&str) -> bool + Send + 'static,
) -> Result<(), String> {
    let stdio = StdioWriter::spawn(write_stdio);
    let mut pending = Pending::default();
    let exit = request_loop(
        &mut manager,
        &mut offload,
        &mut pending,
        no_watch,
        track_current,
        interval_dur,
        first_deadline,
        &rx,
        &registry,
        &stdio,
    );
    let deadline = Instant::now() + offload.exit_drain;
    if matches!(exit, LoopExit::Drain) {
        drain_on_exit(
            &mut manager,
            &offload,
            &mut pending,
            &registry,
            &stdio,
            deadline,
        );
    }
    // Stop the read pool within the deadline (queued reads are skipped; a
    // read still running then is detached with its own lease), then close
    // every store behind the reset barrier — waiting at most until the
    // same deadline — before the sessions drop. Nothing reopens a store.
    offload.reads.shutdown(deadline);
    drop(offload);
    manager.close_reads(deadline);
    registry.finish(deadline);
    stdio.finish(deadline);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn request_loop(
    manager: &mut SessionManager,
    offload: &mut Offload,
    pending: &mut Pending,
    no_watch: bool,
    track_current: bool,
    interval_dur: Duration,
    first_deadline: Instant,
    rx: &mpsc::Receiver<Incoming>,
    registry: &ConnRegistry,
    stdio: &StdioWriter,
) -> LoopExit {
    let mut tools = merged_registry(offload.desktop.as_ref());
    let mut watch = WatchScheduler::new(first_deadline);
    // Stdio is never cancelled: it is the only client of its stream.
    let stdio_live = Arc::new(AtomicBool::new(false));
    // Stdio sent `initialize`, so it gets notifications (NWS-2959).
    let mut stdio_subscribed = false;

    loop {
        // A vanished stdio reader is a clean shutdown; accepted running
        // pulls drain through FileSession's drop handler.
        if stdio.gone() {
            return LoopExit::StdoutGone;
        }
        // Completions and due watch work run even under continuous traffic.
        // Polling this in-memory channel never waits on a pull or HTTP.
        let mut published = Vec::new();
        for session in &mut manager.sessions {
            if session.maintain(!no_watch) {
                published.push(session.key.clone());
            }
        }
        for (key, outcome) in manager.service_resets() {
            match outcome {
                Ok(()) => published.push(key),
                Err(error) => eprintln!("figmog: reopening {key} after a reset failed: {error}"),
            }
        }
        for key in published {
            refresh_current(manager, track_current, &key);
        }
        // Deferred results go back to whoever asked. A token with no
        // pending entry was never parked (its call failed before
        // deferring), so its result has no requester.
        while let Ok(done) = offload.done_rx.try_recv() {
            offload.reads_in_flight.remove(&done.token);
            let Some((to, call)) = pending.remove(done.token) else {
                continue;
            };
            let answer = apply_done(manager, no_watch, interval_dur, &mut watch.next_round, done);
            if !deliver(to, answer_frame(answer, call), registry, stdio) {
                return LoopExit::StdoutGone;
            }
        }
        if !no_watch {
            watch.service_at(manager, interval_dur, Instant::now());
        }
        // Desktop re-detection (NWS-2959): probes run on the desktop
        // worker; only a change in the exposed tool list re-merges and is
        // announced, queued behind every response already sent.
        if let Some(desktop) = &mut offload.desktop
            && desktop.maintain(Instant::now())
        {
            tools = merged_registry(Some(desktop));
            let note = frame(&mcp::tools_list_changed());
            registry.broadcast(&note);
            if stdio_subscribed && !stdio.send(note) {
                return LoopExit::StdoutGone;
            }
        }
        let poll = if offload.reads_in_flight.is_empty() {
            Duration::from_millis(20)
        } else {
            READ_POLL
        };
        let wait = poll.min(if no_watch {
            Duration::from_millis(20)
        } else {
            watch
                .next_deadline(manager)
                .saturating_duration_since(Instant::now())
        });
        let msg = match rx.recv_timeout(wait) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return LoopExit::Drain,
        };

        let (to, line, mode, cancel) = match msg {
            Incoming::StdinClosed => return LoopExit::Drain,
            Incoming::Stdio(l) => (
                ReplyTo::Stdio,
                l,
                ToolErrorMode::McpContent,
                stdio_live.clone(),
            ),
            Incoming::Socket(conn_id, l) => (
                ReplyTo::Socket(conn_id),
                l,
                ToolErrorMode::JsonRpc,
                registry.cancel_flag(conn_id),
            ),
        };

        let over_limit = pending.count(to) >= MAX_PENDING_PER_REQUESTER;
        offload.staged = None;
        let upstream_status = offload
            .desktop
            .as_ref()
            .map_or("disabled", DesktopWorker::status);
        let mut handler = LoopHandler::new(
            |name: &str, args: &Value| -> Result<ToolOutput, String> {
                if over_limit {
                    return Err(format!(
                        "too many pending requests on this connection (limit {MAX_PENDING_PER_REQUESTER}); wait for responses before sending more"
                    ));
                }
                handle_tool_call(
                    manager,
                    offload,
                    &cancel,
                    upstream_status,
                    interval_dur,
                    name,
                    args,
                )
            },
        );
        let reply = mcp::dispatch_message(&line, &tools, &mut handler, mode);
        let LoopHandler {
            initialized,
            unknown_tool,
            ..
        } = handler;
        if initialized {
            match to {
                ReplyTo::Stdio => stdio_subscribed = true,
                ReplyTo::Socket(conn_id) => registry.subscribe(conn_id),
            }
        }
        // A desktop tool called while the desktop is missing: re-check now.
        // The call itself is refused as an unknown tool, not held for the
        // probe; a client that sent `initialize` hears about the tools
        // through `tools/list_changed` and can retry.
        if let Some(name) = unknown_tool
            && !proxy::is_local_tool(&name)
            && let Some(desktop) = &mut offload.desktop
            && !desktop.connected()
        {
            desktop.request_probe(Instant::now());
        }
        match reply {
            None => {}
            Some(Reply::Later(call)) => {
                offload.park(&call);
                pending.insert(to, call);
            }
            Some(Reply::Now(response)) if !deliver(to, frame(&response), registry, stdio) => {
                return LoopExit::StdoutGone;
            }
            Some(Reply::Now(_)) => {}
        }
    }
}

/// The local registry merged with the desktop's current tools (none when
/// it is missing or disabled). Collisions are dropped and logged on every
/// merge (spec §8); the order is local tools, then the desktop's own order.
fn merged_registry(desktop: Option<&DesktopWorker>) -> Vec<mcp::ToolDef> {
    let (tools, dropped) = match desktop {
        Some(desktop) => proxy::merge_registry(dispatch::tool_registry(), desktop.tools()),
        None => (dispatch::tool_registry(), Vec::new()),
    };
    for name in &dropped {
        eprintln!("figmog: dropping upstream tool named like a local tool: {name}");
    }
    tools
}

/// The loop's tool handler: runs calls through `call`, and records the
/// protocol events the loop acts on after dispatch (NWS-2959).
struct LoopHandler<F> {
    call: F,
    /// The client sent `initialize`.
    initialized: bool,
    /// A `tools/call` was refused as an unknown tool.
    unknown_tool: Option<String>,
}

impl<F> LoopHandler<F> {
    fn new(call: F) -> Self {
        Self {
            call,
            initialized: false,
            unknown_tool: None,
        }
    }
}

impl<F: FnMut(&str, &Value) -> Result<ToolOutput, String>> mcp::ToolHandler for LoopHandler<F> {
    fn call(&mut self, name: &str, args: &Value) -> Result<ToolOutput, String> {
        (self.call)(name, args)
    }

    fn initialized(&mut self) {
        self.initialized = true;
    }

    fn unknown_tool(&mut self, name: &str) {
        self.unknown_tool = Some(name.to_string());
    }
}

/// Open startup stores and queue initial pulls on their workers. Store-open
/// and credential errors still fail startup; network pull failures are
/// observable job failures after the server begins serving. Only committed
/// default-file publications refresh `.figmog/current`.
fn build_sessions(
    db_override: Option<PathBuf>,
    files: &[String],
    no_watch: bool,
    token: &Option<String>,
    store_root: StoreRoot,
    interval: Duration,
) -> Result<(SessionManager, bool), String> {
    if let Some(path) = db_override {
        // A `file` positional may still be given alongside `--db` (pre-v4
        // behavior: `--db` fixes the store path, an optional file arg
        // still resolves the key network operations need). With neither,
        // `network_key` stays `None` and the session's own `pull` closure
        // refuses immediately with the old clean "no file key" message —
        // `figmog_sync`/watch under `--no-watch` reach that at call time,
        // same as pre-v4 (`!no_watch` is checked here too, eagerly, so
        // watch mode still fails fast at startup rather than waiting for
        // a tick).
        let key_opt = files.first().and_then(|f| parse_file_ref(f));
        if !no_watch {
            key_opt
                .clone()
                .ok_or_else(|| "no file key: pass a file key or figma.com URL".to_string())?;
            token
                .clone()
                .ok_or_else(|| crate::auth::missing_token("watch"))?;
        }
        let display_key = key_opt
            .clone()
            .unwrap_or_else(|| path.display().to_string());
        let session = sessions::open_session_at(
            path,
            display_key.clone(),
            key_opt.as_deref(),
            token.as_deref(),
        )?;
        let mut manager = SessionManager {
            sessions: vec![session],
            root: store_root,
            token: token.clone(),
            default_key: Some(display_key),
            #[cfg(test)]
            api: None,
        };
        if !no_watch {
            let session = &mut manager.sessions[0];
            if !session.mirrored {
                // No user-facing geometry flag at startup (spec §4) — the
                // mirror's stored setting, if any, still drives this.
                session.request_pull(false, interval);
            }
        }
        // `--db` never resolves a tracked key (pre-v4: `resolve_db`
        // short-circuited to `Db { key: None, .. }` whenever `--db` was
        // given), so it never wrote `.figmog/current` either — preserved
        // verbatim via `track_current = false` below.
        return Ok((manager, false));
    }

    // Watch needs a token to keep polling for the life of the process,
    // independent of whether any startup file actually needs an initial
    // pull — same eager requirement the old single-file path had, scaled
    // to "there's at least one file to watch" (spec §14: zero files is a
    // valid, token-free, idle startup).
    if !no_watch && !files.is_empty() {
        token
            .clone()
            .ok_or_else(|| crate::auth::missing_token("watch"))?;
    }

    let mut manager = SessionManager {
        sessions: Vec::new(),
        root: store_root,
        token: token.clone(),
        default_key: None,
        #[cfg(test)]
        api: None,
    };
    add_startup_files(&mut manager, files, no_watch, interval)?;
    Ok((manager, true))
}

/// Queue missing startup mirrors through the same session lifecycle used
/// by implicit first reads and explicit requests.
fn add_startup_files(
    manager: &mut SessionManager,
    files: &[String],
    no_watch: bool,
    interval: Duration,
) -> Result<(), String> {
    for (i, f) in files.iter().enumerate() {
        let session = manager.open(f)?;
        let key = session.key.clone();
        if !no_watch && !session.mirrored {
            session.request_pull(false, interval);
        }
        if i == 0 {
            manager.default_key = Some(key);
        }
    }
    Ok(())
}

/// Normal rounds and per-file retries share a rate-limited dispatch slot.
/// Alternating when both are due keeps either queue from starving the other.
struct WatchScheduler {
    next_round: Instant,
    round_cursor: usize,
    retry_cursor: usize,
    next_slot: Option<Instant>,
    retry_first: bool,
}

impl WatchScheduler {
    fn new(first_deadline: Instant) -> Self {
        Self {
            next_round: first_deadline,
            round_cursor: 0,
            retry_cursor: 0,
            next_slot: None,
            retry_first: true,
        }
    }

    fn next_deadline(&self, manager: &SessionManager) -> Instant {
        let retry = manager
            .sessions
            .iter()
            .filter_map(|session| session.watch_retry_deadline())
            .min();
        let due = retry.map_or(self.next_round, |retry| retry.min(self.next_round));
        self.next_slot.map_or(due, |slot| due.max(slot))
    }

    fn dispatch_retry(
        &mut self,
        manager: &mut SessionManager,
        index: usize,
        interval: Duration,
        now: Instant,
    ) {
        if manager.sessions[index].retry_poll_at(interval, now) {
            let count = manager.sessions.len();
            // One request can satisfy a retry and the same file's due
            // normal visit; avoid issuing another poll two seconds later.
            if now >= self.next_round && index == self.round_cursor % count {
                self.round_cursor = (index + 1) % count;
                self.next_round = now + tick_deadline(interval, count);
            }
            self.retry_cursor = (index + 1) % count;
            self.retry_first = false;
            self.next_slot = Some(now + MIN_TICK_DEADLINE);
        }
    }

    fn service_at(&mut self, manager: &mut SessionManager, interval: Duration, now: Instant) {
        if self.next_slot.is_some_and(|slot| now < slot) {
            return;
        }
        let retry = (0..manager.sessions.len())
            .map(|offset| (self.retry_cursor + offset) % manager.sessions.len())
            .find(|&index| {
                manager.sessions[index]
                    .watch_retry_deadline()
                    .is_some_and(|due| due <= now)
            });
        let round_due = now >= self.next_round;
        if let Some(index) = retry
            && (!round_due || self.retry_first)
        {
            self.dispatch_retry(manager, index, interval, now);
            return;
        }
        if round_due {
            let (next, dispatched) = watch_tick_at(manager, &mut self.round_cursor, interval, now);
            self.next_round = next;
            if dispatched {
                self.retry_first = true;
                self.next_slot = Some(now + MIN_TICK_DEADLINE);
                return;
            }
        }
        if let Some(index) = retry {
            self.dispatch_retry(manager, index, interval, now);
        }
    }
}

/// Schedule at most one due metadata poll; the session worker performs it.
fn watch_tick_at(
    manager: &mut SessionManager,
    next_idx: &mut usize,
    interval: Duration,
    now: Instant,
) -> (Instant, bool) {
    if manager.sessions.is_empty() {
        return (now + tick_deadline(interval, 0), false);
    }
    let count = manager.sessions.len();
    let idx = *next_idx % count;
    *next_idx = (*next_idx + 1) % count;
    let dispatched = manager.sessions[idx].poll_at(interval, now);
    (now + tick_deadline(interval, count), dispatched)
}

/// Refresh `.figmog/current` to `key` — only when `track` (the no-`--db`-
/// override startup path) and `key` is the *default* session's (spec §14's
/// default rule via [`SessionManager::effective_default_key`], not merely
/// index 0), matching old single-file behavior for the one invocation
/// shape that used to do this (`figmog serve <file>`, no `--db`).
fn refresh_current(manager: &SessionManager, track: bool, key: &str) {
    if track && manager.effective_default_key().as_deref() == Some(key) {
        let _ = crate::cli::write_current(&manager.root.local_root(), key);
    }
}

/// spec §2b: note an explicit-file/URL-file disagreement on a failed call
/// (almost always "no node ... in the mirror" — the node genuinely isn't in
/// the file the explicit arg picked) rather than staying silent about why a
/// URL that looks right didn't resolve. `explicit_key` is the explicit
/// `file` argument's *normalized* key, not its raw text — it can itself be
/// a full Figma URL (or a bare key) naming the very same file `url_key`
/// already extracted, and a raw-string compare would false-positive on
/// that (same file, different spelling). A `file` arg `parse_file_ref`
/// can't make sense of at all falls back to the raw text so a genuine
/// mismatch still shows.
fn file_mismatch_note(msg: String, explicit_key: Option<&str>, url_key: Option<&str>) -> String {
    match (explicit_key, url_key) {
        (Some(ef), Some(uf)) if ef != uf => format!(
            "{msg} (note: the id/under URL names file {uf}, but the explicit `file` argument {ef} was used instead)"
        ),
        _ => msg,
    }
}

/// Route one `tools/call`. `figmog_files`/`figmog_open` aren't per-file
/// and are handled first; every other local tool's optional `file`
/// argument is extracted (and stripped before tool-specific arg parsing)
/// and routed through [`SessionManager::resolve`]; non-local names are
/// proxied — see this module's doc comment for the cache-routing choice.
/// Open/sync and implicit first reads schedule workers; they never perform
/// a pull on the request thread. Status and files report job completion.
///
/// Images and desktop calls answer [`ToolOutput::Deferred`]: their network
/// work runs off the loop (on the file's worker and the desktop worker) and
/// their result arrives later through `offload`'s completion channel.
///
/// Heavy local reads, `figmog_status` and `figmog_files` (NWS-2962) also
/// answer [`ToolOutput::Deferred`]: they run on the read pool against a
/// leased published view; `cancel` (the requester's) lets the pool skip
/// them once nobody is left to answer.
fn handle_tool_call(
    manager: &mut SessionManager,
    offload: &mut Offload,
    cancel: &Arc<AtomicBool>,
    upstream_status: &'static str,
    interval: Duration,
    name: &str,
    args: &Value,
) -> Result<ToolOutput, String> {
    if name == "figmog_files" {
        let read = manager.files_read();
        return offload.defer_read(cancel, ReadTask::Files(read));
    }

    if name == "figmog_open" {
        let file = dispatch::require_str(args, "file")?;
        // spec §4: `geometry` turns sticky vector-geometry requests on for
        // this mirror going forward; omitted preserves whatever's already
        // stored (default false for a brand-new mirror) — the worker's
        // stored-flag union handles that, so `false` here is exactly "no
        // new override" rather than "force off".
        let geometry = dispatch::arg_bool(args, "geometry")?;
        let session = manager.open(&file)?;
        return Ok(ToolOutput::Json(session.request_pull(geometry, interval)));
    }

    // spec §2b: an explicit `file` arg always wins, but when it's absent
    // and the call's node-id-shaped argument (`id`/`under`/`target`) is a
    // full Figma URL naming a file, that URL's file key routes the call
    // (auto-open semantics apply, same as an explicit `file`). When both
    // are present and disagree, the explicit arg still wins — the
    // disagreement is only surfaced as a note on a not-found error below,
    // never silently overridden the other way.
    // Desktop tools' arguments are the upstream's own business; only a
    // local tool's `file` is figmog's routing property to type-check.
    let explicit_file = if proxy::is_local_tool(name) {
        dispatch::arg_str(args, "file")?
    } else {
        None
    };
    let url_file_key = dispatch::infer_file_from_node_ref(args);
    let file_arg = explicit_file.clone().or_else(|| url_file_key.clone());
    let mut call_args = args.clone();
    if let Some(obj) = call_args.as_object_mut() {
        obj.remove("file");
    }

    if proxy::is_local_tool(name) {
        let session = manager.resolve(file_arg.as_deref(), interval)?;

        if name == "figmog_sync" {
            return Ok(ToolOutput::Json(session.request_pull(false, interval)));
        }
        if name == "figmog_status" {
            let read = session.status_read();
            return offload.defer_read(
                cancel,
                ReadTask::Status {
                    read,
                    upstream: upstream_status,
                },
            );
        }

        if !session.mirrored {
            session.request_pull(false, interval);
        }
        if name == "figmog_images" {
            // Failed first mirrors share the data-read retry path above.
            // Once ready, the file's worker scans, fetches and caches;
            // the response is delivered when it finishes.
            let ids = dispatch::require_str_array(args, "ids")?;
            let format = dispatch::arg_str(args, "format")?;
            let format = format.as_deref().unwrap_or("png");
            let scale = dispatch::arg_f64(args, "scale")?;
            let (token, reply) = offload.reply();
            session.images(
                &ids,
                format,
                scale,
                Box::new(move |result| {
                    let output =
                        result.map(|items| ToolOutput::Raw(crate::images::to_mcp_content(&items)));
                    reply.send(output, false);
                }),
            )?;
            return Ok(ToolOutput::Deferred(token));
        }

        let explicit_key = explicit_file
            .as_deref()
            .map(|f| parse_file_ref(f).unwrap_or_else(|| f.to_string()));
        if !reads::is_inline_read(name) {
            let lease = session.lease()?;
            return offload.defer_read(
                cancel,
                ReadTask::Tool {
                    lease,
                    name: name.to_string(),
                    args: call_args,
                    explicit_key,
                    url_key: url_file_key,
                },
            );
        }
        return session.dispatch(name, &call_args).map_err(|msg| {
            file_mismatch_note(msg, explicit_key.as_deref(), url_file_key.as_deref())
        });
    }

    // Desktop context is independent of every mirror. Never route its
    // responses through a session's version-keyed cache. The desktop
    // worker forwards it; a likely write's poll expedite is applied when
    // the result is delivered (see `apply_done`).
    if offload.desktop.is_none() {
        return Err(format!("upstream not attached: {name}"));
    }
    let (token, reply) = offload.reply();
    if let Some(desktop) = &offload.desktop {
        desktop.submit(DesktopJob::Call {
            name: name.to_string(),
            args: args.clone(),
            reply,
        });
    }
    Ok(ToolOutput::Deferred(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_http::{Peer, RESPONSE_BOUND, Reply, TEST_TIMEOUT};
    use crate::api::{FigmaApi, UreqApi};
    use crate::mcp::ToolDef;
    use crate::upstream::FakeUpstream;
    use serde_json::json;

    #[test]
    fn watch_tick_reconciles_once_despite_different_endpoint_timestamps() {
        use crate::api::{ApiError, FileMetaResp};
        use std::sync::atomic::AtomicUsize;

        struct ScriptedApi {
            meta_calls: AtomicUsize,
            file_calls: AtomicUsize,
        }

        impl FigmaApi for ScriptedApi {
            fn file_meta(&self, _key: &str) -> Result<FileMetaResp, ApiError> {
                self.meta_calls.fetch_add(1, Ordering::Relaxed);
                Ok(FileMetaResp {
                    name: "Synthetic".into(),
                    last_touched_at: "2026-01-01T12:00:00.123Z".into(),
                })
            }

            fn file(&self, _key: &str, _geometry: bool) -> Result<Value, ApiError> {
                self.file_calls.fetch_add(1, Ordering::Relaxed);
                Ok(json!({
                    "name": "Synthetic", "version": "1", "lastModified": "2026-01-01T12:00:00Z",
                    "document": {"id": "0:0", "name": "Synthetic", "type": "DOCUMENT"}
                }))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let api = Arc::new(ScriptedApi {
            meta_calls: AtomicUsize::new(0),
            file_calls: AtomicUsize::new(0),
        });
        let session = sessions::open_session_with_api(
            dir.path().join("db"),
            "ABC123".into(),
            Some("ABC123"),
            Some(api.clone()),
        )
        .unwrap();
        let mut manager = SessionManager {
            sessions: vec![session],
            root: dir.path().to_path_buf().into(),
            token: None,
            default_key: Some("ABC123".into()),
            api: None,
        };
        let mut next_idx = 0;
        let interval = Duration::ZERO;

        watch_tick_at(&mut manager, &mut next_idx, interval, Instant::now());
        let deadline = Instant::now() + Duration::from_secs(5);
        while manager.list()[0]["pull"]["job"]["state"] != "succeeded" {
            manager.sessions[0].maintain(true);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(api.meta_calls.load(Ordering::Relaxed), 1);
        assert_eq!(api.file_calls.load(Ordering::Relaxed), 1);
        assert_eq!(manager.list()[0]["version"], "1");
        // Poll again with a zero per-file wait. Differing endpoint
        // timestamps must still require only the first reconciliation.
        manager.sessions[0].poll_at(Duration::ZERO, Instant::now());
        let deadline = Instant::now() + Duration::from_secs(5);
        while api.meta_calls.load(Ordering::Relaxed) < 2 {
            manager.sessions[0].maintain(true);
            manager.sessions[0].poll_at(Duration::ZERO, Instant::now());
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        manager.sessions[0].maintain(true);
        assert_eq!(api.file_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn serve_answers_local_requests_and_resumes_polling_after_http_timeout() {
        let peer = Peer::new(vec![
            Reply::Stall(""),
            Reply::Complete(
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"file\":{\"name\":\"Fixture\",\"last_touched_at\":\"t\"}}",
            ),
        ]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT);
        assert_serve_recovers(
            api,
            &peer.requests,
            "GET /v1/files/ABC123/meta ",
            RESPONSE_BOUND,
        );
    }

    #[test]
    fn serve_resumes_after_fragmented_tls_handshake() {
        assert_serve_recovers_from_tls(
            crate::api::test_https::Reply::HandshakeTrickle,
            "TLS handshake started",
        );
    }

    #[test]
    fn serve_resumes_after_fragmented_encrypted_body() {
        assert_serve_recovers_from_tls(
            crate::api::test_https::Reply::EncryptedTrickle(
                "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            ),
            "GET /v1/files/ABC123/meta ",
        );
    }

    fn assert_serve_recovers_from_tls(reply: crate::api::test_https::Reply, first: &str) {
        use crate::api::test_https;
        let peer = test_https::TlsPeer::new(vec![
            reply,
            test_https::Reply::Complete(
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"file\":{\"name\":\"Fixture\",\"last_touched_at\":\"t\"}}",
            ),
        ]);
        let api = UreqApi::with_base_url("synthetic-token".into(), peer.url.clone())
            .with_timeout(TEST_TIMEOUT)
            .with_tls_config(test_https::client_config());
        assert_serve_recovers(api, &peer.requests, first, Duration::from_secs(1));
    }

    fn assert_serve_recovers(
        api: UreqApi,
        requests: &mpsc::Receiver<String>,
        first_prefix: &str,
        response_bound: Duration,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let (tx, rx) = mpsc::channel();
        let (client, socket) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(response_bound)).unwrap();
        socket.set_write_timeout(Some(RESPONSE_BOUND)).unwrap();
        let worker = std::thread::spawn(move || {
            let session = sessions::open_session_with_api(
                db.clone(),
                "ABC123".into(),
                Some("ABC123"),
                Some(Arc::new(api)),
            )
            .unwrap();
            let manager = SessionManager {
                sessions: vec![session],
                root: db.into(),
                token: Some("synthetic-token".into()),
                default_key: Some("ABC123".into()),
                api: None,
            };
            let registry: ConnRegistry = Arc::default();
            registry.register(1, socket).unwrap();
            serve_loop(
                manager,
                None,
                false,
                false,
                Duration::from_secs(2),
                rx,
                registry,
            )
            .unwrap();
        });

        // Queue a local request while the first meta poll is blocked on
        // transport. Its response must arrive before the peer closes
        // its deliberately incomplete response/record (at least 3s).
        let first = requests.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(first.starts_with(first_prefix), "{first}");
        let first_poll = Instant::now();
        let mut reader = BufReader::new(client);
        let request = |id| {
            Incoming::Socket(
                1,
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": "figmog_files", "arguments": {}}
                })
                .to_string(),
            )
        };
        tx.send(request(1)).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], json!(1));
        assert_eq!(response["result"]["isError"], json!(false));
        let files: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(files[0]["key"], json!("ABC123"));
        assert!(first_poll.elapsed() < response_bound);

        // The ordinary five-second failure backoff expires, and a new
        // HTTP poll succeeds. The same loop still answers local calls.
        let next = requests.recv_timeout(Duration::from_secs(8)).unwrap();
        assert!(next.starts_with("GET /v1/files/ABC123/meta "));
        assert!(first_poll.elapsed() >= Duration::from_secs(5));
        tx.send(request(2)).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], json!(2));
        assert_eq!(response["result"]["isError"], json!(false));
        tx.send(Incoming::StdinClosed).unwrap();
        worker.join().unwrap();
    }

    fn local_registry() -> Vec<ToolDef> {
        dispatch::tool_registry()
    }

    #[test]
    fn merged_registry_places_local_tools_first_then_upstream_verbatim() {
        let upstream = FakeUpstream::new(vec![json!({
            "name": "get_design_context",
            "description": "Design context for a node",
            "inputSchema": {"type": "object"},
        })]);
        let (tools, dropped) = proxy::merge_registry(local_registry(), upstream.tools());
        assert!(dropped.is_empty());
        assert_eq!(tools.len(), 22);
        assert!(tools[..21].iter().all(|t| t.name.starts_with("figmog_")));
        assert_eq!(tools[21].name, "get_design_context");
        assert!(tools[21].description.starts_with("[via Figma desktop] "));
    }

    #[test]
    fn routing_local_name_never_reaches_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let mut st = crate::open_store!(&db);
        let flattened = crate::flatten::flatten_file(&json!({
            "name": "F", "version": "1", "lastModified": "t",
            "document": {"id": "0:0", "name": "Document", "type": "DOCUMENT", "children": []},
            "components": {}, "componentSets": {}, "styles": {},
        }))
        .unwrap();
        crate::store::sync(&mut st, &std::collections::BTreeSet::new(), &flattened, 0);

        let result =
            st.rtx(|r| dispatch::dispatch_read_tool("figmog_status", &json!({}), "connected", &r));
        let value = result
            .expect("figmog_status is a recognized local tool")
            .unwrap();
        assert_eq!(value["upstream"], json!("connected"));

        // A non-figmog_ name is simply not recognized by the local
        // dispatcher — proving the namespace rule routes it away from the
        // local path without needing a live upstream to demonstrate it.
        let result =
            st.rtx(|r| dispatch::dispatch_read_tool("get_code", &json!({}), "connected", &r));
        assert!(result.is_none());
    }

    #[test]
    fn clamp_interval_caps_at_max_and_leaves_sane_values_alone() {
        assert_eq!(clamp_interval(10), Duration::from_secs(10));
        assert_eq!(
            clamp_interval(MAX_INTERVAL_SECS),
            Duration::from_secs(MAX_INTERVAL_SECS)
        );
        // A huge/typo'd interval — including `u64::MAX`, which would
        // otherwise overflow `Instant::now() + interval` — clamps rather
        // than propagating.
        assert_eq!(
            clamp_interval(u64::MAX),
            Duration::from_secs(MAX_INTERVAL_SECS)
        );
        assert_eq!(
            clamp_interval(MAX_INTERVAL_SECS + 1),
            Duration::from_secs(MAX_INTERVAL_SECS)
        );
    }

    #[test]
    fn tick_deadline_splits_interval_across_sessions_floored_at_2s() {
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 0),
            Duration::from_secs(10)
        );
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 1),
            Duration::from_secs(10)
        );
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 5),
            Duration::from_secs(2)
        );
        // Floored: 10s / 100 sessions would be 0.1s, floored to 2s.
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 100),
            Duration::from_secs(2)
        );
        // `--interval 0` is floored too, with or without sessions
        // (NWS-2938: an unfloored idle wait busy-looped the server).
        for sessions in [0, 1, 5] {
            assert_eq!(tick_deadline(Duration::ZERO, sessions), MIN_TICK_DEADLINE);
        }
    }

    #[test]
    fn empty_server_with_zero_interval_never_schedules_a_zero_wait() {
        // NWS-2938: with no sessions and `--interval 0`, every idle watch
        // round used to land at `now`, so the request loop's
        // `recv_timeout` got a zero wait and spun. The idle wakeup must
        // stay at least the two-second floor away, every round.
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager {
            sessions: Vec::new(),
            root: dir.path().to_path_buf().into(),
            token: None,
            default_key: None,
            api: None,
        };
        let mut watch = WatchScheduler::new(Instant::now());
        let mut now = Instant::now();
        for _ in 0..3 {
            watch.service_at(&mut manager, Duration::ZERO, now);
            let deadline = watch.next_deadline(&manager);
            assert!(
                deadline >= now + MIN_TICK_DEADLINE,
                "idle wakeup scheduled {:?} after now",
                deadline.saturating_duration_since(now)
            );
            now = deadline;
        }
    }

    // ---- unix-socket control plane (v0.0.2 spec §1) ----

    #[test]
    fn classify_probe_connect_success_means_owned() {
        assert_eq!(classify_probe(Ok(())).unwrap(), StaleProbe::Owned);
    }

    #[test]
    fn classify_probe_connection_refused_means_stale() {
        let err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(classify_probe(Err(err)).unwrap(), StaleProbe::Stale);
    }

    #[test]
    fn classify_probe_any_other_connect_error_also_means_stale() {
        // A path that exists but isn't a live listening socket at all
        // (garbage, or some other filesystem error) is treated the same
        // as a refused connection — see `classify_probe`'s doc comment.
        let err = std::io::Error::other("not a socket");
        assert_eq!(classify_probe(Err(err)).unwrap(), StaleProbe::Stale);
    }

    #[test]
    fn classify_probe_invalid_address_does_not_mean_stale() {
        let error = std::io::Error::from(std::io::ErrorKind::InvalidInput);
        assert_eq!(
            classify_probe(Err(error)).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn incoming_messages_carry_their_connection_tag_through_the_channel() {
        // Proves the tagging/routing mechanism itself: several sources
        // (stdin, two different socket connections) share one channel, and
        // each `Incoming` value on the receiving end still carries enough
        // information to route a response back to the right place.
        let (tx, rx) = mpsc::channel::<Incoming>();
        tx.send(Incoming::Stdio("stdio line".to_string())).unwrap();
        tx.send(Incoming::Socket(7, "first conn".to_string()))
            .unwrap();
        tx.send(Incoming::Socket(9, "second conn".to_string()))
            .unwrap();
        tx.send(Incoming::StdinClosed).unwrap();

        match rx.recv().unwrap() {
            Incoming::Stdio(l) => assert_eq!(l, "stdio line"),
            other => panic!("expected Stdio, got {other:?}"),
        }
        match rx.recv().unwrap() {
            Incoming::Socket(id, l) => {
                assert_eq!(id, 7);
                assert_eq!(l, "first conn");
            }
            other => panic!("expected Socket(7, ..), got {other:?}"),
        }
        match rx.recv().unwrap() {
            Incoming::Socket(id, l) => {
                assert_eq!(id, 9);
                assert_eq!(l, "second conn");
            }
            other => panic!("expected Socket(9, ..), got {other:?}"),
        }
        assert!(matches!(rx.recv().unwrap(), Incoming::StdinClosed));
    }

    #[test]
    fn socket_response_is_delivered_to_the_registered_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        let registry: ConnRegistry = Arc::default();
        registry.register(1, server_side).unwrap();

        registry.send(1, frame(&json!({"hello": "world"})));

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v, json!({"hello": "world"}));
        // The write succeeded, so the entry is still there for a
        // subsequent response on the same connection.
        assert!(registry.is_registered(1));
        assert!(!registry.cancel_flag(1).load(Ordering::Relaxed));
    }

    #[test]
    fn socket_response_for_an_unregistered_connection_is_a_silent_no_op() {
        // A response for a connection id that was never registered (or
        // whose entry was already removed — the client vanished) must not
        // panic; the response is simply dropped.
        let registry: ConnRegistry = Arc::default();
        registry.send(999, frame(&json!({"ignored": true})));
        assert!(!registry.is_registered(999));
        assert!(registry.cancel_flag(999).load(Ordering::Relaxed));
    }

    #[test]
    fn bind_socket_creates_a_0600_file_and_a_second_probe_sees_it_as_owned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, path) = bind_socket(&root).expect("first bind should succeed");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket file should be mode 0600");

        // A second bind attempt against the same root, with the first
        // listener still alive, must classify the existing file as owned
        // (spec §1: "success ⇒ another serve owns this root") rather than
        // stealing it.
        let err = bind_socket(&root).unwrap_err();
        assert!(err.contains("another figmog serve owns this root"), "{err}");

        drop(listener);
    }

    #[test]
    fn bind_socket_unlinks_and_rebinds_a_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        // A leftover regular file at the socket path, with nothing
        // listening — the simplest reproduction of a stale socket from an
        // unclean exit.
        std::fs::write(socket_path(&root), b"not a socket").unwrap();

        let (listener, _path) = bind_socket(&root).expect("stale file should be reclaimed");
        drop(listener);
    }

    #[test]
    fn bind_socket_sets_the_root_dir_mode_to_0700() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, _path) = bind_socket(&root).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "figmog-root should be tightened to 0700");
        drop(listener);
    }

    #[test]
    fn bind_socket_preserves_a_live_socket_when_its_address_is_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("a".repeat(90)).join("b".repeat(90));
        std::fs::create_dir_all(root.parent().unwrap()).unwrap();
        let staging = tempfile::tempdir().unwrap();
        let _listener = UnixListener::bind(socket_path(staging.path())).unwrap();
        std::fs::rename(staging.path(), &root).unwrap();
        let path = socket_path(&root);
        let before = std::fs::metadata(&path).unwrap();

        let error = bind_socket(&root).unwrap_err();

        assert!(error.contains("probing socket"), "{error}");
        let after = std::fs::metadata(&path).expect("invalid address must not unlink live socket");
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    }

    // ---- review fix round: C1/C2/m1-m6 ----

    #[test]
    fn socket_guard_does_not_unlink_a_file_replaced_at_the_same_path() {
        // Simulates a racer, or an operator manually `rm`-ing and
        // recreating the socket path while serve is still running (m3):
        // the guard must check the file is still the *inode* it bound, not
        // just that "something" exists at the path, before removing it.
        //
        // The replacement is built at a *sibling* path while the original
        // still exists, then moved over the guard's path with `rename(2)`
        // — the only way to guarantee a genuinely different inode across
        // platforms. A plain remove-then-recreate at the same path doesn't
        // reliably do that: ext4 (the common Linux CI filesystem) recycles
        // inode numbers immediately, so a delete-then-recreate at the same
        // path can legitimately land on the exact same inode the guard
        // bound — which would make this test pass for the wrong reason on
        // Linux (or, as originally written, fail there outright, since two
        // *simultaneously live* files are guaranteed distinct inodes but a
        // remove-then-recreate isn't). `rename(2)` preserves the source
        // inode across the move, so the file at `path` afterward is
        // provably not the guard's original inode, on every platform.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, path) = bind_socket(&root).unwrap();
        let guard = SocketGuard::new(path.clone()).unwrap();
        drop(listener);

        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"someone else's file now").unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        drop(guard);

        assert!(
            path.exists(),
            "the guard must not remove a file it didn't itself create"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "someone else's file now"
        );
    }

    #[test]
    fn socket_guard_unlinks_its_own_untouched_socket_file() {
        // The ordinary case, pinned alongside the "don't touch a replaced
        // file" regression above so the two behaviors stay in view
        // together.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, path) = bind_socket(&root).unwrap();
        let guard = SocketGuard::new(path.clone()).unwrap();
        drop(listener);

        drop(guard);

        assert!(
            !path.exists(),
            "the guard should remove its own socket file"
        );
    }

    #[test]
    fn read_bounded_line_returns_ok_none_on_clean_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        drop(client); // EOF from the server's read side, no data ever sent.

        let mut reader = BufReader::new(server_side);
        assert_eq!(read_bounded_line(&mut reader).unwrap(), None);
    }

    #[test]
    fn read_bounded_line_splits_multiple_pipelined_frames_from_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        // Two frames written in one `write_all` call, arriving as (very
        // likely) one chunk from the reader's perspective — proving
        // `read_bounded_line` only consumes up through the first newline,
        // leaving the second frame's bytes buffered for the next call
        // rather than dropping them.
        client.write_all(b"first\nsecond\n").unwrap();

        let mut reader = BufReader::new(server_side);
        assert_eq!(
            read_bounded_line(&mut reader).unwrap(),
            Some("first".to_string())
        );
        assert_eq!(
            read_bounded_line(&mut reader).unwrap(),
            Some("second".to_string())
        );
    }

    #[test]
    fn read_bounded_line_disconnects_on_an_oversized_frame() {
        // review m4: a line with no newline that exceeds `MAX_FRAME_BYTES`
        // must error (disconnect the connection) rather than growing the
        // buffer without bound.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        let oversized = vec![b'a'; MAX_FRAME_BYTES + 1];
        let writer = std::thread::spawn(move || {
            // Never completes the frame with a newline — the reader must
            // give up once the cap is exceeded, not once EOF arrives.
            let _ = client.write_all(&oversized);
        });

        let mut reader = BufReader::new(server_side);
        let result = read_bounded_line(&mut reader);
        assert!(
            result.is_err(),
            "an oversized frame should error, not block forever collecting it"
        );

        let _ = writer.join();
    }

    #[test]
    fn a_non_reading_client_is_dropped_without_blocking_the_sender() {
        // review I1 / NWS-2962: a client that never drains what serve
        // writes back can neither hang the loop (queueing never blocks)
        // nor hold memory without bound: once its queue is full the
        // connection is dropped, its socket shut down and its pending
        // reads cancelled.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap(); // never read from
        let (server_side, _) = listener.accept().unwrap();

        let registry: ConnRegistry = Arc::default();
        registry.register(1, server_side).unwrap();
        let cancel = registry.cancel_flag(1);

        let big = frame(&json!({"padding": "x".repeat(64 * 1024)}));
        let start = Instant::now();
        // The kernel's socket buffer absorbs a few frames before the
        // writer blocks; the queue then fills behind it.
        for _ in 0..1000 {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            registry.send(1, big.clone());
        }
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "queueing a response must never wait on the client"
        );
        assert!(
            cancel.load(Ordering::Relaxed),
            "a non-reading client's connection is dropped once its queue is full"
        );
        // The socket was shut down: the client reads what was flushed, then EOF.
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut sink = Vec::new();
        std::io::Read::read_to_end(&mut &client, &mut sink).unwrap();
    }
}

#[cfg(test)]
mod async_tests;
