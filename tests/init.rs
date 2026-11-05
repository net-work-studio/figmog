#![recursion_limit = "256"]

//! `figmog init`, `figmog settings` and `figmog doctor` (NWS-2946), driven
//! through the compiled binary in synthetic React + Tailwind repositories.
//! Offline: the first pull reads a saved response (`--from-file`), Figma's
//! REST API is a local synthetic server, and every "unreachable" endpoint
//! is a closed loopback port.

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const KEY: &str = "SyntheticInitKey0001";
const OTHER: &str = "SyntheticInitKey0002";
const TIMEOUT: Duration = Duration::from_secs(30);

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A new synthetic React + Tailwind v4 (Vite plugin) repository.
fn react_tailwind_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "."]);
    std::fs::write(
        dir.path().join("package.json"),
        json!({
            "name": "synthetic-app",
            "private": true,
            "dependencies": {"react": "19.0.0", "react-dom": "19.0.0"},
            "devDependencies": {"@tailwindcss/vite": "4.0.0", "tailwindcss": "4.0.0", "vite": "6.0.0"}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/index.css"),
        "@import \"tailwindcss\";\n",
    )
    .unwrap();
    dir
}

/// The fixture as a saved `GET /v1/files/:key` response, outside the repo.
fn saved_response() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.json");
    std::fs::write(&path, common::fixture_v1().to_string()).unwrap();
    (dir, path)
}

fn bin() -> PathBuf {
    PathBuf::from(assert_cmd::cargo::cargo_bin!("figmog"))
}

fn run_json(cwd: &Path, args: &[&str]) -> Value {
    let out = common::command()
        .current_dir(cwd)
        .args(args)
        .assert()
        .success();
    serde_json::from_slice(&out.get_output().stdout).unwrap()
}

fn run_error(cwd: &Path, args: &[&str]) -> String {
    let out = common::command()
        .current_dir(cwd)
        .args(args)
        .assert()
        .failure()
        .code(1);
    let error: Value = serde_json::from_slice(&out.get_output().stderr).unwrap();
    error["error"].as_str().unwrap().to_string()
}

/// Every file `init` writes, by name (absent ones omitted).
fn written(repo: &Path) -> BTreeMap<&'static str, String> {
    ["figmog.toml", ".mcp.json", ".gitignore", "AGENTS.md"]
        .into_iter()
        .filter_map(|name| Some((name, std::fs::read_to_string(repo.join(name)).ok()?)))
        .collect()
}

fn mirror(repo: &Path, key: &str) -> PathBuf {
    std::fs::canonicalize(repo.join(".git"))
        .unwrap()
        .join("figmog")
        .join(key)
        .join("db")
}

/// A closed loopback port: nothing listens there.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One MCP client over a child's stdio.
struct Mcp {
    child: std::process::Child,
    rx: mpsc::Receiver<String>,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mcp {
    fn send(&mut self, frame: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{frame}").unwrap();
        stdin.flush().unwrap();
    }

    fn response(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.rx.recv_timeout(left).expect("no MCP response");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == id {
                return frame;
            }
        }
    }
}

/// Start the server exactly as Claude Code would from `.mcp.json`: its
/// command and args, in the project directory.
fn start_from_mcp_json(repo: &Path) -> Mcp {
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(repo.join(".mcp.json")).unwrap()).unwrap();
    let server = &config["mcpServers"]["figmog"];
    assert_eq!(server["type"], "stdio");
    let args: Vec<&str> = server["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let mut child = Command::new(server["command"].as_str().unwrap())
        .args(&args)
        .current_dir(repo)
        .env("FIGMA_TOKEN", "")
        .env("FIGMOG_OWNER_IDLE_MS", "300")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    Mcp { child, rx }
}

/// Wait for the repository's background owner (if any) to idle out.
fn wait_owner_gone(repo: &Path) {
    let record = repo.join(".git/figmog/owner.json");
    let deadline = Instant::now() + TIMEOUT;
    while record.exists() {
        assert!(Instant::now() < deadline, "owner never idled out");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The owner releases its lock after removing the record; give it a
    // moment to exit before the temp dir goes.
    std::thread::sleep(Duration::from_millis(300));
}

/// The NWS-2946 acceptance test: one `figmog init` takes a new React +
/// Tailwind repository to a working mirror and a working MCP config, and
/// rerunning it changes nothing.
#[test]
fn init_takes_a_new_react_tailwind_repo_to_a_mirror_and_mcp_server() {
    let repo = react_tailwind_repo();
    let (_saved, response) = saved_response();
    let bin = bin();
    let args = [
        "init",
        "--file",
        &format!("https://www.figma.com/design/{KEY}/Synthetic?node-id=1-2"),
        "--from-file",
        response.to_str().unwrap(),
        "--mcp-command",
        bin.to_str().unwrap(),
    ];
    let out = run_json(repo.path(), &args);
    assert_eq!(out["config"]["status"], "created");
    assert_eq!(
        out["config"]["added_tables"],
        json!(["files", "stack", "inspector", "verify"])
    );
    assert_eq!(out["files"], json!({"default": KEY, "library": null}));
    assert_eq!(
        out["stack"],
        json!({
            "framework": "react",
            "styling": "tailwind",
            "tokens": "src/figmog-tokens.css",
            "source": {
                "framework": "package.json dependencies.react",
                "styling": "package.json devDependencies.tailwindcss",
            },
        })
    );
    assert_eq!(out["token"]["configured"], false);
    assert_eq!(out["gitignore"]["updated"], true);
    assert_eq!(out["migrated"], json!([]));
    assert_eq!(out["mcp"]["status"], "added");
    assert_eq!(out["agents"]["status"], "created");
    assert_eq!(out["pull"]["skipped"], false);
    assert_eq!(out["pull"]["churn"]["removed"], 0);
    assert!(out["pull"]["churn"]["added"].as_u64().unwrap() > 0);

    let files = written(repo.path());
    assert_eq!(files.len(), 4, "{files:?}");
    assert!(files["figmog.toml"].contains(&format!("default = \"{KEY}\"")));
    assert_eq!(files[".gitignore"], ".figmog/\nfigmog.local.toml\n");
    assert!(files["AGENTS.md"].starts_with("<!-- figmog:begin"));
    assert!(files["AGENTS.md"].contains("src/figmog-tokens.css"));

    // The mirror is in the shared root, and every command finds it.
    assert!(mirror(repo.path(), KEY).is_dir());
    let status = run_json(repo.path(), &["status"]);
    assert_eq!(status["name"], "Fixture");

    // The settings API, as other tools read it.
    let settings = run_json(repo.path(), &["settings"]);
    assert_eq!(settings["files"], json!({"default": KEY}));
    assert_eq!(settings["stack"]["framework"], "react");
    assert_eq!(
        settings["inspector"],
        json!({"enabled": true, "shortcut": "Alt+Shift+F"})
    );
    assert_eq!(
        settings["verify"],
        json!({
            "loop": true,
            "dev_server": "http://localhost:5173",
            "threshold": 95.0,
            "viewports": [1440, 390],
            "color_scheme": "light",
            "state": "default",
        })
    );

    // Idempotent: a rerun rewrites nothing.
    let again = run_json(repo.path(), &args);
    assert_eq!(again["config"]["status"], "unchanged");
    assert_eq!(again["config"]["added_tables"], json!([]));
    assert_eq!(again["gitignore"]["updated"], false);
    assert_eq!(again["mcp"]["status"], "unchanged");
    assert_eq!(again["agents"]["status"], "unchanged");
    assert_eq!(again["pull"]["churn"]["added"], 0);
    assert_eq!(again["pull"]["churn"]["changed"], 0);
    assert_eq!(written(repo.path()), files);

    // The registered server answers from the new mirror.
    let mut mcp = start_from_mcp_json(repo.path());
    mcp.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}));
    assert_eq!(mcp.response(1)["result"]["serverInfo"]["name"], "figmog");
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "figmog_status", "arguments": {}}}));
    let response = mcp.response(2);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    let status: Value = serde_json::from_str(text).unwrap();
    assert_eq!(status["name"], "Fixture", "{response}");
    drop(mcp.child.stdin.take());
    let deadline = Instant::now() + TIMEOUT;
    while mcp.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "server did not exit on EOF");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(mcp);
    wait_owner_gone(repo.path());
}

/// An existing project: the old per-checkout mirror moves to the shared
/// root, an existing `figmog.toml` keeps its text and only gains the
/// tables it lacks, and a flag contradicting it changes nothing.
#[test]
fn init_migrates_old_mirrors_and_only_appends_missing_tables() {
    let repo = react_tailwind_repo();
    let (_saved, response) = saved_response();
    let original = format!("# ours\n[files]\ndefault = \"{KEY}\" # product\n");
    std::fs::write(repo.path().join("figmog.toml"), &original).unwrap();
    std::fs::write(repo.path().join("AGENTS.md"), "# Agents\n\nBe nice.\n").unwrap();
    let legacy = repo.path().join(".figmog").join(KEY).join("db");
    common::command()
        .current_dir(repo.path())
        .args(["pull", KEY, "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(&legacy)
        .assert()
        .success();

    let error = run_error(repo.path(), &["init", "--file", OTHER, "--no-pull"]);
    assert!(error.contains("already sets files.default"), "{error}");
    let error = run_error(
        repo.path(),
        &["init", "--no-pull", "--dev-server", "http://10.0.0.1:5173"],
    );
    assert!(
        error.contains("--dev-server") && error.contains("not a local address"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("figmog.toml")).unwrap(),
        original
    );
    assert!(legacy.is_dir());

    let out = run_json(
        repo.path(),
        &[
            "init",
            "--from-file",
            response.to_str().unwrap(),
            "--tokens",
            "src/styles/tokens.css",
            "--dev-server",
            "http://127.0.0.1:3000",
        ],
    );
    assert_eq!(out["config"]["status"], "updated");
    assert_eq!(
        out["config"]["added_tables"],
        json!(["stack", "inspector", "verify"])
    );
    assert_eq!(out["migrated"].as_array().unwrap().len(), 1, "{out}");
    assert_eq!(out["migrated"][0]["file"], KEY);
    assert_eq!(out["agents"]["status"], "appended");
    assert_eq!(out["pull"]["churn"]["added"], 0, "{out}");
    assert!(!legacy.exists());
    assert!(mirror(repo.path(), KEY).is_dir());

    let toml = std::fs::read_to_string(repo.path().join("figmog.toml")).unwrap();
    assert!(
        toml.starts_with(&format!("{original}\n[stack]\n")),
        "{toml}"
    );
    let agents = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
    assert!(
        agents.starts_with("# Agents\n\nBe nice.\n\n<!-- figmog:begin"),
        "{agents}"
    );
    assert!(agents.contains("src/styles/tokens.css"));
    let settings = run_json(repo.path(), &["settings"]);
    assert_eq!(settings["stack"]["tokens"], "src/styles/tokens.css");
    assert_eq!(settings["verify"]["dev_server"], "http://127.0.0.1:3000");

    // Now that [stack] exists, a different flag is refused, the same one is not.
    let error = run_error(repo.path(), &["init", "--no-pull", "--tokens", "other.css"]);
    assert!(error.contains("already sets stack.tokens"), "{error}");
    let before = written(repo.path());
    run_json(
        repo.path(),
        &["init", "--no-pull", "--tokens", "src/styles/tokens.css"],
    );
    assert_eq!(written(repo.path()), before);
}

/// Without a detectable stack (and no flags), nothing is written.
#[test]
fn init_without_react_and_tailwind_fails_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "."]);
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"dependencies": {"vue": "3"}}"#,
    )
    .unwrap();
    let error = run_error(dir.path(), &["init", "--file", KEY, "--no-pull"]);
    assert!(error.contains("--framework react"), "{error}");
    assert!(error.contains("--styling tailwind"), "{error}");
    assert!(written(dir.path()).is_empty());

    let error = run_error(dir.path(), &["init", "--no-pull"]);
    assert!(error.contains("--file"), "{error}");
    let error = run_error(dir.path(), &["init", "--file", KEY, "--framework", "vue"]);
    assert!(error.contains("unsupported framework"), "{error}");
    assert!(written(dir.path()).is_empty());

    // Flags stand in for detection.
    let out = run_json(
        dir.path(),
        &[
            "init",
            "--file",
            KEY,
            "--no-pull",
            "--framework",
            "react",
            "--styling",
            "tailwind",
        ],
    );
    assert_eq!(out["stack"]["source"]["framework"], "--framework");
    assert_eq!(out["pull"], json!({"skipped": true}));
}

/// A synthetic Figma REST API: file meta for `KEY`, 403 on variables.
fn fake_figma() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
            let (status, body) = if path == format!("/v1/files/{KEY}/meta") {
                (
                    "200 OK",
                    json!({"file": {"name": "Fixture", "last_touched_at": "2026-08-01T00:00:00Z"}})
                        .to_string(),
                )
            } else if path == format!("/v1/files/{KEY}/variables/local") {
                (
                    "403 Forbidden",
                    json!({"status": 403, "err": "Forbidden"}).to_string(),
                )
            } else {
                (
                    "404 Not Found",
                    json!({"status": 404, "err": "Not found"}).to_string(),
                )
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
        }
    });
    address
}

fn checks(report: &Value) -> BTreeMap<String, (String, String)> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                (
                    c["status"].as_str().unwrap().to_string(),
                    c["detail"].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect()
}

fn status_of(checks: &BTreeMap<String, (String, String)>) -> BTreeMap<&str, &str> {
    checks
        .iter()
        .map(|(name, (status, _))| (name.as_str(), status.as_str()))
        .collect()
}

#[test]
fn doctor_reports_every_check_and_degrades_cleanly_offline() {
    let repo = react_tailwind_repo();
    let dev_server = TcpListener::bind("127.0.0.1:0").unwrap();
    let dev_url = format!(
        "http://127.0.0.1:{}",
        dev_server.local_addr().unwrap().port()
    );
    let bin = bin();
    run_json(
        repo.path(),
        &[
            "init",
            "--file",
            KEY,
            "--no-pull",
            "--dev-server",
            &dev_url,
            "--mcp-command",
            bin.to_str().unwrap(),
        ],
    );
    let desktop = format!("http://127.0.0.1:{}/mcp", closed_port());
    let doctor = |token: &str, api: &str| {
        Command::new(&bin)
            .args(["doctor", "--upstream", &desktop])
            .current_dir(repo.path())
            .env("FIGMA_TOKEN", token)
            .env("FIGMOG_API_BASE", api)
            .output()
            .unwrap()
    };

    // Online (synthetic API): file access ok, no Variables REST API.
    let api = fake_figma();
    let output = doctor("figd_synthetic", &api);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let found = checks(&report);
    assert_eq!(
        status_of(&found),
        BTreeMap::from([
            ("config", "ok"),
            ("token", "ok"),
            ("mirror", "warn"),
            ("file_access", "ok"),
            ("variables_access", "warn"),
            ("desktop_mcp", "warn"),
            ("mcp_config", "ok"),
            ("dev_server", "ok"),
        ])
    );
    let order: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "config",
            "token",
            "mirror",
            "file_access",
            "variables_access",
            "desktop_mcp",
            "mcp_config",
            "dev_server"
        ]
    );
    assert_eq!(report["status"], "warn");
    assert!(found["variables_access"].1.contains("import-variables"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("figd_synthetic"));

    // Offline: network checks warn quickly instead of failing or hanging.
    drop(dev_server);
    let started = Instant::now();
    let output = doctor(
        "figd_synthetic",
        &format!("http://127.0.0.1:{}", closed_port()),
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let found = checks(&serde_json::from_slice(&output.stdout).unwrap());
    assert_eq!(found["file_access"].0, "warn");
    assert!(
        found["file_access"].1.contains("could not reach Figma"),
        "{found:?}"
    );
    assert_eq!(found["variables_access"].0, "warn");
    assert_eq!(found["dev_server"].0, "warn");

    // No token: a failed check, the report still printed, exit 1.
    let output = doctor("", &api);
    assert_eq!(output.status.code(), Some(1));
    let found = checks(&serde_json::from_slice(&output.stdout).unwrap());
    assert_eq!(found["token"].0, "fail");
    assert_eq!(
        found["file_access"],
        ("warn".into(), "skipped: no token".into())
    );
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"], "doctor: failed checks: token");

    // A malformed figmog.toml is a failed check, not a crash.
    std::fs::write(
        repo.path().join("figmog.toml"),
        "[verify]\nthreshold = 500\n",
    )
    .unwrap();
    let output = doctor("figd_synthetic", &api);
    assert_eq!(output.status.code(), Some(1));
    let found = checks(&serde_json::from_slice(&output.stdout).unwrap());
    assert_eq!(found["config"].0, "fail");
    assert!(
        found["config"]
            .1
            .starts_with("figmog.toml:2: verify.threshold"),
        "{found:?}"
    );
}
