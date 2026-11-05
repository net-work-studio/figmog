//! `figmog serve` as a stdio relay to the repository's background owner
//! (NWS-2953). The agent's MCP frames go to the owner over its socket and
//! the owner's answers and notifications come back, so closing one
//! agent's session never affects another's; the owner itself opens the
//! mirrors and pulls each changed file once for every worktree.
//!
//! **Worktree context stays here.** The owner has no default file. For
//! every per-file local tool call without an explicit `file` (or a node
//! URL naming one) the relay injects *its* worktree's file: the first
//! startup `FILE`, else `figmog.toml`'s default (else `.figmog/current`),
//! else the one file this relay itself has referenced — the old
//! single-process serve's defaulting rule, kept per worktree. Without any,
//! the relay answers the old "no default file" error itself. A file whose
//! mirror in this worktree is still in the old per-checkout location is
//! refused with a pointer at `figmog migrate` rather than shadowed by a
//! new shared mirror (spec §16).
//!
//! **Reconnects.** A request the owner answers "not accepted" (it is
//! shutting down, nothing was started) is re-sent on a new connection —
//! which may start a new owner — up to [`MAX_RESENDS`] times; nothing else
//! is ever retried. If the owner goes away with requests unanswered, each
//! is answered with an error saying its outcome is unknown, and the relay
//! reconnects. On every new connection it repeats the handshake (with its
//! startup files) and the agent's `initialize`, so notifications such as
//! `tools/list_changed` keep reaching the agent.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Instant;

use serde_json::{Value, json};

use crate::ident::parse_file_ref;
use crate::mcp::ToolErrorMode;
use crate::owner::{self, OwnerConn, OwnerPaths, Settings};
use crate::project::Layout;

/// How often one request is re-sent after "not accepted" answers.
const MAX_RESENDS: u32 = 5;
/// Prefix of the relay's own JSON-RPC ids (its replayed `initialize`).
const INTERNAL_ID: &str = "figmog-relay-";

enum Msg {
    Stdin(String),
    StdinEof,
    /// A frame from the owner connection of this epoch.
    Owner(u64, String),
    /// The owner connection of this epoch ended.
    OwnerGone(u64),
}

/// A forwarded request awaiting its answer.
struct Sent {
    /// The frame as sent (re-sent after "not accepted").
    frame: String,
    /// The connection it was sent on.
    epoch: u64,
    resends: u32,
}

struct Relay {
    layout: Layout,
    paths: OwnerPaths,
    settings: Settings,
    /// Startup files' keys, attached on every connection.
    attach: Vec<String>,
    default_key: Option<String>,
    /// Files this relay's agent referenced (for the sole-file default).
    referenced: BTreeSet<String>,
    /// The agent's `initialize`, replayed on reconnect.
    initialize: Option<Value>,
    writer: Option<UnixStream>,
    epoch: u64,
    pending: BTreeMap<String, Sent>,
    tx: mpsc::Sender<Msg>,
    next_internal: u64,
}

/// Run `figmog serve` as a relay. `files` are its startup files; the
/// owner runs with `settings` if this relay starts it.
pub(crate) fn run_relay(
    layout: Layout,
    files: &[String],
    settings: Settings,
) -> Result<(), String> {
    let mut attach = Vec::new();
    for file in files {
        let key =
            parse_file_ref(file).ok_or_else(|| format!("not a Figma file key or URL: {file}"))?;
        // A startup file must be served from the shared mirror root.
        shared_store(&layout, &key)?;
        if !attach.contains(&key) {
            attach.push(key);
        }
    }
    let default_key = match attach.first() {
        Some(key) => Some(key.clone()),
        None => layout.default_key()?,
    };
    if let Some(key) = &default_key
        && let Err(error) = shared_store(&layout, key)
    {
        eprintln!("figmog: {error}");
    }
    let paths = OwnerPaths::ensure(&layout.mirror_root)?;
    let (tx, rx) = mpsc::channel();
    let mut relay = Relay {
        layout,
        paths,
        settings,
        attach,
        default_key,
        referenced: BTreeSet::new(),
        initialize: None,
        writer: None,
        epoch: 0,
        pending: BTreeMap::new(),
        tx: tx.clone(),
        next_internal: 0,
    };
    let info = relay.connect()?;
    if !info.attach_errors.is_empty() {
        let errors: Vec<String> = info
            .attach_errors
            .iter()
            .map(|(key, error)| format!("{key}: {error}"))
            .collect();
        return Err(errors.join("; "));
    }
    eprintln!(
        "{} relaying to this repository's background figmog (pid {}, generation {}, {})",
        crate::mcp::SERVER_NAME,
        info.pid,
        info.generation,
        relay.paths.identity()
    );

    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(Msg::Stdin(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(Msg::StdinEof);
    });
    relay.run(&rx);
    Ok(())
}

/// `key`'s mirror must be in the shared mirror root: an old per-checkout
/// mirror (or two mirrors) is refused rather than shadowed by a new one.
fn shared_store(layout: &Layout, key: &str) -> Result<(), String> {
    let store = layout.active_store(key)?;
    if store.migration_pending {
        return Err(format!(
            "the mirror for {key} is still in this checkout's old location ({}); run `figmog \
             migrate` so the shared background figmog can serve it",
            store.path.display()
        ));
    }
    Ok(())
}

/// One frame to stdout. `false`: the agent is gone.
fn to_agent(frame: &str) -> bool {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{frame}").is_ok() && stdout.flush().is_ok()
}

/// A request's id as a map key.
fn id_key(id: &Value) -> String {
    id.to_string()
}

/// The answer to `id` when the relay cannot forward it: a tool error as
/// MCP content (like stdio always got) for `tools/call`, else a JSON-RPC
/// error.
fn local_error(id: Value, method: &str, message: &str) -> Value {
    if method == "tools/call" {
        crate::mcp::call_response(id, ToolErrorMode::McpContent, Err(message.to_string()))
    } else {
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}})
    }
}

impl Relay {
    /// Connect (launching an owner if needed), handshake with the startup
    /// files, start this connection's reader, and replay `initialize`.
    fn connect(&mut self) -> Result<owner::OwnerInfo, String> {
        let OwnerConn {
            reader,
            writer,
            info,
        } = owner::connect_or_launch(
            &self.paths,
            &self.layout.project_root,
            &self.settings,
            &self.attach,
        )?;
        if let Some(owner_settings) = &info.settings {
            for note in owner::settings_notes(&self.settings, owner_settings, info.pid) {
                eprintln!("{note}");
            }
        }
        self.epoch += 1;
        let epoch = self.epoch;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = line.trim_end_matches(['\n', '\r']).to_string();
                        if tx.send(Msg::Owner(epoch, line)).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Msg::OwnerGone(epoch));
        });
        self.writer = Some(writer);
        if let Some(initialize) = self.initialize.clone() {
            let mut replay = initialize;
            replay["id"] = json!(format!("{INTERNAL_ID}{}", self.next_internal));
            self.next_internal += 1;
            let _ = self.write_owner(&replay.to_string());
        }
        Ok(info)
    }

    fn write_owner(&mut self, frame: &str) -> bool {
        let Some(writer) = &mut self.writer else {
            return false;
        };
        let ok = writeln!(writer, "{frame}")
            .and_then(|()| writer.flush())
            .is_ok();
        if !ok {
            self.writer = None;
        }
        ok
    }

    /// Send `frame` (a request with `id`, or a notification when `None`),
    /// reconnecting first if the owner connection is gone.
    fn forward(&mut self, frame: String, request: Option<(Value, String)>, resends: u32) -> bool {
        if self.writer.is_none()
            && let Err(error) = self.connect()
        {
            eprintln!("figmog: {error}");
            return match request {
                Some((id, method)) => to_agent(&local_error(id, &method, &error).to_string()),
                None => true,
            };
        }
        if let Some((id, _)) = &request {
            self.pending.insert(
                id_key(id),
                Sent {
                    frame: frame.clone(),
                    epoch: self.epoch,
                    resends,
                },
            );
        }
        // On a failed write the reader reports the lost connection, and
        // its pending requests (this one included) are answered then.
        let _ = self.write_owner(&frame);
        true
    }

    fn run(&mut self, rx: &mpsc::Receiver<Msg>) {
        let mut drain_until: Option<Instant> = None;
        loop {
            if let Some(deadline) = drain_until
                && (self.pending.is_empty() || Instant::now() >= deadline)
            {
                return;
            }
            let msg = match drain_until {
                Some(deadline) => {
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(msg) => msg,
                        Err(_) => return,
                    }
                }
                None => match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return,
                },
            };
            let alive = match msg {
                Msg::Stdin(line) => self.on_agent(line),
                Msg::StdinEof => {
                    // Answers already owed are still delivered, within the
                    // same bound the in-process serve used.
                    drain_until = Some(Instant::now() + super::EXIT_DRAIN);
                    true
                }
                Msg::Owner(epoch, line) => self.on_owner(epoch, &line),
                Msg::OwnerGone(epoch) => self.owner_gone(epoch, drain_until.is_none()),
            };
            if !alive {
                return;
            }
        }
    }

    /// One agent frame: route it with this worktree's file and forward it,
    /// or answer it here.
    fn on_agent(&mut self, line: String) -> bool {
        let Ok(mut frame) = serde_json::from_str::<Value>(&line) else {
            // The owner answers the parse error.
            return self.forward(line, None, 0);
        };
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        let id = frame.get("id").cloned();
        let Some(id) = id.filter(|_| !method.is_empty() && !method.starts_with("notifications/"))
        else {
            return self.forward(line, None, 0);
        };
        if method == "initialize" {
            self.initialize = Some(frame.clone());
        }
        if method == "tools/call"
            && let Err(message) = self.route(&mut frame)
        {
            return to_agent(&local_error(id, &method, &message).to_string());
        }
        self.forward(frame.to_string(), Some((id, method)), 0)
    }

    /// Give a per-file local tool call this worktree's file, and refuse
    /// files whose mirror the shared owner must not serve from here.
    fn route(&mut self, frame: &mut Value) -> Result<(), String> {
        let name = frame["params"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if crate::serve::writes::is_internal(&name) || !crate::proxy::is_local_tool(&name) {
            return Ok(());
        }
        let args = &frame["params"]["arguments"];
        let explicit = match args.get("file") {
            Some(Value::String(file)) => Some(file.clone()),
            // Not a string: the owner reports the type error.
            Some(Value::Null) | None => None,
            Some(_) => return Ok(()),
        };
        if name == "figmog_files" {
            return Ok(());
        }
        if name == "figmog_open" {
            if let Some(key) = explicit.as_deref().and_then(parse_file_ref) {
                shared_store(&self.layout, &key)?;
                self.referenced.insert(key);
            }
            return Ok(());
        }
        if let Some(file) = explicit {
            // An unparseable `file` is the owner's error to report.
            if let Some(key) = parse_file_ref(&file) {
                shared_store(&self.layout, &key)?;
                self.referenced.insert(key);
            }
            return Ok(());
        }
        if let Some(key) = crate::dispatch::infer_file_from_node_ref(args) {
            shared_store(&self.layout, &key)?;
            self.referenced.insert(key);
            return Ok(());
        }
        let key = self
            .default_key
            .clone()
            .or_else(|| {
                (self.referenced.len() == 1)
                    .then(|| self.referenced.iter().next().cloned())
                    .flatten()
            })
            .ok_or_else(|| crate::sessions::NO_DEFAULT_FILE_MSG.to_string())?;
        shared_store(&self.layout, &key)?;
        let params = &mut frame["params"];
        if !params["arguments"].is_object() {
            params["arguments"] = json!({});
        }
        params["arguments"]["file"] = json!(key);
        Ok(())
    }

    /// One owner frame: forward notifications and answers; re-send a
    /// request the owner did not accept.
    fn on_owner(&mut self, epoch: u64, line: &str) -> bool {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        if frame.get("method").is_some() {
            // Notifications from the current connection only (a replaced
            // connection's would be duplicates or stale).
            return epoch != self.epoch || to_agent(line);
        }
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        if id.as_str().is_some_and(|id| id.starts_with(INTERNAL_ID)) {
            return true;
        }
        let key = id_key(&id);
        if frame["error"]["code"].as_i64() == Some(owner::NOT_ACCEPTED)
            && let Some(sent) = self.pending.remove(&key)
        {
            if sent.resends >= MAX_RESENDS {
                return to_agent(line);
            }
            // That owner is shutting down: stop writing to it.
            if sent.epoch == self.epoch {
                self.writer = None;
            }
            let method = serde_json::from_str::<Value>(&sent.frame)
                .ok()
                .and_then(|f| f["method"].as_str().map(str::to_string))
                .unwrap_or_default();
            return self.forward(sent.frame, Some((id, method)), sent.resends + 1);
        }
        self.pending.remove(&key);
        to_agent(line)
    }

    /// The owner connection of `epoch` ended: answer what it owed, then
    /// reconnect if it was the current one.
    fn owner_gone(&mut self, epoch: u64, reconnect: bool) -> bool {
        let lost: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, sent)| sent.epoch == epoch)
            .map(|(key, _)| key.clone())
            .collect();
        for key in lost {
            let Some(sent) = self.pending.remove(&key) else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(&sent.frame) else {
                continue;
            };
            let method = frame["method"].as_str().unwrap_or_default();
            let answer = local_error(
                frame["id"].clone(),
                method,
                "outcome unknown: the background figmog exited before answering; check \
                 figmog_status or figmog_files before retrying",
            );
            if !to_agent(&answer.to_string()) {
                return false;
            }
        }
        if epoch == self.epoch {
            self.writer = None;
            if reconnect && let Err(error) = self.connect() {
                // Retried on the agent's next request.
                eprintln!("figmog: {error}");
            }
        }
        true
    }
}
