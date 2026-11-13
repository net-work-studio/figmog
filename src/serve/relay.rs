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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Instant;

use crate::deep_json::OwnedValue;
use serde_json::{Value, json};

use crate::ident::parse_file_ref;
use crate::mcp::ToolErrorMode;
use crate::owner::{self, OwnerConn, OwnerPaths, Settings};
use crate::project::Layout;

/// How often one request is re-sent after "not accepted" answers.
const MAX_RESENDS: u32 = 5;
/// Prefix of the JSON-RPC ids the relay uses on the owner link. Every
/// request it sends there — the agent's (whose own id is mapped back on
/// the answer) and its own replayed `initialize` — gets a fresh one, so an
/// agent id can never be mistaken for an internal one, whatever it looks
/// like.
const OWNER_ID: &str = "figmog-relay-";

enum Msg {
    Stdin(String),
    StdinEof,
    /// The agent closed its output stream.
    StdoutGone,
    /// A frame from the owner connection of this epoch.
    Owner(u64, String),
    /// Only the reader sends this, after every frame it has read.
    OwnerGone(u64),
    Connected(u64, Result<OwnerConn, String>),
}

struct Connecting {
    id: u64,
    cancel: Arc<owner::ConnectCancel>,
}

impl Drop for Connecting {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

enum Outgoing {
    Request(String, Sent),
    Notification(String),
}

/// A forwarded request awaiting its answer, keyed by its owner-link id.
struct Sent {
    /// The agent's own id for it; `None` for the relay's own requests,
    /// whose answers are not the agent's.
    agent_id: Option<OwnedValue>,
    method: String,
    /// The frame as sent, with its owner-link id (re-sent after "not
    /// accepted").
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
    initialize: Option<String>,
    writer: Option<UnixStream>,
    owner_output: Option<mpsc::Sender<String>>,
    connecting: Option<Connecting>,
    next_connect: u64,
    outgoing: VecDeque<Outgoing>,
    drain_until: Option<Instant>,
    /// Agent output never blocks the relay's request and shutdown loop.
    stdout: Option<super::output::StdioWriter>,
    epoch: u64,
    pending: BTreeMap<String, Sent>,
    tx: mpsc::Sender<Msg>,
    /// The next owner-link request id's number.
    next_id: u64,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.connecting.take();
        self.disconnect();
    }
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
        && !attach.contains(key)
    {
        shared_store(&layout, key)?;
        attach.push(key.clone());
    }
    let paths = OwnerPaths::ensure(&layout.mirror_root)?;
    let (tx, rx) = mpsc::channel();
    let output_tx = tx.clone();
    let stdout = super::output::StdioWriter::spawn(move |frame| {
        let written = write_agent(frame);
        if !written {
            let _ = output_tx.send(Msg::StdoutGone);
        }
        written
    });
    let mut relay = Relay {
        layout,
        paths,
        settings,
        attach,
        default_key,
        referenced: BTreeSet::new(),
        initialize: None,
        writer: None,
        owner_output: None,
        connecting: None,
        next_connect: 0,
        outgoing: VecDeque::new(),
        drain_until: None,
        stdout: Some(stdout),
        epoch: 0,
        pending: BTreeMap::new(),
        tx: tx.clone(),
        next_id: 0,
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
        let mut reader = std::io::stdin().lock();
        while let Ok(Some(line)) = super::read_bounded_line(&mut reader) {
            if tx.send(Msg::Stdin(line)).is_err() {
                return;
            }
        }
        let _ = tx.send(Msg::StdinEof);
    });
    let deadline = relay
        .run(&rx)
        .unwrap_or_else(|| Instant::now() + super::EXIT_DRAIN);
    relay.connecting.take();
    // The reader holds another descriptor for this connection. Shut down
    // both halves before waiting on the agent's output, so an unread pipe
    // cannot keep the owner attached for the rest of the drain budget.
    relay.disconnect();
    relay.stdout.take().unwrap().finish(deadline);
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

/// One frame to stdout, called only by the output worker.
fn write_agent(frame: &str) -> bool {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{frame}").is_ok() && stdout.flush().is_ok()
}

/// The answer to `id` when the relay cannot forward it: a tool error as
/// MCP content (like stdio always got) for `tools/call`, else a JSON-RPC
/// error.
fn local_error(id: OwnedValue, method: &str, message: &str) -> OwnedValue {
    if method == "tools/call" {
        OwnedValue(crate::mcp::call_response(
            id,
            ToolErrorMode::McpContent,
            Err(message.to_string()),
        ))
    } else {
        let mut response =
            OwnedValue(json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": message}}));
        response.insert("id".into(), id);
        response
    }
}

impl Relay {
    fn to_agent(&self, frame: &str) -> bool {
        self.stdout
            .as_ref()
            .is_some_and(|stdout| stdout.send(frame.to_owned()))
    }

    /// Connect (launching an owner if needed), handshake with the startup
    /// files, start this connection's reader, and replay `initialize`.
    fn connect(&mut self) -> Result<owner::OwnerInfo, String> {
        let connection = owner::connect_or_launch(
            &self.paths,
            &self.layout.project_root,
            &self.settings,
            &self.attach,
        )?;
        self.install(connection)
    }

    fn install(&mut self, connection: OwnerConn) -> Result<owner::OwnerInfo, String> {
        self.install_with_reader_hook(connection, || {})
    }

    fn install_with_reader_hook(
        &mut self,
        connection: OwnerConn,
        mut after_read: impl FnMut() + Send + 'static,
    ) -> Result<owner::OwnerInfo, String> {
        let OwnerConn {
            reader,
            writer,
            info,
        } = connection;
        if self.expired() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
            return Err("relay EOF drain deadline expired".into());
        }
        let mut output = writer.try_clone().map_err(|error| error.to_string())?;
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
                        after_read();
                        let line = line.trim_end_matches(['\n', '\r']).to_string();
                        if tx.send(Msg::Owner(epoch, line)).is_err() {
                            return;
                        }
                    }
                }
            }
            // Sole terminal event: all replies read above must reach the
            // event loop before their pending agent-ID mappings retire.
            let _ = tx.send(Msg::OwnerGone(epoch));
        });
        let (frames, queue) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for frame in queue {
                if writeln!(output, "{frame}")
                    .and_then(|()| output.flush())
                    .is_err()
                {
                    // Wake the reader, which orders OwnerGone after any
                    // reply it has read. A writer terminal event could
                    // overtake that reply and discard its ID mapping.
                    let _ = output.shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        });
        self.writer = Some(writer);
        self.owner_output = Some(frames);
        if self.expired() {
            self.disconnect();
            return Err("relay EOF drain deadline expired".into());
        }
        if let Some(initialize) = self.initialize.clone() {
            let owner_id = self.owner_id();
            let mut replay =
                crate::deep_json::from_str(&initialize).map_err(|error| error.to_string())?;
            replay.insert("id".into(), OwnedValue(json!(owner_id)));
            let frame =
                crate::deep_json::to_string(&replay.0).map_err(|error| error.to_string())?;
            self.pending.insert(
                owner_id,
                Sent {
                    agent_id: None,
                    method: "initialize".into(),
                    frame: frame.clone(),
                    epoch: self.epoch,
                    resends: 0,
                },
            );
            let _ = self.write_owner(&frame);
        }
        Ok(info)
    }

    /// A fresh owner-link request id.
    fn owner_id(&mut self) -> String {
        let id = format!("{OWNER_ID}{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn write_owner(&mut self, frame: &str) -> bool {
        if self.expired() {
            return false;
        }
        let ok = self
            .owner_output
            .as_ref()
            .is_some_and(|output| output.send(frame.to_owned()).is_ok());
        if !ok {
            // Queue failure also leaves terminal ordering to the reader.
            // Shutdown wakes it without retiring its buffered replies.
            self.disconnect();
        }
        ok
    }

    fn disconnect(&mut self) {
        self.owner_output.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }

    fn expired(&self) -> bool {
        self.drain_until
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// All runtime connection work is off the event loop. Cancelling the
    /// retained attempt interrupts HELLO and forbids subsequent launches.
    fn begin_connect(&mut self) {
        if self.connecting.is_some() || self.expired() {
            return;
        }
        self.next_connect += 1;
        let id = self.next_connect;
        let cancel = Arc::new(owner::ConnectCancel::default());
        self.connecting = Some(Connecting {
            id,
            cancel: cancel.clone(),
        });
        let paths = self.paths.clone();
        let cwd = self.layout.project_root.clone();
        let settings = self.settings.clone();
        let attach = self.attach.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result =
                owner::connect_or_launch_cancelled(&paths, &cwd, &settings, &attach, &cancel);
            if cancel.is_cancelled() {
                if let Ok(connection) = result {
                    let _ = connection.writer.shutdown(std::net::Shutdown::Both);
                }
                return;
            }
            let _ = tx.send(Msg::Connected(id, result));
        });
    }

    fn connected(&mut self, id: u64, result: Result<OwnerConn, String>) -> bool {
        if self
            .connecting
            .as_ref()
            .is_none_or(|attempt| attempt.id != id)
            || self.expired()
        {
            if self.expired() {
                self.connecting.take();
            }
            if let Ok(connection) = result {
                let _ = connection.writer.shutdown(std::net::Shutdown::Both);
            }
            return !self.expired();
        }
        let attempt = self.connecting.take().expect("checked above");
        let result = result.and_then(|connection| {
            attempt.cancel.release();
            self.install(connection)
        });
        match result {
            Ok(_) => {
                while self.writer.is_some()
                    && let Some(outgoing) = self.outgoing.pop_front()
                {
                    if self.expired() {
                        return false;
                    }
                    match outgoing {
                        Outgoing::Request(id, sent) => {
                            self.send(id, sent);
                        }
                        Outgoing::Notification(frame) => {
                            self.forward(&frame);
                        }
                    }
                }
                true
            }
            Err(error) => {
                eprintln!("figmog: {error}");
                while let Some(outgoing) = self.outgoing.pop_front() {
                    if let Outgoing::Request(_, sent) = outgoing
                        && let Some(id) = sent.agent_id
                        && !self.to_agent(
                            &crate::deep_json::to_string(&local_error(id, &sent.method, &error).0)
                                .expect("Value serializes"),
                        )
                    {
                        return false;
                    }
                }
                true
            }
        }
    }

    /// Send a notification (or a frame the owner must reject) as is.
    fn forward(&mut self, frame: &str) -> bool {
        if self.writer.is_some() {
            let _ = self.write_owner(frame);
        } else if !self.expired() {
            self.outgoing
                .push_back(Outgoing::Notification(frame.to_owned()));
            self.begin_connect();
        }
        true
    }

    /// Send the agent's request `frame` under a fresh owner-link id.
    fn forward_request(
        &mut self,
        mut frame: OwnedValue,
        agent_id: OwnedValue,
        method: String,
    ) -> bool {
        let owner_id = self.owner_id();
        frame.insert("id".into(), OwnedValue(json!(owner_id)));
        let sent = Sent {
            agent_id: Some(agent_id),
            method,
            frame: crate::deep_json::to_string(&frame.0).expect("Value serializes"),
            epoch: 0,
            resends: 0,
        };
        self.send(owner_id, sent)
    }

    /// Send (or re-send) a pending request on the current connection.
    fn send(&mut self, owner_id: String, mut sent: Sent) -> bool {
        if self.expired() {
            return false;
        }
        if self.writer.is_none() {
            self.outgoing.push_back(Outgoing::Request(owner_id, sent));
            self.begin_connect();
            return true;
        }
        sent.epoch = self.epoch;
        let frame = sent.frame.clone();
        self.pending.insert(owner_id, sent);
        // On a failed write the reader reports the lost connection, and
        // its pending requests (this one included) are answered then.
        let _ = self.write_owner(&frame);
        true
    }

    /// Return the EOF deadline so final stdout flushing shares its budget.
    fn run(&mut self, rx: &mpsc::Receiver<Msg>) -> Option<Instant> {
        self.run_with_drain(rx, super::EXIT_DRAIN)
    }

    fn run_with_drain(
        &mut self,
        rx: &mpsc::Receiver<Msg>,
        budget: std::time::Duration,
    ) -> Option<Instant> {
        loop {
            if let Some(deadline) = self.drain_until
                && ((!self.pending.values().any(|sent| sent.agent_id.is_some())
                    && !self.outgoing.iter().any(|outgoing| matches!(outgoing, Outgoing::Request(_, sent) if sent.agent_id.is_some())))
                    || Instant::now() >= deadline)
            {
                return self.stop();
            }
            let msg = match self.drain_until {
                Some(deadline) => {
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(msg) => msg,
                        Err(_) => return self.stop(),
                    }
                }
                None => match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => return self.stop(),
                },
            };
            let alive = match msg {
                Msg::Stdin(line) => self.on_agent(line),
                Msg::StdinEof => {
                    // Answers already owed are still delivered, within the
                    // same bound the in-process serve used.
                    self.drain_until = Some(Instant::now() + budget);
                    true
                }
                Msg::StdoutGone => false,
                Msg::Owner(epoch, line) => self.on_owner(epoch, &line),
                Msg::OwnerGone(epoch) => self.owner_gone(epoch, self.drain_until.is_none()),
                Msg::Connected(id, result) => self.connected(id, result),
            };
            if !alive {
                return self.stop();
            }
        }
    }

    fn stop(&mut self) -> Option<Instant> {
        self.connecting.take();
        self.disconnect();
        self.drain_until
    }

    /// One agent frame: route it with this worktree's file and forward it,
    /// or answer it here.
    fn on_agent(&mut self, line: String) -> bool {
        let Ok(mut frame) = crate::deep_json::from_str(&line) else {
            // Parse failures have no request id to track in `pending`.
            // Answer here so an immediately queued stdin EOF cannot drop
            // the response while it is still on its way from the owner.
            return self.to_agent(
                &json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": "parse error"},
                })
                .to_string(),
            );
        };
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        // Every id-bearing frame but a notification is answered by the
        // owner — also one with an empty, missing or non-string method
        // ("method not found") — so it gets an owner-link id like any
        // request: its answer must never be matched against another
        // pending request whose owner-link id it happens to share. Frames
        // without request ids and notifications pass unchanged.
        let id = frame
            .0
            .get_mut("id")
            .map(|id| OwnedValue(std::mem::take(id)));
        let Some(id) = id.filter(|_| !method.starts_with("notifications/")) else {
            return self.forward(&line);
        };
        if method == "initialize" {
            self.initialize = Some(line.clone());
        }
        if method == "tools/call"
            && let Err(message) = self.route(&mut frame.0)
        {
            return self.to_agent(
                &crate::deep_json::to_string(&local_error(id, &method, &message).0)
                    .expect("Value serializes"),
            );
        }
        self.forward_request(frame, id, method)
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
        // Match the owner's typed validation before a rejected request
        // can affect this relay's sole-file default or admission checks.
        let read = crate::dispatch::ReadCommand::from_tool_args(&name, args)?;
        if read.is_none()
            && !matches!(
                name.as_str(),
                "figmog_files" | "figmog_open" | "figmog_sync" | "figmog_images"
            )
        {
            // MCP rejects unknown names before examining arguments.
            return Ok(());
        }
        crate::dispatch::validate_object_args(args)?;
        if name == "figmog_files" {
            return Ok(());
        }
        if name == "figmog_open" {
            let file = crate::dispatch::require_str(args, "file")?;
            crate::dispatch::arg_bool(args, "geometry")?;
            if let Some(key) = parse_file_ref(&file) {
                shared_store(&self.layout, &key)?;
                self.referenced.insert(key);
            }
            return Ok(());
        }
        if name == "figmog_images" {
            crate::dispatch::require_str_array(args, "ids")?;
            crate::dispatch::arg_str(args, "format")?;
            crate::dispatch::arg_f64(args, "scale")?;
        }
        let explicit = crate::dispatch::arg_str(args, "file")?;
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
            drop(OwnedValue(std::mem::replace(
                &mut params["arguments"],
                json!({}),
            )));
        }
        params["arguments"]["file"] = json!(key);
        Ok(())
    }

    /// One owner frame: forward notifications, and answers under the
    /// agent's own id; drop answers to the relay's own requests; re-send a
    /// request the owner did not accept.
    fn on_owner(&mut self, epoch: u64, line: &str) -> bool {
        let Ok(mut frame) = crate::deep_json::from_str(line) else {
            return true;
        };
        if frame.get("method").is_some() {
            // Notifications from the current connection only (a replaced
            // connection's would be duplicates or stale).
            return epoch != self.epoch || self.to_agent(line);
        }
        let Some(owner_id) = frame["id"].as_str().map(str::to_string) else {
            // An answer to no request of ours (a frame the owner could not
            // parse): the agent's, as is.
            return self.to_agent(line);
        };
        let Some(mut sent) = self.pending.remove(&owner_id) else {
            return self.to_agent(line);
        };
        if sent.agent_id.is_none() {
            // The relay's own request (its replayed `initialize`).
            return true;
        }
        if frame["error"]["code"].as_i64() == Some(owner::NOT_ACCEPTED)
            && sent.resends < MAX_RESENDS
        {
            // That owner is shutting down: stop writing to it.
            if sent.epoch == self.epoch {
                self.disconnect();
            }
            let resent = Sent {
                resends: sent.resends + 1,
                ..sent
            };
            return self.send(owner_id, resent);
        }
        frame.insert("id".into(), sent.agent_id.take().expect("checked above"));
        self.to_agent(&crate::deep_json::to_string(&frame.0).expect("Value serializes"))
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
            let Some(Sent {
                agent_id: Some(agent_id),
                method,
                ..
            }) = self.pending.remove(&key)
            else {
                continue;
            };
            let answer = local_error(
                agent_id,
                &method,
                "outcome unknown: the background figmog exited before answering; check \
                 figmog_status or figmog_files before retrying",
            );
            if !self.to_agent(&crate::deep_json::to_string(&answer.0).expect("Value serializes")) {
                return false;
            }
        }
        if epoch == self.epoch {
            self.disconnect();
            if reconnect {
                self.begin_connect();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn check_buffered_reply_before_write_failure(queue_failure: bool, not_accepted: bool) {
        use std::time::Duration;

        let (_dir, mut relay, _old_peer) = configured_relay();
        relay.disconnect();
        let (tx, rx) = mpsc::channel();
        relay.tx = tx;
        let (answers, received) = mpsc::channel();
        relay.stdout.take().unwrap().finish(Instant::now());
        relay.stdout = Some(super::super::output::StdioWriter::spawn(move |frame| {
            answers.send(frame.to_owned()).is_ok()
        }));
        let (read, paused) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (writer, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        relay
            .install_with_reader_hook(
                OwnerConn {
                    reader: BufReader::new(writer.try_clone().unwrap()),
                    writer,
                    info: owner::OwnerInfo {
                        pid: 42,
                        generation: "reply-order".into(),
                        settings: None,
                        attach_errors: vec![],
                    },
                },
                move || {
                    read.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(2)).unwrap();
                },
            )
            .unwrap();
        // A refusal queues its retry without starting an unrelated owner.
        relay.connecting = Some(Connecting {
            id: 1,
            cancel: Arc::default(),
        });
        assert!(
            relay.on_agent(
                json!({"jsonrpc":"2.0", "id":"agent-A", "method":"tools/list"}).to_string()
            )
        );
        let mut line = String::new();
        BufReader::new(peer.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let reply = if not_accepted {
            json!({"jsonrpc":"2.0", "id":request["id"],
                "error":{"code":owner::NOT_ACCEPTED,"message":"closing"}})
        } else {
            json!({"jsonrpc":"2.0", "id":request["id"], "result":{"answer":"A"}})
        };
        writeln!(peer, "{reply}").unwrap();
        paused.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(peer);
        if queue_failure {
            // Stop the writer normally, then fail the event loop's queue
            // send independently of a socket write failure.
            relay.owner_output.take();
            let (sender, receiver) = mpsc::channel();
            drop(receiver);
            relay.owner_output = Some(sender);
        }
        assert!(
            relay.on_agent(
                json!({"jsonrpc":"2.0", "id":"agent-B", "method":"tools/list"}).to_string()
            )
        );
        if !queue_failure {
            // Receiver teardown proves the real output worker has failed
            // its socket write before the reader may enqueue A's reply.
            let deadline = Instant::now() + Duration::from_secs(2);
            while relay
                .owner_output
                .as_ref()
                .unwrap()
                .send(String::new())
                .is_ok()
            {
                assert!(Instant::now() < deadline, "writer did not fail");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        release.send(()).unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                Msg::Owner(epoch, line) => assert!(relay.on_owner(epoch, &line)),
                Msg::OwnerGone(epoch) => {
                    assert!(relay.owner_gone(epoch, false));
                    break;
                }
                _ => panic!("unexpected relay event"),
            }
        }
        relay
            .stdout
            .take()
            .unwrap()
            .finish(Instant::now() + Duration::from_secs(2));
        let answers: Vec<Value> = received
            .try_iter()
            .map(|frame| serde_json::from_str(&frame).unwrap())
            .collect();
        if not_accepted {
            assert_eq!(answers.len(), 1, "{answers:?}");
            let Some(Outgoing::Request(id, sent)) = relay.outgoing.pop_front() else {
                panic!("buffered refusal lost its retry");
            };
            assert_eq!(id, request["id"].as_str().unwrap());
            assert_eq!(sent.agent_id.as_ref().unwrap().as_str(), Some("agent-A"));
            assert_eq!(sent.resends, 1);
        } else {
            assert_eq!(answers.len(), 2, "{answers:?}");
            assert_eq!(answers[0]["id"], "agent-A", "{answers:?}");
            assert_eq!(answers[0]["result"]["answer"], "A", "{answers:?}");
        }
        let unknown = answers.last().unwrap();
        assert_eq!(unknown["id"], "agent-B", "{answers:?}");
        assert!(
            unknown["error"]["message"]
                .as_str()
                .unwrap()
                .contains("outcome unknown")
        );
        assert!(relay.pending.is_empty());
    }

    #[test]
    fn buffered_owner_replies_precede_write_failure_cleanup() {
        for queue_failure in [false, true] {
            check_buffered_reply_before_write_failure(queue_failure, false);
        }
    }

    #[test]
    fn buffered_not_accepted_replies_survive_write_failures() {
        for queue_failure in [false, true] {
            check_buffered_reply_before_write_failure(queue_failure, true);
        }
    }

    #[test]
    fn not_accepted_reconnect_during_eof_keeps_the_original_deadline() {
        use std::time::Duration;
        let (_dir, mut relay, _old_peer) = configured_relay();
        let _lock = owner::OwnerLock::try_acquire(&relay.paths)
            .unwrap()
            .unwrap();
        let listener = owner::bind_socket(&relay.paths.socket()).unwrap();
        owner::write_discovery(
            &relay.paths,
            &owner::Discovery {
                socket: relay.paths.socket(),
                pid: 42,
                generation: "stalled-replacement".into(),
                version: owner::version().into(),
                protocol: owner::PROTOCOL,
            },
        )
        .unwrap();
        let (hello, entered) = mpsc::channel();
        let (closed, finished) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains(owner::HELLO));
            hello.send(()).unwrap();
            // The replacement never answers HELLO. Cancellation must
            // close it rather than waiting its normal sixty seconds.
            closed
                .send(reader.read_line(&mut String::new()).unwrap())
                .unwrap();
        });
        let (tx, rx) = mpsc::channel();
        relay.tx = tx.clone();
        relay.pending.insert(
            "rejected".into(),
            Sent {
                agent_id: Some(OwnedValue(json!(7))),
                method: "tools/list".into(),
                frame: json!({"jsonrpc":"2.0", "id":"rejected", "method":"tools/list"}).to_string(),
                epoch: relay.epoch,
                resends: 0,
            },
        );
        tx.send(Msg::StdinEof).unwrap();
        tx.send(Msg::Owner(relay.epoch, json!({"jsonrpc":"2.0", "id":"rejected", "error":{"code":owner::NOT_ACCEPTED,"message":"closing"}}).to_string())).unwrap();
        let started = Instant::now();
        let deadline = relay
            .run_with_drain(&rx, Duration::from_millis(500))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(deadline <= started + Duration::from_secs(1));
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(finished.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
        assert!(relay.connecting.is_none());
        assert!(relay.writer.is_none());
        assert!(
            !relay.paths.log().exists(),
            "no owner launched after cancellation"
        );
        peer.join().unwrap();
        std::fs::remove_file(relay.paths.socket()).unwrap();
    }

    #[test]
    fn a_late_connection_cannot_replay_or_resend_after_eof_deadline() {
        use std::time::Duration;
        let (_dir, mut relay, _old_peer) = configured_relay();
        relay.initialize =
            Some(json!({"jsonrpc":"2.0", "id":1, "method":"initialize"}).to_string());
        relay.drain_until = Some(Instant::now() - Duration::from_millis(1));
        relay.connecting = Some(Connecting {
            id: 1,
            cancel: Arc::default(),
        });
        let (writer, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let connection = OwnerConn {
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            info: owner::OwnerInfo {
                pid: 42,
                generation: "late".into(),
                settings: None,
                attach_errors: vec![],
            },
        };
        assert!(!relay.connected(1, Ok(connection)));
        assert_eq!(relay.epoch, 1);
        let mut reader = BufReader::new(peer);
        assert_eq!(reader.read_line(&mut String::new()).unwrap(), 0);
    }

    fn configured_relay() -> (tempfile::TempDir, Relay, UnixStream) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::discover(dir.path()).unwrap();
        let paths = OwnerPaths::ensure(&layout.mirror_root).unwrap();
        let (writer, reader) = UnixStream::pair().unwrap();
        let (tx, _rx) = mpsc::channel();
        let relay = Relay {
            layout,
            paths,
            settings: Settings {
                watch: false,
                interval: 10,
                upstream: None,
            },
            attach: vec![],
            default_key: Some("ABC1234567890".into()),
            referenced: BTreeSet::new(),
            initialize: None,
            writer: Some(writer.try_clone().unwrap()),
            owner_output: Some({
                let (tx, rx) = mpsc::channel::<String>();
                let mut writer = writer;
                std::thread::spawn(move || {
                    for frame in rx {
                        writeln!(writer, "{frame}").unwrap();
                    }
                });
                tx
            }),
            connecting: None,
            next_connect: 0,
            outgoing: VecDeque::new(),
            drain_until: None,
            stdout: Some(super::super::output::StdioWriter::spawn(|_| true)),
            epoch: 1,
            pending: BTreeMap::new(),
            tx,
            next_id: 0,
        };
        (dir, relay, reader)
    }

    #[test]
    fn malformed_agent_frames_are_answered_without_forwarding_to_the_owner() {
        let (_dir, mut relay, mut reader) = configured_relay();
        assert!(relay.on_agent("{\n".into()));
        reader.set_nonblocking(true).unwrap();
        let mut byte = [0];
        assert_eq!(
            std::io::Read::read(&mut reader, &mut byte)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(relay.pending.is_empty());
    }

    #[test]
    fn stalled_agent_output_does_not_block_eof_or_extend_the_flush_deadline() {
        use std::time::Duration;

        let (_dir, mut relay, _reader) = configured_relay();
        let (entered, writing) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        relay.stdout.take().unwrap().finish(Instant::now());
        relay.stdout = Some(super::super::output::StdioWriter::spawn(move |_| {
            entered.send(()).unwrap();
            resume.recv().unwrap();
            done.send(()).unwrap();
            true
        }));
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Stdin("{\n".into())).unwrap();
        tx.send(Msg::StdinEof).unwrap();
        let started = Instant::now();
        let deadline = relay.run(&rx).expect("EOF establishes the drain deadline");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(deadline > started);
        writing.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        relay
            .stdout
            .take()
            .unwrap()
            .finish(started + Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn valid_json_with_a_null_id_remains_a_tracked_request() {
        let (_dir, mut relay, reader) = configured_relay();
        assert!(relay.on_agent(json!({"jsonrpc": "2.0", "id": null}).to_string()));
        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).unwrap();
        let forwarded: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(forwarded["id"], "figmog-relay-0");
        assert!(
            relay.pending["figmog-relay-0"]
                .agent_id
                .as_ref()
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn deep_agent_arguments_keep_worktree_routing_and_request_id_remapping() {
        let (_dir, mut relay, reader) = configured_relay();
        let value = format!("{}0{}", "[".repeat(512), "]".repeat(512));
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":"agent","method":"tools/call","params":{{"name":"figmog_search","arguments":{{"query":"deep","equals":{value}}}}}}}"#
        );
        assert!(relay.on_agent(request));
        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).unwrap();
        let forwarded = crate::deep_json::from_str(&line).unwrap();
        assert_eq!(forwarded["params"]["arguments"]["file"], "ABC1234567890");
        assert_eq!(forwarded["id"], "figmog-relay-0");
        assert_eq!(
            relay.pending["figmog-relay-0"]
                .agent_id
                .as_ref()
                .unwrap()
                .as_str(),
            Some("agent")
        );
    }
    #[test]
    fn local_argument_type_errors_are_not_normalized_into_valid_requests() {
        let (_dir, mut relay, _reader) = configured_relay();
        for name in [
            "figmog_sync",
            "figmog_status",
            "figmog_open",
            "figmog_files",
        ] {
            for args in [json!([]), json!(true), json!(1), json!("invalid")] {
                let mut frame = json!({"params": {"name": name, "arguments": args}});
                let error = relay.route(&mut frame).unwrap_err();
                assert!(error.contains("object"), "{name}: {error}");
                assert_eq!(frame["params"]["arguments"], args);
            }
        }
        assert!(relay.referenced.is_empty());
    }
}
