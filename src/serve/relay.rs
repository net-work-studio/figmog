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
    /// A frame from the owner connection of this epoch.
    Owner(u64, String),
    /// The owner connection of this epoch ended.
    OwnerGone(u64),
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
    epoch: u64,
    pending: BTreeMap<String, Sent>,
    tx: mpsc::Sender<Msg>,
    /// The next owner-link request id's number.
    next_id: u64,
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
    if settings.watch
        && let Some(key) = &default_key
        && !attach.contains(key)
    {
        shared_store(&layout, key)?;
        attach.push(key.clone());
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

    /// Reconnect if the owner connection is gone. `Err`: no owner.
    fn ensure_connected(&mut self) -> Result<(), String> {
        if self.writer.is_none() {
            self.connect().map_err(|error| {
                eprintln!("figmog: {error}");
                error
            })?;
        }
        Ok(())
    }

    /// Send a notification (or a frame the owner must reject) as is.
    fn forward(&mut self, frame: &str) -> bool {
        if self.ensure_connected().is_ok() {
            let _ = self.write_owner(frame);
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
        if let Err(error) = self.ensure_connected() {
            return match sent.agent_id {
                Some(id) => to_agent(
                    &crate::deep_json::to_string(&local_error(id, &sent.method, &error).0)
                        .expect("Value serializes"),
                ),
                None => true,
            };
        }
        sent.epoch = self.epoch;
        let frame = sent.frame.clone();
        self.pending.insert(owner_id, sent);
        // On a failed write the reader reports the lost connection, and
        // its pending requests (this one included) are answered then.
        let _ = self.write_owner(&frame);
        true
    }

    fn run(&mut self, rx: &mpsc::Receiver<Msg>) {
        let mut drain_until: Option<Instant> = None;
        loop {
            if let Some(deadline) = drain_until
                && (!self.pending.values().any(|sent| sent.agent_id.is_some())
                    || Instant::now() >= deadline)
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
        let Ok(mut frame) = crate::deep_json::from_str(&line) else {
            // Parse failures have no request id to track in `pending`.
            // Answer here so an immediately queued stdin EOF cannot drop
            // the response while it is still on its way from the owner.
            return to_agent(
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
            return to_agent(
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
        crate::dispatch::validate_object_args(args)?;
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
            return epoch != self.epoch || to_agent(line);
        }
        let Some(owner_id) = frame["id"].as_str().map(str::to_string) else {
            // An answer to no request of ours (a frame the owner could not
            // parse): the agent's, as is.
            return to_agent(line);
        };
        let Some(mut sent) = self.pending.remove(&owner_id) else {
            return to_agent(line);
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
                self.writer = None;
            }
            let resent = Sent {
                resends: sent.resends + 1,
                ..sent
            };
            return self.send(owner_id, resent);
        }
        frame.insert("id".into(), sent.agent_id.take().expect("checked above"));
        to_agent(&crate::deep_json::to_string(&frame.0).expect("Value serializes"))
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
            if !to_agent(&crate::deep_json::to_string(&answer.0).expect("Value serializes")) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

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
            writer: Some(writer),
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
            r#"{{"jsonrpc":"2.0","id":"agent","method":"tools/call","params":{{"name":"figmog_search","arguments":{{"equals":{value}}}}}}}"#
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
