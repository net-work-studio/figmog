//! The repository's shared background figmog (NWS-2953): one *owner*
//! process per mirror root holds every mirror of the project open, and
//! every `figmog serve` an agent starts is a thin stdio relay to it.
//!
//! This module is the part both sides share — the owner's election lock,
//! its socket address, the discovery record it publishes, and the
//! handshake every client runs before using the socket:
//!
//! * **Project identity:** the canonical mirror root (inside Git
//!   `<git-common-dir>/figmog`, outside Git `<project>/.figmog`, spec §16),
//!   identical from every worktree of a repository.
//! * **Owner lock:** `<mirror root>/owner.lock`, a permanent file (never
//!   deleted), opened close-on-exec and `flock`ed exclusively by the owner
//!   from before it probes or removes a stale socket until after it has
//!   closed its stores and removed its socket. Whoever holds it is *the*
//!   owner; a process that cannot take it exits without touching anything.
//! * **Socket:** `$XDG_RUNTIME_DIR/figmog/<16 hex>.sock` when that runtime
//!   directory is private and short enough; otherwise
//!   `/tmp/figmog-<uid>/<16 hex>.sock`. The hex is an FNV-1a project hash.
//!   Clients validate the project-specific basename, private runtime directory, socket owner
//!   and connected peer uid before sending any protocol data.
//! * **Discovery:** `<mirror root>/owner.json` (0600, written to a temp file
//!   and renamed), naming the socket, pid, owner generation, binary version
//!   and protocol version. Clients independently authenticate its socket location.
//! * **Handshake:** the first frame on every connection is a
//!   [`HELLO`] request carrying the client's project identity, binary
//!   version and protocol version; the owner refuses a mismatch with
//!   [`HANDSHAKE_REFUSED`] and the client independently checks the owner's
//!   answer, so neither side silently talks to the wrong owner.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

/// Bumped whenever the relay/owner wire contract changes incompatibly.
pub(crate) const PROTOCOL: u64 = 1;
/// The handshake method, the first frame of every owner connection.
pub(crate) const HELLO: &str = "figmog/hello";
/// The JSON-RPC id clients use for their handshake.
pub(crate) const HELLO_ID: &str = "figmog-hello";
/// JSON-RPC error code: the owner is shutting down and did not accept this
/// request. Safe to retry on a new connection (which may start a new
/// owner); nothing was started.
pub(crate) const NOT_ACCEPTED: i64 = -32001;
/// JSON-RPC error code: the handshake was refused (wrong project, binary
/// version or protocol version).
pub(crate) const HANDSHAKE_REFUSED: i64 = -32002;
/// How long an owner with no clients and no work waits before exiting.
pub(crate) const DEFAULT_IDLE: Duration = Duration::from_secs(600);
/// Overrides [`DEFAULT_IDLE`], in milliseconds (tests use short ones).
pub(crate) const IDLE_ENV: &str = "FIGMOG_OWNER_IDLE_MS";

const LOCK_FILE: &str = "owner.lock";
const DISCOVERY_FILE: &str = "owner.json";
const LOG_FILE: &str = "owner.log";
/// The owner's log is truncated before a launch once it exceeds this.
const LOG_LIMIT: u64 = 1 << 20;
/// Bounds every handshake read: an owner answers its hello from the loop,
/// after opening any startup files the client asked for.
const HELLO_TIMEOUT: Duration = Duration::from_secs(60);

/// This binary's version, as compared in the handshake.
pub(crate) fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions or side effects.
    unsafe { libc::geteuid() }
}

/// The idle timeout this owner uses: [`IDLE_ENV`] or [`DEFAULT_IDLE`].
pub(crate) fn idle_timeout() -> Duration {
    std::env::var(IDLE_ENV)
        .ok()
        .and_then(|ms| ms.trim().parse().ok())
        .map_or(DEFAULT_IDLE, Duration::from_millis)
}

/// 64-bit FNV-1a: tiny, stable across platforms and releases (unlike
/// `std`'s `DefaultHasher`), and plenty to key one user's projects.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Use the session's private runtime directory when it is safe and short
/// enough for a Unix socket. Invalid/missing XDG directories use /tmp.
pub(crate) fn runtime_dir() -> PathBuf {
    runtime_dir_for(std::env::var_os("XDG_RUNTIME_DIR").as_deref())
}

fn runtime_dir_for(xdg: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(xdg) = xdg {
        let root = Path::new(xdg);
        let candidate = root.join("figmog");
        if root.is_absolute()
            && candidate.join("0000000000000000.sock").as_os_str().len() < 104
            && std::fs::symlink_metadata(root)
                .is_ok_and(|m| m.is_dir() && m.uid() == euid() && m.mode() & 0o777 == 0o700)
            && crate::private_dir::existing(root).is_ok_and(|root| root.is_some())
        {
            return candidate;
        }
    }
    PathBuf::from(format!("/tmp/figmog-{}", euid()))
}

fn check_runtime_directory(dir: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("checking owner socket directory {}: {e}", dir.display()))?;
    if !metadata.is_dir() || metadata.uid() != euid() || metadata.mode() & 0o777 != 0o700 {
        return Err(format!(
            "unsafe owner socket directory {}: must be a real directory owned by the current user \
             with mode 0700; use a private XDG_RUNTIME_DIR or ask its owner/administrator to remove it",
            dir.display()
        ));
    }
    crate::private_dir::existing(dir)?;
    Ok(())
}

/// Authenticate the connected endpoint independently of pathname metadata.
fn check_peer(stream: &UnixStream) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let uid = {
        let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&credential) as libc::socklen_t;
        // SAFETY: credential and len point to writable, correctly sized storage.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credential).cast(),
                &raw mut len,
            )
        } != 0
        {
            return Err(format!(
                "checking owner socket peer: {}",
                std::io::Error::last_os_error()
            ));
        }
        credential.uid
    };
    #[cfg(not(target_os = "linux"))]
    let uid = {
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: both pointers refer to writable uid/gid storage.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) } != 0 {
            return Err(format!(
                "checking owner socket peer: {}",
                std::io::Error::last_os_error()
            ));
        }
        uid
    };
    if uid != euid() {
        return Err("refusing owner socket: peer is not the current user".into());
    }
    Ok(())
}

// The handler performs one lock-free atomic store; the serve loop checks it
// on every wake (at most two seconds), then runs its ordinary drain/cleanup.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
extern "C" fn request_shutdown(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

pub(crate) fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

pub(crate) struct ShutdownSignals {
    term: libc::sigaction,
    interrupt: libc::sigaction,
}

impl ShutdownSignals {
    pub(crate) fn install() -> Result<Self, String> {
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
        // SAFETY: the zeroed actions are initialized before registration; the
        // handler uses only a lock-free atomic and never touches Rust I/O.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = request_shutdown as *const () as usize;
            libc::sigemptyset(&raw mut action.sa_mask);
            let mut term = std::mem::zeroed();
            let mut interrupt = std::mem::zeroed();
            if libc::sigaction(libc::SIGTERM, &action, &raw mut term) != 0 {
                return Err(format!(
                    "installing SIGTERM handler: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if libc::sigaction(libc::SIGINT, &action, &raw mut interrupt) != 0 {
                libc::sigaction(libc::SIGTERM, &term, std::ptr::null_mut());
                return Err(format!(
                    "installing SIGINT handler: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self { term, interrupt })
        }
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        // SAFETY: restoring the actions returned by sigaction above.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.term, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.interrupt, std::ptr::null_mut());
        }
    }
}

/// Where one project's owner listens.
pub(crate) fn socket_for(project: &Path) -> PathBuf {
    runtime_dir().join(socket_name(project))
}

fn socket_name(project: &Path) -> String {
    format!("{:016x}.sock", fnv1a(project.as_os_str().as_bytes()))
}

/// A published address may come from another session's XDG runtime
/// directory. Authenticate its independently checked location rather than
/// requiring it to match this client's environment. `false` is stale/missing.
fn validate_discovery_socket(paths: &OwnerPaths, socket: &Path) -> Result<bool, String> {
    let expected_name = socket_name(&paths.project);
    if !socket.is_absolute()
        || socket.as_os_str().len() >= 104
        || socket.file_name() != Some(std::ffi::OsStr::new(&expected_name))
    {
        return Err(format!(
            "refusing owner discovery {}: socket {} does not match expected project socket {expected_name}",
            paths.discovery().display(),
            socket.display()
        ));
    }
    let dir = socket.parent().ok_or("owner socket has no directory")?;
    if dir != runtime_dir_for(None) {
        if dir.file_name() != Some(std::ffi::OsStr::new("figmog")) {
            return Err(format!(
                "refusing owner socket {}: not a supported runtime directory",
                socket.display()
            ));
        }
        let runtime = dir
            .parent()
            .ok_or("owner runtime directory has no parent")?;
        match std::fs::symlink_metadata(runtime) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            _ => check_runtime_directory(runtime)?,
        }
    }
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        _ => check_runtime_directory(dir)?,
    }
    match std::fs::symlink_metadata(socket) {
        Ok(meta) if !meta.file_type().is_socket() || meta.uid() != euid() => Err(format!(
            "refusing unsafe owner socket {}: must be a socket owned by the current user with mode 0600",
            socket.display()
        )),
        // A successor may have bound this pathname but not chmodded it
        // yet while a crashed owner's discovery record still names it.
        // Never connect until mode 0600; let the lock/probe retry await it.
        Ok(meta) if meta.mode() & 0o777 != 0o600 => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("checking owner socket {}: {e}", socket.display())),
        Ok(_) => Ok(true),
    }
}

/// One project's owner files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnerPaths {
    /// The canonical mirror root: the project identity.
    pub(crate) project: PathBuf,
}

impl OwnerPaths {
    /// The paths for `layout`'s mirror root if it exists (clients never
    /// create it); `None` means no owner can be running.
    pub(crate) fn existing(mirror_root: &Path) -> Option<Self> {
        let project = std::fs::canonicalize(mirror_root).ok()?;
        Some(Self { project })
    }

    /// The paths for `mirror_root`, creating it privately (0700) if needed.
    pub(crate) fn ensure(mirror_root: &Path) -> Result<Self, String> {
        let resolved = crate::private_dir::ensure(mirror_root)?;
        let project = std::fs::canonicalize(&resolved)
            .map_err(|e| format!("resolving {}: {e}", resolved.display()))?;
        Ok(Self { project })
    }

    pub(crate) fn lock(&self) -> PathBuf {
        self.project.join(LOCK_FILE)
    }

    pub(crate) fn discovery(&self) -> PathBuf {
        self.project.join(DISCOVERY_FILE)
    }

    pub(crate) fn log(&self) -> PathBuf {
        self.project.join(LOG_FILE)
    }

    pub(crate) fn socket(&self) -> PathBuf {
        socket_for(&self.project)
    }

    pub(crate) fn identity(&self) -> String {
        self.project.display().to_string()
    }
}

// ---- the owner lock ----

/// The held owner lock. Dropping it (or the process exiting, even by a
/// crash) releases it; the file itself is never deleted.
pub(crate) struct OwnerLock {
    file: File,
    path: PathBuf,
}

impl OwnerLock {
    /// Take the project's owner lock without blocking. `Ok(None)` when
    /// another process holds it (it is, or is about to be, the owner).
    pub(crate) fn try_acquire(paths: &OwnerPaths) -> Result<Option<Self>, String> {
        let path = paths.lock();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| format!("opening owner lock {}: {e}", path.display()))?;
        let metadata = file
            .metadata()
            .map_err(|e| format!("reading owner lock {}: {e}", path.display()))?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != euid() {
            return Err(format!(
                "owner lock {} must be a regular, unshared file owned by the current user",
                path.display()
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file, path })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => {
                Err(format!("locking owner lock {}: {e}", path.display()))
            }
        }
    }

    /// The lock file at its path is still the one this process locked:
    /// if it was deleted or replaced, another process could lock the new
    /// file and become a second owner, so the holder must stop.
    pub(crate) fn still_valid(&self) -> bool {
        let (Ok(held), Ok(named)) = (self.file.metadata(), std::fs::symlink_metadata(&self.path))
        else {
            return false;
        };
        held.dev() == named.dev() && held.ino() == named.ino()
    }
}

// ---- the socket ----

/// Bind the owner socket at `socket`, under the caller's held
/// [`OwnerLock`]. A file already there is a leftover of this project's
/// previous owner (the lock excludes a live one): it is removed only after
/// a connect fails and it proves to be a socket owned by this user.
pub(crate) fn bind_socket(socket: &Path) -> Result<UnixListener, String> {
    let dir = socket.parent().ok_or("owner socket has no directory")?;
    if let Ok(metadata) = std::fs::symlink_metadata(dir)
        && (!metadata.is_dir() || metadata.uid() != euid())
    {
        check_runtime_directory(dir)?;
    }
    crate::private_dir::ensure(dir)
        .map_err(|e| format!("preparing the owner socket directory: {e}; use a private XDG_RUNTIME_DIR or ask the administrator to remove a foreign runtime directory"))?;
    check_runtime_directory(dir)?;
    if let Ok(meta) = std::fs::symlink_metadata(socket) {
        if UnixStream::connect(socket).is_ok() {
            return Err(format!(
                "another process is listening at {} although this project's owner lock is free; \
                 stop it and retry",
                socket.display()
            ));
        }
        if !meta.file_type().is_socket() || meta.uid() != euid() {
            return Err(format!(
                "refusing to remove {}: not a socket owned by the current user",
                socket.display()
            ));
        }
        std::fs::remove_file(socket)
            .map_err(|e| format!("removing stale socket {}: {e}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket)
        .map_err(|e| format!("binding socket {}: {e}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("protecting socket {}: {e}", socket.display()))?;
    Ok(listener)
}

// ---- discovery ----

/// The owner's published record (`owner.json`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Discovery {
    pub(crate) socket: PathBuf,
    pub(crate) pid: u32,
    pub(crate) generation: String,
    pub(crate) version: String,
    pub(crate) protocol: u64,
}

impl Discovery {
    fn to_json(&self, paths: &OwnerPaths) -> Value {
        json!({
            "generation": self.generation,
            "pid": self.pid,
            "project": paths.identity(),
            "protocol": self.protocol,
            "socket": self.socket.display().to_string(),
            "version": self.version,
        })
    }
}

/// The owner's record, if one is published and readable.
pub(crate) fn read_discovery(paths: &OwnerPaths) -> Option<Discovery> {
    let text = std::fs::read_to_string(paths.discovery()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(Discovery {
        socket: PathBuf::from(value["socket"].as_str()?),
        pid: u32::try_from(value["pid"].as_u64()?).ok()?,
        generation: value["generation"].as_str()?.to_string(),
        version: value["version"].as_str()?.to_string(),
        protocol: value["protocol"].as_u64()?,
    })
}

/// Publish `discovery` atomically (temp file, then rename), owner-only.
pub(crate) fn write_discovery(paths: &OwnerPaths, discovery: &Discovery) -> Result<(), String> {
    let target = paths.discovery();
    let temp = paths
        .project
        .join(format!(".{DISCOVERY_FILE}.{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(discovery.to_json(paths).to_string().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &target)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("publishing {}: {e}", target.display())
    })
}

/// Remove the record if it is still this owner's (`generation`).
pub(crate) fn remove_discovery(paths: &OwnerPaths, generation: &str) {
    if read_discovery(paths).is_some_and(|d| d.generation == generation) {
        let _ = std::fs::remove_file(paths.discovery());
    }
}

// ---- settings ----

/// Watch, poll and desktop settings: the owner runs with the ones of the
/// relay that started it; a later relay asking for others gets a note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) watch: bool,
    pub(crate) interval: u64,
    /// The desktop MCP URL, or `None` for `--no-upstream`.
    pub(crate) upstream: Option<String>,
}

impl Settings {
    pub(crate) fn to_json(&self) -> Value {
        json!({"interval": self.interval, "upstream": self.upstream, "watch": self.watch})
    }

    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            watch: value["watch"].as_bool()?,
            interval: value["interval"].as_u64()?,
            upstream: match &value["upstream"] {
                Value::Null => None,
                Value::String(url) => Some(url.clone()),
                _ => return None,
            },
        })
    }

    /// The `figmog serve --background` arguments that reproduce these.
    pub(crate) fn args(&self) -> Vec<String> {
        let mut args = vec!["--interval".to_string(), self.interval.to_string()];
        if !self.watch {
            args.push("--no-watch".into());
        }
        match &self.upstream {
            Some(url) => args.extend(["--upstream".to_string(), url.clone()]),
            None => args.push("--no-upstream".into()),
        }
        args
    }
}

/// One stderr note per setting `relay` asked for that the running
/// `owner` does not use (sorted by setting name).
pub(crate) fn settings_notes(relay: &Settings, owner: &Settings, pid: u32) -> Vec<String> {
    let on = |b: bool| if b { "on" } else { "off" };
    let upstream = |u: &Option<String>| u.clone().unwrap_or_else(|| "off".to_string());
    let mut notes = Vec::new();
    if relay.interval != owner.interval {
        notes.push((
            "interval",
            format!("{}s", relay.interval),
            format!("{}s", owner.interval),
        ));
    }
    if relay.upstream != owner.upstream {
        notes.push((
            "upstream",
            upstream(&relay.upstream),
            upstream(&owner.upstream),
        ));
    }
    if relay.watch != owner.watch {
        notes.push(("watch", on(relay.watch).into(), on(owner.watch).into()));
    }
    notes
        .into_iter()
        .map(|(name, asked, used)| {
            format!(
                "figmog: note: {name} {asked} was not applied — this repository's shared background \
                 figmog (pid {pid}) runs with {name} {used}; its settings come from the `figmog serve` \
                 that started it and change when it restarts after idling out"
            )
        })
        .collect()
}

// ---- the handshake ----

/// The handshake a client sends first. `relay` connections get MCP
/// framing (tool errors as `isError` content, no internal tools), exactly
/// like stdio; `attach` names files to open as startup files.
pub(crate) fn hello_request(paths: &OwnerPaths, relay: Option<(&Settings, &[String])>) -> Value {
    let mut params = json!({
        "client": if relay.is_some() { "relay" } else { "cli" },
        "project": paths.identity(),
        "protocol": PROTOCOL,
        "version": version(),
    });
    if let Some((settings, attach)) = relay {
        params["settings"] = settings.to_json();
        params["attach"] = json!(attach);
    }
    json!({"jsonrpc": "2.0", "id": HELLO_ID, "method": HELLO, "params": params})
}

/// Why two figmog builds cannot share an owner.
pub(crate) fn mismatch_message(owner_version: &str, owner_protocol: u64, pid: u32) -> String {
    format!(
        "this repository's shared background figmog (pid {pid}) runs figmog {owner_version} \
         (protocol {owner_protocol}), but this is figmog {} (protocol {PROTOCOL}); they cannot \
         share it — let it exit (it stops on its own after it has had no clients for a while), \
         or close its attached `figmog serve` relays, stop it with `kill {pid}`, then retry",
        version()
    )
}

/// Owner side: check a client's hello params against this owner.
pub(crate) fn check_hello(params: &Value, paths: &OwnerPaths) -> Result<(), String> {
    let project = params["project"].as_str().unwrap_or_default();
    if project != paths.identity() {
        return Err(format!(
            "this figmog owner serves {}, not {project}",
            paths.identity()
        ));
    }
    let client_version = params["version"].as_str().unwrap_or_default();
    let client_protocol = params["protocol"].as_u64().unwrap_or_default();
    if client_version != version() || client_protocol != PROTOCOL {
        return Err(format!(
            "this repository's shared background figmog (pid {}) runs figmog {} (protocol \
             {PROTOCOL}), but the client is figmog {client_version} (protocol {client_protocol}); \
             they cannot share it — let it exit (it stops on its own after it has had no \
             clients for a while), or close its attached `figmog serve` relays, stop it with `kill {}`, then retry",
            std::process::id(),
            version(),
            std::process::id()
        ));
    }
    Ok(())
}

/// What the owner told a client in its handshake answer.
#[derive(Clone, Debug)]
pub(crate) struct OwnerInfo {
    pub(crate) pid: u32,
    pub(crate) generation: String,
    pub(crate) settings: Option<Settings>,
    /// Startup files the owner could not open, by key (sorted).
    pub(crate) attach_errors: Vec<(String, String)>,
}

/// A handshaken connection to the owner.
pub(crate) struct OwnerConn {
    pub(crate) reader: BufReader<UnixStream>,
    pub(crate) writer: UnixStream,
    pub(crate) info: OwnerInfo,
}

/// What a client found when it looked for the owner.
pub(crate) enum Probe {
    /// Connected and handshaken.
    Ready(OwnerConn),
    /// Nothing is listening: no owner (or one that crashed).
    Absent,
    /// An owner is shutting down; a new one can start once it has exited.
    Closing,
}

/// Look for `paths`' owner and handshake with it. `Err` is final: the
/// owner refused this client (or is a different figmog build).
pub(crate) fn probe(
    paths: &OwnerPaths,
    relay: Option<(&Settings, &[String])>,
) -> Result<Probe, String> {
    let Some(discovery) = read_discovery(paths) else {
        return Ok(Probe::Absent);
    };
    if !validate_discovery_socket(paths, &discovery.socket)? {
        return Ok(Probe::Absent);
    }
    let Ok(stream) = UnixStream::connect(&discovery.socket) else {
        return Ok(Probe::Absent);
    };
    check_peer(&stream)?;
    let io = |e: std::io::Error| format!("talking to the background figmog: {e}");
    stream.set_read_timeout(Some(HELLO_TIMEOUT)).map_err(io)?;
    stream.set_write_timeout(Some(HELLO_TIMEOUT)).map_err(io)?;
    let mut writer = stream.try_clone().map_err(io)?;
    let mut reader = BufReader::new(stream);
    let hello = hello_request(paths, relay);
    // A connection that is accepted and then closed without an answer is
    // an owner past its last request, still closing its stores: wait. (A
    // dead one refuses the next connect, which reads as `Absent`.)
    if writeln!(writer, "{hello}")
        .and_then(|()| writer.flush())
        .is_err()
    {
        return Ok(Probe::Closing);
    }
    let response = loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Ok(Probe::Closing),
            Ok(_) => {}
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            return Err("the background figmog sent an unreadable handshake".into());
        };
        if frame.get("method").is_none() && frame["id"] == HELLO_ID {
            break frame;
        }
    };
    if let Some(error) = response.get("error") {
        return match error["code"].as_i64() {
            Some(NOT_ACCEPTED) => Ok(Probe::Closing),
            Some(HANDSHAKE_REFUSED) => Err(error["message"]
                .as_str()
                .unwrap_or("handshake refused")
                .to_string()),
            _ => Err(mismatch_message(
                &discovery.version,
                discovery.protocol,
                discovery.pid,
            )),
        };
    }
    let result = &response["result"];
    let owner_version = result["version"].as_str().unwrap_or_default();
    let owner_protocol = result["protocol"].as_u64().unwrap_or_default();
    let pid = result["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .unwrap_or(discovery.pid);
    if owner_version != version() || owner_protocol != PROTOCOL {
        return Err(mismatch_message(owner_version, owner_protocol, pid));
    }
    if result["project"].as_str() != Some(paths.identity().as_str()) {
        return Err(format!(
            "the socket named in {} belongs to another project ({}); remove that file and retry",
            paths.discovery().display(),
            result["project"]
        ));
    }
    let mut attach_errors: Vec<(String, String)> = result["attach_errors"]
        .as_object()
        .map(|errors| {
            errors
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    attach_errors.sort();
    let info = OwnerInfo {
        pid,
        generation: result["generation"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        settings: Settings::from_json(&result["settings"]),
        attach_errors,
    };
    let stream = reader.get_ref();
    stream.set_read_timeout(None).map_err(io)?;
    stream.set_write_timeout(None).map_err(io)?;
    Ok(Probe::Ready(OwnerConn {
        reader,
        writer,
        info,
    }))
}

/// Wait (up to `timeout`) for a closing owner to go away, then probe
/// again: the next owner, if a relay started one, or `Absent`.
pub(crate) fn probe_after_close(
    paths: &OwnerPaths,
    relay: Option<(&Settings, &[String])>,
    timeout: Duration,
) -> Result<Probe, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match probe(paths, relay)? {
            Probe::Closing if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Probe::Closing => {
                return Err(
                    "the background figmog is still shutting down; retry in a moment".into(),
                );
            }
            other => return Ok(other),
        }
    }
}

// ---- launching ----

/// Open (append) the owner log for a newly launched owner's stderr,
/// truncating it first once it has grown past [`LOG_LIMIT`].
fn open_log(paths: &OwnerPaths) -> Result<File, String> {
    let path = paths.log();
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    if file.metadata().is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = file.set_len(0);
    }
    Ok(file)
}

/// Start `figmog serve --background <args>` detached: its own session (so
/// closing the agent's terminal or process group never signals it), stdin
/// and stdout on `/dev/null`, stderr appended to the owner log, working
/// directory `cwd`. Returns a channel that receives its exit status (a
/// thread reaps it, so it never lingers as a zombie of the relay).
pub(crate) fn launch(
    paths: &OwnerPaths,
    cwd: &Path,
    args: &[String],
) -> Result<std::sync::mpsc::Receiver<std::process::ExitStatus>, String> {
    #[cfg(target_os = "macos")]
    if let Some(addchdir) = macos_spawn::addchdir() {
        return macos_spawn::launch(paths, cwd, args, addchdir)
            .map_err(|e| format!("starting the background figmog: {e}"));
    }
    launch_command(paths, cwd, args)
}

// Linux uses close_range(CLOEXEC); older platforms without an atomic
// close-on-exec spawn facility retain the bounded descriptor-limit scan.
fn launch_command(
    paths: &OwnerPaths,
    cwd: &Path,
    args: &[String],
) -> Result<std::sync::mpsc::Receiver<std::process::ExitStatus>, String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe =
        std::env::current_exe().map_err(|e| format!("locating the figmog executable: {e}"))?;
    let log = open_log(paths)?;
    // Compute the bound before fork; only async-signal-safe fcntl calls run
    // in pre_exec. CLOEXEC preserves Rust's exec-error pipe until exec itself.
    let fd_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if fd_limit < 0 || fd_limit > i64::from(libc::c_int::MAX) {
        return Err("cannot determine the inherited descriptor limit".into());
    }
    let fd_limit = fd_limit as libc::c_int;
    let mut command = Command::new(exe);
    command
        .args(["serve", "--background"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // SAFETY: setsid and fcntl are async-signal-safe. The child detaches
    // from the relay's session and marks inherited fds close-on-exec;
    // stdio remains and Rust's error pipe stays usable if exec fails.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            // CLOSE_RANGE_CLOEXEC (4) marks without closing the exec-error
            // pipe. An old kernel can fall back to the portable fcntl loop.
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) == 0 {
                return Ok(());
            }
            for fd in 3..fd_limit {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("starting the background figmog: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(status) = child.wait() {
            let _ = tx.send(status);
        }
    });
    Ok(rx)
}

// Darwin's documented spawn flags perform descriptor isolation atomically,
// avoiding SC_OPEN_MAX fcntl calls (commonly a million) for every owner.
#[cfg(target_os = "macos")]
mod macos_spawn {
    use super::*;
    use std::ffi::CString;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::FromRawFd;
    use std::os::unix::process::ExitStatusExt;

    pub(super) type AddChdir = unsafe extern "C" fn(
        *mut libc::posix_spawn_file_actions_t,
        *const libc::c_char,
    ) -> libc::c_int;

    pub(super) fn addchdir() -> Option<AddChdir> {
        // The public _np entry point exists since macOS 10.15. Resolve it
        // dynamically so older systems can use the portable launcher.
        // SAFETY: dlsym returns the documented C function with this signature.
        unsafe {
            let symbol = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"posix_spawn_file_actions_addchdir_np".as_ptr(),
            );
            if symbol.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*mut libc::c_void, AddChdir>(symbol))
            }
        }
    }

    fn check(code: libc::c_int) -> io::Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    }

    fn cstring(bytes: impl Into<Vec<u8>>) -> io::Result<CString> {
        CString::new(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in owner launch path, argument or environment",
            )
        })
    }

    struct Spawn {
        attrs: libc::posix_spawnattr_t,
        actions: libc::posix_spawn_file_actions_t,
    }

    impl Spawn {
        fn new() -> io::Result<Self> {
            // SAFETY: init initializes each object before use; partial
            // initialization is destroyed explicitly before returning.
            unsafe {
                let mut attrs = MaybeUninit::uninit();
                check(libc::posix_spawnattr_init(attrs.as_mut_ptr()))?;
                let mut attrs = attrs.assume_init();
                let mut actions = MaybeUninit::uninit();
                if let Err(error) = check(libc::posix_spawn_file_actions_init(actions.as_mut_ptr()))
                {
                    libc::posix_spawnattr_destroy(&raw mut attrs);
                    return Err(error);
                }
                Ok(Self {
                    attrs,
                    actions: actions.assume_init(),
                })
            }
        }
    }

    impl Drop for Spawn {
        fn drop(&mut self) {
            // SAFETY: both objects were initialized by Spawn::new.
            unsafe {
                libc::posix_spawn_file_actions_destroy(&raw mut self.actions);
                libc::posix_spawnattr_destroy(&raw mut self.attrs);
            }
        }
    }

    pub(super) fn launch(
        paths: &OwnerPaths,
        cwd: &Path,
        args: &[String],
        addchdir: AddChdir,
    ) -> io::Result<std::sync::mpsc::Receiver<std::process::ExitStatus>> {
        let exe = std::env::current_exe()?;
        let exe = cstring(exe.as_os_str().as_bytes())?;
        let cwd = cstring(cwd.as_os_str().as_bytes())?;
        let mut argv = vec![exe.clone(), c"serve".into(), c"--background".into()];
        for arg in args {
            argv.push(cstring(arg.as_bytes())?);
        }
        // Snapshot through std's environment lock; never pass a racing
        // process-global environ pointer to libc. Keep all backing strings
        // alive until posix_spawn returns.
        let environment: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
        let envp: Vec<CString> = environment
            .into_iter()
            .map(|(key, value)| {
                let mut bytes = key.as_bytes().to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_bytes());
                cstring(bytes)
            })
            .collect::<io::Result<_>>()?;
        let pointers = |strings: &[CString]| {
            strings
                .iter()
                .map(|s| s.as_ptr().cast_mut())
                .chain(std::iter::once(std::ptr::null_mut()))
                .collect::<Vec<_>>()
        };
        let argv_ptrs = pointers(&argv);
        let envp_ptrs = pointers(&envp);
        let log = super::open_log(paths).map_err(io::Error::other)?;
        let log = if log.as_raw_fd() < 3 {
            // A relay may have inherited a closed standard descriptor.
            // Do not let the /dev/null actions overwrite the log source.
            // SAFETY: duplicate a live File, then transfer the new fd to File.
            let fd = unsafe { libc::fcntl(log.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            unsafe { File::from_raw_fd(fd) }
        } else {
            log
        };
        let mut spawn = Spawn::new()?;
        let mut pid = 0;
        // SAFETY: the initialized spawn objects and NUL-terminated vectors
        // outlive this call. File actions explicitly preserve only 0/1/2;
        // CLOEXEC_DEFAULT closes all other descriptors atomically, including
        // concurrent opens. posix_spawn returns exec/setup errors directly.
        unsafe {
            // POSIX_SPAWN_SETSID is 0x0400 in Darwin's public sys/spawn.h;
            // libc exposes CLOEXEC_DEFAULT but does not name SETSID there.
            const SETSID: libc::c_int = 0x0400;
            check(libc::posix_spawnattr_setflags(
                &raw mut spawn.attrs,
                (libc::POSIX_SPAWN_CLOEXEC_DEFAULT | SETSID | libc::POSIX_SPAWN_SETSIGDEF)
                    as libc::c_short,
            ))?;
            let mut defaults = std::mem::zeroed();
            if libc::sigemptyset(&raw mut defaults) != 0
                || libc::sigaddset(&raw mut defaults, libc::SIGPIPE) != 0
            {
                return Err(io::Error::last_os_error());
            }
            check(libc::posix_spawnattr_setsigdefault(
                &raw mut spawn.attrs,
                &defaults,
            ))?;
            check(libc::posix_spawn_file_actions_addopen(
                &raw mut spawn.actions,
                0,
                c"/dev/null".as_ptr(),
                libc::O_RDONLY,
                0,
            ))?;
            check(libc::posix_spawn_file_actions_addopen(
                &raw mut spawn.actions,
                1,
                c"/dev/null".as_ptr(),
                libc::O_WRONLY,
                0,
            ))?;
            check(libc::posix_spawn_file_actions_adddup2(
                &raw mut spawn.actions,
                log.as_raw_fd(),
                2,
            ))?;
            check(libc::posix_spawn_file_actions_addclose(
                &raw mut spawn.actions,
                log.as_raw_fd(),
            ))?;
            check(addchdir(&raw mut spawn.actions, cwd.as_ptr()))?;
            check(libc::posix_spawn(
                &raw mut pid,
                exe.as_ptr(),
                &spawn.actions,
                &spawn.attrs,
                argv_ptrs.as_ptr(),
                envp_ptrs.as_ptr(),
            ))?;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut status = 0;
            loop {
                // SAFETY: pid is the child returned by posix_spawn; status
                // points to writable storage and this thread alone reaps it.
                if unsafe { libc::waitpid(pid, &raw mut status, 0) } != -1 {
                    let _ = tx.send(std::process::ExitStatus::from_raw(status));
                    break;
                }
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        });
        Ok(rx)
    }
}

/// How long a relay waits for an owner to become ready, including an old
/// one finishing its shutdown (its drain is bounded at 15 s) first.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(60);

/// Connect to the project's owner, launching one if none is running.
/// Concurrent callers may all launch; the owner lock lets exactly one win
/// and the rest exit at once, and every caller connects to the winner.
pub(crate) fn connect_or_launch(
    paths: &OwnerPaths,
    cwd: &Path,
    settings: &Settings,
    attach: &[String],
) -> Result<OwnerConn, String> {
    connect_or_launch_with_timeout(paths, cwd, settings, attach, START_TIMEOUT)
}

fn connect_or_launch_with_timeout(
    paths: &OwnerPaths,
    cwd: &Path,
    settings: &Settings,
    attach: &[String],
    timeout: Duration,
) -> Result<OwnerConn, String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut launched: Option<std::sync::mpsc::Receiver<std::process::ExitStatus>> = None;
    let mut delay = Duration::from_millis(25);
    loop {
        let mut unreachable_lock_holder = false;
        match probe(paths, Some((settings, attach)))? {
            Probe::Ready(conn) => return Ok(conn),
            Probe::Absent if launched.is_none() => {
                // Do not spawn a losing process every time an owner holds
                // the election lock but cannot yet publish/reach its socket.
                match OwnerLock::try_acquire(paths)? {
                    Some(lock) => {
                        drop(lock);
                        launched = Some(launch(paths, cwd, &settings.args())?);
                    }
                    None => {
                        // Desktop initialization happens before discovery
                        // is published. Give this owner the full startup
                        // budget, just like one launched by this relay.
                        unreachable_lock_holder = true;
                    }
                }
            }
            Probe::Absent | Probe::Closing => {}
        }
        // A launched owner that already exited either lost the election
        // (status 0: another owner holds or held the lock — keep probing,
        // and launch again if that one goes away) or failed (report why).
        if let Some(rx) = &launched
            && let Ok(status) = rx.try_recv()
        {
            if !status.success() {
                // The shared log can contain other concurrent launches;
                // never attribute an arbitrary tail to this child.
                return Err(format!(
                    "the background figmog failed to start ({status}); see {}",
                    paths.log().display()
                ));
            }
            launched = None;
        }
        if std::time::Instant::now() >= deadline {
            if unreachable_lock_holder {
                return Err(format!(
                    "background figmog owner lock is held but its socket is unreachable; \
                     wait for startup or shutdown, and check whether the processes use \
                     different /tmp namespaces or XDG_RUNTIME_DIR values; see {}",
                    paths.log().display()
                ));
            }
            return Err(format!(
                "the background figmog did not become ready within {}s; see {}",
                timeout.as_secs(),
                paths.log().display()
            ));
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_addresses_are_short_stable_and_per_project() {
        let a = socket_for(Path::new("/home/someone/repo/.git/figmog"));
        let b = socket_for(Path::new("/home/someone/other/.git/figmog"));
        assert_ne!(a, b);
        assert_eq!(a, socket_for(Path::new("/home/someone/repo/.git/figmog")));
        let deep = socket_for(&PathBuf::from("/x".repeat(2000)));
        assert!(deep.as_os_str().len() < 104, "{}", deep.display());
        assert!(a.starts_with(runtime_dir()));
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn xdg_runtime_requires_private_real_short_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("run");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(runtime_dir_for(Some(root.as_os_str())), root.join("figmog"));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(
            runtime_dir_for(Some(root.as_os_str())),
            runtime_dir_for(None)
        );
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&root, &linked).unwrap();
        assert_eq!(
            runtime_dir_for(Some(linked.as_os_str())),
            runtime_dir_for(None)
        );
        let long = dir.path().join("x".repeat(110));
        std::fs::create_dir(&long).unwrap();
        std::fs::set_permissions(&long, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            runtime_dir_for(Some(long.as_os_str())),
            runtime_dir_for(None)
        );
    }

    #[test]
    fn a_client_refuses_discovery_pointing_elsewhere_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(dir.path()).unwrap();
        let socket = dir.path().join("impostor.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        write_discovery(
            &paths,
            &Discovery {
                socket,
                pid: 1,
                generation: "evil".into(),
                version: version().into(),
                protocol: PROTOCOL,
            },
        )
        .unwrap();
        let error = match probe(&paths, None) {
            Err(error) => error,
            _ => panic!("untrusted discovery was accepted"),
        };
        assert!(error.contains("does not match expected"), "{error}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn a_client_reaches_an_owner_started_with_a_different_runtime_environment() {
        let project = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(project.path()).unwrap();
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket =
            runtime_dir_for(Some(runtime.path().as_os_str())).join(socket_name(&paths.project));
        assert_ne!(
            socket,
            paths.socket(),
            "the owner and client must select different runtime roots"
        );
        let listener = bind_socket(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        write_discovery(
            &paths,
            &Discovery {
                socket: socket.clone(),
                pid: 42,
                generation: "other-runtime".into(),
                version: version().into(),
                protocol: PROTOCOL,
            },
        )
        .unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let identity = paths.identity();
        let owner =
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("waiting for owner hello: {error}"),
                    }
                };
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let hello: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(hello["params"]["project"], identity);
                writeln!(stream, "{}", json!({
                "jsonrpc": "2.0", "id": HELLO_ID, "result": {
                    "pid": 42, "project": identity, "protocol": PROTOCOL,
                    "version": version(), "generation": "other-runtime", "attach_errors": {},
                }
            })).unwrap();
                stream.flush().unwrap();
                let _ = release_rx.recv();
            });
        // The probe authenticates the published address independently of
        // this process's fallback or differently configured XDG selection.
        let result = probe(&paths, None);
        release_tx.send(()).unwrap();
        owner.join().unwrap();
        match result {
            Ok(Probe::Ready(conn)) => assert_eq!(conn.info.generation, "other-runtime"),
            Err(error) => panic!("a safe owner in another runtime root was rejected: {error}"),
            _ => panic!("a safe owner in another runtime root was not reached"),
        }
    }

    #[test]
    fn discovery_rejects_unsafe_runtime_roots_even_with_the_correct_project_basename() {
        let project = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(project.path()).unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let name = socket_name(&paths.project);
        let arbitrary = runtime.path().join(&name);
        assert!(
            validate_discovery_socket(&paths, &arbitrary)
                .unwrap_err()
                .contains("not a supported runtime directory")
        );
        let socket = runtime.path().join("figmog").join(&name);
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            validate_discovery_socket(&paths, &socket)
                .unwrap_err()
                .contains("mode 0700")
        );
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), runtime.path().join("figmog")).unwrap();
        assert!(validate_discovery_socket(&paths, &socket).is_err());
    }

    #[test]
    fn a_socket_not_yet_protected_is_not_contacted_and_can_be_probed_again() {
        let project = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(project.path()).unwrap();
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir = runtime.path().join("figmog");
        crate::private_dir::ensure(&dir).unwrap();
        let socket = dir.join(socket_name(&paths.project));
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_discovery(
            &paths,
            &Discovery {
                socket: socket.clone(),
                pid: 1,
                generation: "stale".into(),
                version: version().into(),
                protocol: PROTOCOL,
            },
        )
        .unwrap();
        assert!(matches!(probe(&paths, None), Ok(Probe::Absent)));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validate_discovery_socket(&paths, &socket).unwrap());
    }

    #[test]
    fn runtime_directory_rejects_symlinks_and_broad_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("run");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            check_runtime_directory(&root)
                .unwrap_err()
                .contains("mode 0700")
        );
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(dir.path(), &linked).unwrap();
        assert!(check_runtime_directory(&linked).is_err());
        let (one, _two) = UnixStream::pair().unwrap();
        check_peer(&one).unwrap();
    }

    #[test]
    fn an_unreachable_lock_holder_is_not_spawned_over_and_over() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(dir.path()).unwrap();
        let _held = OwnerLock::try_acquire(&paths).unwrap().unwrap();
        let settings = Settings {
            watch: false,
            interval: 10,
            upstream: None,
        };
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(100);
        let error =
            match connect_or_launch_with_timeout(&paths, dir.path(), &settings, &[], timeout) {
                Err(error) => error,
                _ => panic!("unreachable lock holder unexpectedly connected"),
            };
        assert!(
            error.contains("lock is held but its socket is unreachable"),
            "{error}"
        );
        assert!(start.elapsed() >= timeout);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(error.contains("XDG_RUNTIME_DIR"), "{error}");
        assert!(!paths.log().exists(), "no child should have been launched");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_spawn_reports_setup_errors_without_a_child_status_timeout() {
        let Some(addchdir) = macos_spawn::addchdir() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(dir.path()).unwrap();
        let error =
            macos_spawn::launch(&paths, &dir.path().join("missing"), &[], addchdir).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn settings_round_trip_and_mismatches_are_noted_in_name_order() {
        let owner = Settings {
            watch: true,
            interval: 10,
            upstream: Some("http://127.0.0.1:3845/mcp".into()),
        };
        assert_eq!(Settings::from_json(&owner.to_json()), Some(owner.clone()));
        assert!(settings_notes(&owner, &owner, 7).is_empty());
        let relay = Settings {
            watch: false,
            interval: 30,
            upstream: None,
        };
        let notes = settings_notes(&relay, &owner, 7);
        assert_eq!(notes.len(), 3);
        assert!(
            notes[0].contains("interval 30s was not applied"),
            "{notes:?}"
        );
        assert!(notes[1].contains("upstream off"), "{notes:?}");
        assert!(notes[2].contains("watch off"), "{notes:?}");
        assert_eq!(
            relay.args(),
            vec!["--interval", "30", "--no-watch", "--no-upstream"]
        );
    }

    #[test]
    fn discovery_is_written_atomically_owner_only_and_removed_only_by_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(dir.path()).unwrap();
        assert_eq!(read_discovery(&paths), None);
        let discovery = Discovery {
            socket: paths.socket(),
            pid: 42,
            generation: "abc".into(),
            version: version().into(),
            protocol: PROTOCOL,
        };
        write_discovery(&paths, &discovery).unwrap();
        assert_eq!(read_discovery(&paths), Some(discovery.clone()));
        let mode = std::fs::metadata(paths.discovery())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        remove_discovery(&paths, "other");
        assert!(paths.discovery().exists());
        remove_discovery(&paths, "abc");
        assert!(!paths.discovery().exists());
    }

    #[test]
    fn the_owner_lock_admits_one_holder_and_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(dir.path()).unwrap();
        let first = OwnerLock::try_acquire(&paths).unwrap().unwrap();
        assert!(first.still_valid());
        assert!(OwnerLock::try_acquire(&paths).unwrap().is_none());
        drop(first);
        let second = OwnerLock::try_acquire(&paths).unwrap().unwrap();
        // A replaced lock file invalidates the holder.
        std::fs::remove_file(paths.lock()).unwrap();
        assert!(!second.still_valid());
    }

    #[test]
    fn hello_checks_project_version_and_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(dir.path()).unwrap();
        let hello = hello_request(&paths, None);
        assert_eq!(hello["method"], HELLO);
        check_hello(&hello["params"], &paths).unwrap();
        let mut other = hello["params"].clone();
        other["project"] = json!("/elsewhere");
        assert!(
            check_hello(&other, &paths)
                .unwrap_err()
                .contains("not /elsewhere")
        );
        let mut older = hello["params"].clone();
        older["version"] = json!("0.0.1");
        let err = check_hello(&older, &paths).unwrap_err();
        assert!(
            err.contains("cannot share it") && err.contains("kill"),
            "{err}"
        );
        let mut newer = hello["params"].clone();
        newer["protocol"] = json!(PROTOCOL + 1);
        assert!(check_hello(&newer, &paths).is_err());
    }

    #[test]
    fn a_stale_socket_is_replaced_but_a_live_one_or_a_foreign_file_is_not() {
        let dir = tempfile::tempdir().unwrap();
        // Keep the test's sockets short and inside the tempdir.
        let socket = dir.path().join("s.sock");
        let live = UnixListener::bind(&socket).unwrap();
        let err = bind_socket(&socket).unwrap_err();
        assert!(err.contains("another process is listening"), "{err}");
        drop(live);
        // The listener is gone, its file is left behind: stale.
        assert!(socket.exists());
        let rebound = bind_socket(&socket).unwrap();
        drop(rebound);
        std::fs::remove_file(&socket).unwrap();
        std::fs::write(&socket, b"not a socket").unwrap();
        let err = bind_socket(&socket).unwrap_err();
        assert!(err.contains("not a socket"), "{err}");
        assert!(socket.exists());
    }
}
