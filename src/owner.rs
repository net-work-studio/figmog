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
//! * **Socket:** `/tmp/figmog-<uid>/<16 hex>.sock`, the hex being a 64-bit
//!   FNV-1a hash of the project identity. Short on every platform (well
//!   under `sun_path`'s 104 bytes) and independent of `$TMPDIR` and
//!   `$XDG_RUNTIME_DIR`; the directory is created 0700 and ownership-checked
//!   ([`crate::private_dir`]), so another OS user can neither read nor
//!   pre-plant it.
//! * **Discovery:** `<mirror root>/owner.json` (0600, written to a temp file
//!   and renamed), naming the socket, pid, owner generation, binary version
//!   and protocol version. Clients connect to the socket it names.
//! * **Handshake:** the first frame on every connection is a
//!   [`HELLO`] request carrying the client's project identity, binary
//!   version and protocol version; the owner refuses a mismatch with
//!   [`HANDSHAKE_REFUSED`] and the client independently checks the owner's
//!   answer, so neither side silently talks to the wrong owner.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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

/// The per-user runtime directory holding every owner socket.
pub(crate) fn runtime_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/figmog-{}", euid()))
}

/// Where one project's owner listens.
pub(crate) fn socket_for(project: &Path) -> PathBuf {
    runtime_dir().join(format!(
        "{:016x}.sock",
        fnv1a(project.as_os_str().as_bytes())
    ))
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
    crate::private_dir::ensure(dir)
        .map_err(|e| format!("preparing the owner socket directory: {e}"))?;
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
         or stop it now with `kill {pid}`, then retry",
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
             clients for a while), or stop it now with `kill {}`, then retry",
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
    let Ok(stream) = UnixStream::connect(&discovery.socket) else {
        return Ok(Probe::Absent);
    };
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

/// The tail of the owner log, for a launch that failed.
pub(crate) fn log_tail(paths: &OwnerPaths) -> String {
    let text = std::fs::read_to_string(paths.log()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(5).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
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
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe =
        std::env::current_exe().map_err(|e| format!("locating the figmog executable: {e}"))?;
    let log = open_log(paths)?;
    let mut command = Command::new(exe);
    command
        .args(["serve", "--background"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // SAFETY: setsid is async-signal-safe and only detaches the child
    // from the relay's session and controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
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
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    let mut launched: Option<std::sync::mpsc::Receiver<std::process::ExitStatus>> = None;
    loop {
        match probe(paths, Some((settings, attach)))? {
            Probe::Ready(conn) => return Ok(conn),
            Probe::Absent if launched.is_none() => {
                launched = Some(launch(paths, cwd, &settings.args())?);
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
                let tail = log_tail(paths);
                return Err(format!(
                    "the background figmog failed to start ({status}); see {}{}",
                    paths.log().display(),
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!(":\n{tail}")
                    }
                ));
            }
            launched = None;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "the background figmog did not become ready within {}s; see {}",
                START_TIMEOUT.as_secs(),
                paths.log().display()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
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
