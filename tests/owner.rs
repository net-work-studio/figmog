#![recursion_limit = "256"]

//! End-to-end tests of the repository's shared background figmog
//! (NWS-2953): every `figmog serve` is a stdio relay to one detached owner
//! per mirror root. Drives real relays (the compiled binary) in synthetic
//! Git repositories and worktrees, and a synthetic Figma REST server for
//! the one-pull-per-edit acceptance test. Owners are counted by process
//! inspection: `/proc` on Linux, `ps` and `lsof` elsewhere (macOS).

mod common;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);
const KEY: &str = "figmogownerkeyaaaa111";
const OTHER: &str = "figmogownerkeybbbb222";

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=figmog",
            "-c",
            "user.email=figmog@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A fresh repository with one commit (worktrees need one).
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "."]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "init"]);
    dir
}

fn mirror_root(repo: &Path) -> PathBuf {
    std::fs::canonicalize(repo.join(".git/figmog")).unwrap()
}

/// The published owner record, once there is one.
fn discovery(repo: &Path) -> Option<Value> {
    let text = std::fs::read(repo.join(".git/figmog/owner.json")).ok()?;
    serde_json::from_slice(&text).ok()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Running `figmog serve --background` processes whose working directory
/// is `cwd`.
fn owners_in(cwd: &Path) -> Vec<u32> {
    if cfg!(target_os = "linux") {
        owners_in_proc(cwd)
    } else {
        owners_in_ps(cwd)
    }
}

/// [`owners_in`] through Linux's `/proc`.
fn owners_in_proc(cwd: &Path) -> Vec<u32> {
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        if !args.contains(&b"--background".as_slice()) {
            continue;
        }
        // A zombie has no cmdline; a live owner has its cwd.
        if std::fs::read_link(entry.path().join("cwd")).is_ok_and(|link| link == cwd) {
            pids.push(pid);
        }
    }
    pids.sort();
    pids
}

/// [`owners_in`] without `/proc` (macOS): `ps` lists the live
/// `serve --background` processes, `lsof` gives each one's working
/// directory.
fn owners_in_ps(cwd: &Path) -> Vec<u32> {
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let ps = Command::new("ps")
        .args(["-Ao", "pid=,stat=,command="])
        .output()
        .unwrap();
    assert!(ps.status.success(), "ps failed");
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&ps.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(stat)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let args: Vec<&str> = fields.collect();
        if stat.starts_with('Z') || !args.contains(&"serve") || !args.contains(&"--background") {
            continue;
        }
        let Ok(lsof) = Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .output()
        else {
            continue;
        };
        // Gone since `ps`: no output.
        let dir = String::from_utf8_lossy(&lsof.stdout)
            .lines()
            .find_map(|line| line.strip_prefix('n').map(PathBuf::from));
        if dir
            .and_then(|dir| std::fs::canonicalize(dir).ok())
            .as_deref()
            == Some(&cwd)
        {
            pids.push(pid);
        }
    }
    pids.sort();
    pids
}

fn kill(pid: u64) {
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Kills its child on drop so a failing test leaves no relay behind.
struct Relay {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: u64,
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn relay_command(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("figmog"));
    cmd.arg("serve")
        .args(args)
        .current_dir(cwd)
        .env("FIGMA_TOKEN", "")
        .env("FIGMOG_OWNER_IDLE_MS", "500");
    cmd
}

fn spawn_relay(cmd: &mut Command) -> Relay {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();
    let stderr = Arc::new(Mutex::new(Vec::new()));
    {
        let stderr = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                stderr.lock().unwrap().push(line);
            }
        });
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    Relay {
        child,
        stdin,
        rx,
        stderr,
        next_id: 1,
    }
}

impl Relay {
    fn send(&mut self, frame: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{frame}").unwrap();
        stdin.flush().unwrap();
    }

    /// The response to `id`, skipping notifications.
    fn response(&mut self, id: u64) -> Value {
        self.response_to(&json!(id))
    }

    /// The response to the request with JSON-RPC id `id`.
    fn response_to(&mut self, id: &Value) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.rx.recv_timeout(left).unwrap_or_else(|_| {
                panic!(
                    "no response to {id}; relay stderr: {:?}",
                    self.stderr.lock().unwrap()
                )
            });
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == *id {
                return frame;
            }
        }
    }

    fn initialize(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}));
        let response = self.response(id);
        assert_eq!(
            response["result"]["serverInfo"]["name"], "figmog",
            "{response}"
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// A `tools/call`'s result: its parsed JSON text, or `Err` text.
    fn call(&mut self, tool: &str, args: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args}}));
        let response = self.response(id);
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("unexpected response {response}"));
        if result["isError"] == true {
            return Err(text.to_string());
        }
        Ok(serde_json::from_str(text).unwrap_or(Value::String(text.into())))
    }

    /// The owner pid this relay reported it relays to.
    fn owner_pid(&self) -> u64 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(pid) = self.stderr.lock().unwrap().iter().find_map(|line| {
                let rest = line.split("background figmog (pid ").nth(1)?;
                rest.split(',').next()?.parse().ok()
            }) {
                return pid;
            }
            assert!(Instant::now() < deadline, "relay never reported its owner");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr.lock().unwrap().join("\n")
    }

    /// Close stdin and wait for a clean exit.
    fn finish(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "relay exited {status:?}: {}",
                    self.stderr_text()
                );
                return;
            }
            assert!(Instant::now() < deadline, "relay did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Pull a synthetic fixture into `cwd`'s project as `key` (direct: no owner
/// is running), landing in the shared mirror root.
fn seed(cwd: &Path, key: &str, fixture: &Value) {
    let response = cwd.join(format!("{key}.json"));
    std::fs::write(&response, fixture.to_string()).unwrap();
    common::command()
        .args(["pull", key, "--from-file"])
        .arg(&response)
        .current_dir(cwd)
        .assert()
        .success();
    std::fs::remove_file(response).unwrap();
}

fn wait_owner_gone(repo: &Path) {
    wait_until("the owner to idle out", || discovery(repo).is_none());
}

/// Ten relays started at once in one repository elect exactly one owner
/// and all reach it; the owner idles out (removing its socket and record)
/// once they close. Repeated over several trials.
#[test]
fn concurrent_relays_elect_exactly_one_owner() {
    const TRIALS: usize = 3;
    const RELAYS: usize = 10;
    for _ in 0..TRIALS {
        let repo = repo();
        let mut relays: Vec<Relay> = (0..RELAYS)
            .map(|_| {
                spawn_relay(&mut relay_command(
                    repo.path(),
                    &["--no-watch", "--no-upstream"],
                ))
            })
            .collect();
        for relay in &mut relays {
            relay.initialize();
            assert_eq!(relay.call("figmog_files", json!({})).unwrap(), json!([]));
        }
        let pids: BTreeSet<u64> = relays.iter().map(Relay::owner_pid).collect();
        assert_eq!(pids.len(), 1, "relays reached different owners: {pids:?}");
        let record = discovery(repo.path()).unwrap();
        assert_eq!(record["pid"].as_u64(), pids.first().copied());
        assert_eq!(
            owners_in(repo.path()),
            vec![record["pid"].as_u64().unwrap() as u32],
            "exactly one background owner process"
        );
        if cfg!(target_os = "linux") {
            // The `ps`/`lsof` count other platforms use agrees.
            assert_eq!(owners_in_ps(repo.path()), owners_in(repo.path()));
        }
        let socket = PathBuf::from(record["socket"].as_str().unwrap());
        assert!(socket.as_os_str().len() < 104, "{}", socket.display());
        assert!(socket.starts_with("/tmp"), "{}", socket.display());
        for relay in relays {
            relay.finish();
        }
        wait_owner_gone(repo.path());
        assert!(!socket.exists(), "the owner removed its socket");
        assert!(mirror_root(repo.path()).join("owner.lock").exists());
        assert!(owners_in(repo.path()).is_empty());
    }
}

/// Ten owners launched at once: one wins the lock, the rest exit at once,
/// successfully, without touching its socket. A crashed owner (SIGKILL,
/// no cleanup) releases the lock with the process and leaves a stale
/// socket file, which the next owner replaces.
#[test]
fn concurrent_owner_launches_and_a_crash_leave_exactly_one_owner() {
    let repo = repo();
    let launch = || {
        Command::new(assert_cmd::cargo::cargo_bin!("figmog"))
            .args(["serve", "--background", "--no-watch", "--no-upstream"])
            .current_dir(repo.path())
            .env("FIGMA_TOKEN", "")
            .env("FIGMOG_OWNER_IDLE_MS", "60000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut children: Vec<Child> = (0..10).map(|_| launch()).collect();
    let mut exited = 0;
    let deadline = Instant::now() + TIMEOUT;
    while exited < 9 {
        exited = 0;
        for child in &mut children {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "a losing owner must exit 0");
                exited += 1;
            }
        }
        assert!(Instant::now() < deadline, "losers did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_until("discovery", || discovery(repo.path()).is_some());
    let record = discovery(repo.path()).unwrap();
    let winner = record["pid"].as_u64().unwrap();
    let running: Vec<u64> = children
        .iter_mut()
        .filter_map(|c| c.try_wait().unwrap().is_none().then(|| u64::from(c.id())))
        .collect();
    assert_eq!(running, vec![winner]);
    let socket = PathBuf::from(record["socket"].as_str().unwrap());

    // Crash: no cleanup at all.
    kill(winner);
    for child in &mut children {
        let _ = child.wait();
    }
    assert!(socket.exists(), "a crash leaves the socket file behind");
    assert!(discovery(repo.path()).is_some(), "and the stale record");

    // A relay finds nothing listening, starts a new owner, which takes the
    // released lock and replaces the stale socket.
    let mut relay = spawn_relay(&mut relay_command(
        repo.path(),
        &["--no-watch", "--no-upstream"],
    ));
    relay.initialize();
    assert_eq!(relay.call("figmog_files", json!({})).unwrap(), json!([]));
    let new_pid = relay.owner_pid();
    assert_ne!(new_pid, winner);
    assert_eq!(
        discovery(repo.path()).unwrap()["pid"].as_u64(),
        Some(new_pid)
    );
    relay.finish();
    wait_owner_gone(repo.path());
}

/// A relay whose owner crashes answers the next request through a new
/// owner it starts; the agent's session survives.
#[test]
fn a_relay_survives_its_owner_crashing() {
    let repo = repo();
    seed(repo.path(), KEY, &common::fixture_v1());
    let mut relay = spawn_relay(&mut relay_command(
        repo.path(),
        &["--no-watch", "--no-upstream", KEY],
    ));
    relay.initialize();
    assert_eq!(
        relay.call("figmog_status", json!({})).unwrap()["name"],
        "Fixture"
    );
    let first = relay.owner_pid();
    kill(first);
    wait_until("a new owner", || {
        discovery(repo.path()).is_some_and(|d| d["pid"].as_u64() != Some(first))
    });
    let status = relay.call("figmog_status", json!({})).unwrap();
    assert_eq!(status["name"], "Fixture");
    relay.finish();
    wait_owner_gone(repo.path());
}

/// Clients arriving while the owner idles out are served — by it, or by
/// the next owner after a "not accepted, reconnect" — never failed.
#[test]
fn clients_arriving_during_idle_exit_are_served() {
    let repo = repo();
    seed(repo.path(), KEY, &common::fixture_v1());
    for trial in 0..12u64 {
        let mut relay = spawn_relay(
            relay_command(repo.path(), &["--no-watch", "--no-upstream", KEY])
                .env("FIGMOG_OWNER_IDLE_MS", "200"),
        );
        relay.initialize();
        assert_eq!(
            relay.call("figmog_status", json!({})).unwrap()["name"],
            "Fixture"
        );
        relay.finish();
        // Arrive around the 200 ms idle deadline.
        std::thread::sleep(Duration::from_millis(150 + trial * 10));
        if trial % 2 == 0 {
            let out = common::command()
                .args(["status"])
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "trial {trial}: {out:?}");
            let status: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(status["name"], "Fixture");
        } else {
            let mut next = spawn_relay(
                relay_command(repo.path(), &["--no-watch", "--no-upstream", KEY])
                    .env("FIGMOG_OWNER_IDLE_MS", "200"),
            );
            next.initialize();
            assert_eq!(
                next.call("figmog_status", json!({})).unwrap()["name"],
                "Fixture",
                "trial {trial}"
            );
            next.finish();
        }
    }
    wait_owner_gone(repo.path());
}

/// A fake owner speaking another figmog version: a relay refuses with an
/// explanation (and starts no owner of its own), and so does the CLI.
#[test]
fn a_mismatched_owner_is_refused_with_an_explanation() {
    let repo = repo();
    let root = repo.path().join(".git/figmog");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let socket = repo.path().join("old.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let record = json!({"generation": "old", "pid": 4_000_000u64, "project": root.display().to_string(),
        "protocol": 1, "socket": socket.display().to_string(), "version": "0.0.1"});
    std::fs::write(root.join("owner.json"), record.to_string()).unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    {
        let served = served.clone();
        let root = root.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                served.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let hello: Value = serde_json::from_str(&line).unwrap();
                let reply = json!({"jsonrpc": "2.0", "id": hello["id"], "result": {
                    "attach_errors": {}, "generation": "old", "pid": 4_000_000u64,
                    "project": root.display().to_string(), "protocol": 1,
                    "settings": {"interval": 10, "upstream": null, "watch": false},
                    "version": "0.0.1"}});
                let mut out = stream;
                let _ = writeln!(out, "{reply}");
            }
        });
    }
    let output = relay_command(repo.path(), &["--no-watch", "--no-upstream"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    let error = error["error"].as_str().unwrap();
    assert!(
        error.contains("runs figmog 0.0.1") && error.contains("cannot share it"),
        "{error}"
    );
    assert!(error.contains("kill 4000000"), "{error}");
    assert!(
        owners_in(repo.path()).is_empty(),
        "no second owner was started"
    );
    assert_eq!(discovery(repo.path()).unwrap()["generation"], "old");

    let out = common::command()
        .args(["call", "figmog_files"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot share it"));
    assert!(served.load(Ordering::SeqCst) >= 2);
}

/// Two repositories run fully independent owners at the same time.
#[test]
fn two_projects_run_independent_owners() {
    let (one, two) = (repo(), repo());
    seed(one.path(), KEY, &common::fixture_v1());
    seed(two.path(), OTHER, &common::fixture_other());
    let mut a = spawn_relay(&mut relay_command(
        one.path(),
        &["--no-watch", "--no-upstream", KEY],
    ));
    let mut b = spawn_relay(&mut relay_command(
        two.path(),
        &["--no-watch", "--no-upstream", OTHER],
    ));
    a.initialize();
    b.initialize();
    assert_eq!(
        a.call("figmog_status", json!({})).unwrap()["name"],
        "Fixture"
    );
    assert_eq!(
        b.call("figmog_status", json!({})).unwrap()["name"],
        "OtherFixture"
    );
    assert_ne!(a.owner_pid(), b.owner_pid());
    let (da, db) = (
        discovery(one.path()).unwrap(),
        discovery(two.path()).unwrap(),
    );
    assert_ne!(da["socket"], db["socket"]);
    a.finish();
    wait_owner_gone(one.path());
    // The other project's owner is untouched.
    assert_eq!(b.call("figmog_files", json!({})).unwrap()[0]["key"], OTHER);
    b.finish();
    wait_owner_gone(two.path());
}

/// Worktrees share one owner, but each relay routes calls without a
/// `file` to its own worktree's `figmog.toml` default — never to the
/// owner's or another worktree's. Settings the owner does not use are
/// reported, not silently taken.
#[test]
fn each_worktree_relays_its_own_default_file() {
    let repo = repo();
    let wt = repo.path().join("wt");
    git(
        repo.path(),
        &["worktree", "add", "-q", wt.to_str().unwrap()],
    );
    std::fs::write(
        repo.path().join("figmog.toml"),
        format!("[files]\ndefault = \"{KEY}\"\n"),
    )
    .unwrap();
    std::fs::write(
        wt.join("figmog.toml"),
        format!("[files]\ndefault = \"{OTHER}\"\n"),
    )
    .unwrap();
    seed(repo.path(), KEY, &common::fixture_v1());
    seed(&wt, OTHER, &common::fixture_other());

    let mut main = spawn_relay(&mut relay_command(
        repo.path(),
        &["--no-watch", "--no-upstream"],
    ));
    main.initialize();
    let mut other = spawn_relay(&mut relay_command(
        &wt,
        &["--no-upstream", "--interval", "30"],
    ));
    other.initialize();
    assert_eq!(main.owner_pid(), other.owner_pid());
    assert_eq!(
        main.call("figmog_status", json!({})).unwrap()["name"],
        "Fixture"
    );
    assert_eq!(
        other.call("figmog_status", json!({})).unwrap()["name"],
        "OtherFixture"
    );
    // Both sessions are open in the one owner; each default still holds.
    assert_eq!(
        main.call("figmog_pages", json!({})).unwrap()[0]["name"],
        "Page 1"
    );
    assert_eq!(
        main.call("figmog_status", json!({})).unwrap()["name"],
        "Fixture"
    );
    // An explicit file still wins.
    assert_eq!(
        main.call("figmog_status", json!({"file": OTHER})).unwrap()["name"],
        "OtherFixture"
    );
    let notes = other.stderr_text();
    assert!(notes.contains("interval 30s was not applied"), "{notes}");
    assert!(notes.contains("watch on was not applied"), "{notes}");
    // The CLI in each worktree reads the same shared mirrors.
    for (cwd, name) in [(repo.path(), "Fixture"), (wt.as_path(), "OtherFixture")] {
        let out = common::command()
            .arg("status")
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let status: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(status["name"], name);
    }
    main.finish();
    other.finish();
    wait_owner_gone(repo.path());
}

/// A mirror still in a checkout's old `.figmog/<key>/db` is never shadowed
/// by a new shared one: the relay refuses it and points at `migrate`.
#[test]
fn a_relay_refuses_a_mirror_awaiting_migration() {
    let repo = repo();
    let response = repo.path().join("v1.json");
    std::fs::write(&response, common::fixture_v1().to_string()).unwrap();
    common::command()
        .args(["pull", "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(repo.path().join(".figmog").join(KEY).join("db"))
        .assert()
        .success();
    let output = relay_command(repo.path(), &["--no-watch", "--no-upstream", KEY])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("figmog migrate"), "{error}");
    assert!(!repo.path().join(".git/figmog").join(KEY).exists());
}

/// Agent ids are the agent's, whatever they look like: one shaped like
/// the relay's own owner-link ids is answered under that exact id, never
/// swallowed as the relay's — also after a reconnect, whose replayed
/// `initialize` answer the agent never sees.
#[test]
fn a_relay_answers_agent_ids_shaped_like_its_own() {
    let repo = repo();
    let mut relay = spawn_relay(&mut relay_command(
        repo.path(),
        &["--no-watch", "--no-upstream"],
    ));
    relay.initialize();
    let ask = |relay: &mut Relay, id: &str| {
        let id = json!(id);
        relay.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "figmog_files", "arguments": {}}}));
        let response = relay.response_to(&id);
        assert_eq!(response["result"]["isError"], false, "{response}");
        relay.send(&json!({"jsonrpc": "2.0", "id": id, "method": "ping"}));
        let response = relay.response_to(&id);
        assert_eq!(response["result"], json!({}), "{response}");
    };
    for id in ["figmog-relay-x", "figmog-relay-0", "figmog-relay-1"] {
        ask(&mut relay, id);
    }
    // A new owner after a crash: the relay replays `initialize` under its
    // own id; the agent's look-alike ids still get their answers.
    kill(relay.owner_pid());
    wait_until("the crashed owner's record to go stale", || {
        relay.call("figmog_files", json!({})).is_ok()
    });
    for id in ["figmog-relay-x", "figmog-relay-2", "figmog-relay-3"] {
        ask(&mut relay, id);
    }
    // Nothing but answers to the agent's own requests reached it.
    relay.send(&json!({"jsonrpc": "2.0", "id": 99, "method": "ping"}));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = relay.rx.recv_timeout(left).expect("an answer to 99");
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert!(
            frame.get("method").is_some() || frame["id"] == 99,
            "{frame}"
        );
        if frame["id"] == 99 {
            break;
        }
    }
    relay.finish();
    wait_owner_gone(repo.path());
}

/// A mirror still in the checkout's old `.figmog/<key>/db` is never the
/// shared owner's, but an in-process serve holding it (`serve --db`) still
/// answers the CLI over its local socket instead of a lock error.
#[test]
fn the_cli_reaches_an_in_process_serve_of_a_legacy_mirror() {
    let repo = repo();
    let db = repo.path().join(".figmog").join(KEY).join("db");
    let response = repo.path().join("v1.json");
    std::fs::write(&response, common::fixture_v1().to_string()).unwrap();
    common::command()
        .args(["pull", "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();
    std::fs::write(repo.path().join(".figmog/current"), KEY).unwrap();
    let mut serve = spawn_relay(&mut relay_command(
        repo.path(),
        &[
            "--no-watch",
            "--no-upstream",
            "--db",
            db.to_str().unwrap(),
            KEY,
        ],
    ));
    serve.initialize();
    wait_until("the in-process serve's socket", || {
        repo.path().join(".figmog/serve.sock").exists()
    });
    for args in [&["status"][..], &["call", "figmog_status"]] {
        let out = common::command()
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        let status: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(status["name"], "Fixture", "{args:?}");
    }
    assert!(discovery(repo.path()).is_none(), "no owner was involved");
    serve.finish();
}

/// A linked worktree with no default file of its own never gets the main
/// checkout's file from the shared owner, though that is the one file the
/// owner has open: the CLI reports this worktree's "no mirror here", the
/// same as without the owner.
#[test]
fn a_worktree_without_a_default_does_not_inherit_the_owners_file() {
    let repo = repo();
    let wt = repo.path().join("wt");
    git(
        repo.path(),
        &["worktree", "add", "-q", wt.to_str().unwrap()],
    );
    seed(repo.path(), KEY, &common::fixture_v1());
    let mut main = spawn_relay(&mut relay_command(
        repo.path(),
        &["--no-watch", "--no-upstream", KEY],
    ));
    main.initialize();
    assert_eq!(
        main.call("figmog_status", json!({})).unwrap()["name"],
        "Fixture"
    );
    for args in [&["status"][..], &["call", "figmog_status"], &["pages"]] {
        let out = common::command()
            .args(args)
            .current_dir(&wt)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains("no mirror here"), "{args:?}: {error}");
    }
    // Naming the file, or a management call, still reaches the owner.
    let out = common::command()
        .args(["call", "figmog_status", "--args"])
        .arg(json!({"file": KEY}).to_string())
        .current_dir(&wt)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["name"], "Fixture");
    let out = common::command()
        .args(["call", "figmog_files"])
        .current_dir(&wt)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let files: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(files[0]["key"], KEY);
    // The main checkout's own default still routes.
    let out = common::command()
        .arg("status")
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    main.finish();
    wait_owner_gone(repo.path());
}

// ---- acceptance: three worktrees, one mirror, one pull per edit ----

/// A synthetic Figma REST API: `/meta` reports `touched`, the file
/// endpoint serves `file` and counts full-file fetches.
struct FakeFigma {
    state: Mutex<(String, Value)>,
    file_fetches: AtomicUsize,
}

fn serve_fake_figma(fake: Arc<FakeFigma>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let fake = fake.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, body) = {
                    let state = fake.state.lock().unwrap();
                    if path == format!("/v1/files/{KEY}/meta") {
                        let meta = json!({"file": {"name": "Fixture", "last_touched_at": state.0}});
                        ("200 OK", meta.to_string())
                    } else if path == format!("/v1/files/{KEY}") {
                        fake.file_fetches.fetch_add(1, Ordering::SeqCst);
                        ("200 OK", state.1.to_string())
                    } else {
                        (
                            "404 Not Found",
                            json!({"status": 404, "err": "Not found"}).to_string(),
                        )
                    }
                };
                let mut out = stream;
                let _ = write!(
                    out,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = out.flush();
                let mut rest = Vec::new();
                let _ = reader.read_to_end(&mut rest);
            });
        }
    });
    address
}

/// The NWS-2953 acceptance test: three worktrees of one repository, each
/// with its own `figmog serve` relay, read the same mirror concurrently
/// with no lock errors, and one (synthetic) Figma edit causes exactly one
/// pull. Also measures the cold start: first relay spawn → owner launched
/// → its first answer.
#[test]
fn three_worktrees_share_one_mirror_and_one_edit_pulls_once() {
    let repo = repo();
    std::fs::write(
        repo.path().join("figmog.toml"),
        format!("[files]\ndefault = \"{KEY}\"\n"),
    )
    .unwrap();
    git(repo.path(), &["add", "figmog.toml"]);
    git(repo.path(), &["commit", "-q", "-m", "figmog"]);
    let worktrees: Vec<PathBuf> = ["two", "three"]
        .iter()
        .map(|name| {
            let wt = repo.path().join(name);
            git(
                repo.path(),
                &["worktree", "add", "-q", wt.to_str().unwrap()],
            );
            wt
        })
        .collect();
    let fake = Arc::new(FakeFigma {
        state: Mutex::new(("2026-08-01T00:00:00Z".into(), common::fixture_v1())),
        file_fetches: AtomicUsize::new(0),
    });
    let api = serve_fake_figma(fake.clone());
    let cwds: Vec<PathBuf> = std::iter::once(repo.path().to_path_buf())
        .chain(worktrees)
        .collect();
    let started = Instant::now();
    let mut relays: Vec<Relay> = cwds
        .iter()
        .map(|cwd| {
            spawn_relay(
                relay_command(cwd, &["--no-upstream", "--interval", "1"])
                    .env("FIGMA_TOKEN", "synthetic-token")
                    .env("FIGMOG_API_BASE", &api)
                    .env("FIGMOG_OWNER_IDLE_MS", "1000"),
            )
        })
        .collect();
    relays[0].initialize();
    let cold_start = started.elapsed();
    eprintln!("cold start (relay spawn → owner launched → first answer): {cold_start:?}");
    assert!(cold_start < Duration::from_secs(10), "{cold_start:?}");
    for relay in &mut relays[1..] {
        relay.initialize();
    }
    let owner = relays[0].owner_pid();
    assert!(relays.iter().all(|r| r.owner_pid() == owner));

    // The first reference pulls the (new) mirror once for every worktree.
    let version = |relay: &mut Relay| -> Option<String> {
        relay
            .call("figmog_status", json!({}))
            .ok()
            .and_then(|s| s["version"].as_str().map(str::to_string))
    };
    for relay in &mut relays {
        wait_until("the first pull", || {
            version(relay).as_deref() == Some("100")
        });
    }

    // Concurrent reads from all three worktrees, relays and CLI: no lock
    // errors anywhere.
    let readers: Vec<_> = cwds
        .iter()
        .cloned()
        .map(|cwd| {
            std::thread::spawn(move || {
                for _ in 0..5 {
                    let out = common::command()
                        .args(["find", "--type", "FRAME"])
                        .current_dir(&cwd)
                        .output()
                        .unwrap();
                    assert!(out.status.success(), "{out:?}");
                }
            })
        })
        .collect();
    for _ in 0..5 {
        for relay in &mut relays {
            let found = relay.call("figmog_find", json!({"type": "FRAME"})).unwrap();
            assert!(found.as_array().is_some_and(|f| !f.is_empty()), "{found}");
        }
    }
    for reader in readers {
        reader.join().unwrap();
    }

    // Settle: the startup reconciliation pull (spec §6) is allowed; then
    // the count must hold still.
    let settle = |fake: &FakeFigma| {
        let mut last = fake.file_fetches.load(Ordering::SeqCst);
        let mut since = Instant::now();
        let deadline = Instant::now() + TIMEOUT;
        while since.elapsed() < Duration::from_secs(5) {
            assert!(Instant::now() < deadline, "fetches never settled");
            std::thread::sleep(Duration::from_millis(100));
            let now = fake.file_fetches.load(Ordering::SeqCst);
            if now != last {
                last = now;
                since = Instant::now();
            }
        }
        last
    };
    let before = settle(&fake);
    assert!(before <= 2, "startup fetched the file {before} times");

    // One edit in Figma.
    *fake.state.lock().unwrap() = ("2026-08-02T00:00:00Z".into(), common::fixture_v2());
    for relay in &mut relays {
        wait_until("the edit to arrive", || {
            version(relay).as_deref() == Some("101")
        });
    }
    let after = settle(&fake);
    assert_eq!(after, before + 1, "one edit must cause exactly one pull");
    for cwd in &cwds {
        let out = common::command()
            .arg("status")
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let status: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(status["version"], "101");
    }
    for relay in relays {
        relay.finish();
    }
    wait_owner_gone(repo.path());

    // Idle restart: the next agent's relay starts a new owner, which
    // reopens the existing mirror (answering from it at once) and accepts
    // one reconciliation pull — the watch watermark is not persisted.
    let restarted = Instant::now();
    let mut relay = spawn_relay(
        relay_command(repo.path(), &["--no-upstream", "--interval", "1"])
            .env("FIGMA_TOKEN", "synthetic-token")
            .env("FIGMOG_API_BASE", &api)
            .env("FIGMOG_OWNER_IDLE_MS", "1000"),
    );
    relay.initialize();
    assert_eq!(version(&mut relay).as_deref(), Some("101"));
    let restart = restarted.elapsed();
    eprintln!("idle restart (relay spawn → new owner → first mirror read): {restart:?}");
    assert!(restart < Duration::from_secs(10), "{restart:?}");
    let reconciled = settle(&fake);
    assert!(
        reconciled <= after + 1,
        "{reconciled} fetches after a restart"
    );
    relay.finish();
    wait_owner_gone(repo.path());
}
