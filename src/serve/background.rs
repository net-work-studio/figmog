//! The serve loop's side of the repository's background owner (NWS-2953):
//! the handshake, idle shutdown, and the "not accepted" answer of an owner
//! that is shutting down. See [`crate::owner`] for the election, socket
//! and discovery, and [`super::relay`] for the stdio relays it serves.
//!
//! **Idle shutdown.** The owner exits by itself once it has had no open
//! connection and no unanswered or running work for its idle timeout. It
//! then first *closes*: every new request (including a handshake) is
//! answered with [`crate::owner::NOT_ACCEPTED`] — nothing is started for
//! it, so the client may safely reconnect and retry it with the next owner
//! — while accepted work (deferred replies, CLI writes) finishes; then the
//! loop exits, stores are closed and compacted (NWS-2941), the socket and
//! discovery record are removed, and only then is the owner lock released.
//! The same close runs if its socket or lock file disappears or is
//! replaced (an owner that cannot be found, or whose lock another process
//! could take, must not keep running).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::deep_json::OwnedValue;
use serde_json::{Value, json};

use crate::owner::{self, OwnerLock, OwnerPaths, Settings};
use crate::sessions::SessionManager;

/// How often the owner re-checks that its socket and lock file are still
/// its own.
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// A background owner's loop state.
pub(crate) struct OwnerState {
    pub(crate) paths: OwnerPaths,
    lock: Arc<OwnerLock>,
    /// The socket this owner bound, as `(path, dev, ino)`.
    socket: (std::path::PathBuf, u64, u64),
    pub(crate) generation: String,
    pub(crate) settings: Settings,
    idle_after: Duration,
    idle_since: Option<Instant>,
    /// Shutting down: new requests are not accepted.
    pub(crate) closing: bool,
    next_check: Instant,
}

impl OwnerState {
    pub(crate) fn new(
        paths: OwnerPaths,
        lock: Arc<OwnerLock>,
        socket: &super::SocketGuard,
        generation: String,
        settings: Settings,
        idle_after: Duration,
    ) -> Self {
        Self {
            paths,
            lock,
            socket: (socket.path.clone(), socket.dev, socket.ino),
            generation,
            settings,
            idle_after,
            idle_since: None,
            closing: false,
            next_check: Instant::now() + CHECK_EVERY,
        }
    }

    /// Advance the idle clock (`busy`: a connection is open or work is
    /// outstanding) and the periodic self-checks; may start closing.
    pub(crate) fn tick(&mut self, now: Instant, busy: bool) {
        if self.closing {
            return;
        }
        if busy {
            self.idle_since = None;
        } else {
            let since = *self.idle_since.get_or_insert(now);
            if now.duration_since(since) >= self.idle_after {
                eprintln!(
                    "figmog: no clients for {}ms; background owner shutting down",
                    self.idle_after.as_millis()
                );
                self.closing = true;
                return;
            }
        }
        if now >= self.next_check {
            self.next_check = now + CHECK_EVERY;
            let (path, dev, ino) = &self.socket;
            let socket_ok = std::fs::symlink_metadata(path).is_ok_and(|m| {
                use std::os::unix::fs::MetadataExt;
                m.dev() == *dev && m.ino() == *ino
            });
            if !socket_ok {
                eprintln!(
                    "figmog: socket {} disappeared or was replaced; background owner shutting down",
                    path.display()
                );
                self.closing = true;
            } else if !self.lock.still_valid() {
                eprintln!(
                    "figmog: owner lock {} disappeared or was replaced; background owner shutting down",
                    self.paths.lock().display()
                );
                self.closing = true;
            }
        }
    }

    /// Answer a handshake: refuse another project or figmog build;
    /// otherwise open the relay's startup files (`attach`) like `figmog
    /// serve FILE...` did, queueing a first pull for missing mirrors when
    /// watching. Returns the response and whether the client is a relay.
    pub(crate) fn hello(
        &self,
        id: OwnedValue,
        params: &Value,
        manager: &mut SessionManager,
        interval: Duration,
    ) -> (OwnedValue, bool) {
        if let Err(message) = owner::check_hello(params, &self.paths) {
            let mut response = OwnedValue(
                json!({"jsonrpc": "2.0", "error": {"code": owner::HANDSHAKE_REFUSED, "message": message}}),
            );
            response.insert("id".into(), id);
            return (response, false);
        }
        let mut attach_errors = serde_json::Map::new();
        for file in params["attach"].as_array().into_iter().flatten() {
            let Some(file) = file.as_str() else { continue };
            match manager.open(file) {
                Ok(session) => {
                    if self.settings.watch && !session.mirrored {
                        session.request_pull(false, interval);
                    }
                }
                Err(error) => {
                    attach_errors.insert(file.to_string(), json!(error));
                }
            }
        }
        let mut response = OwnedValue(json!({"jsonrpc": "2.0", "result": {
            "attach_errors": attach_errors,
            "generation": self.generation,
            "pid": std::process::id(),
            "project": self.paths.identity(),
            "protocol": owner::PROTOCOL,
            "settings": self.settings.to_json(),
            "version": owner::version(),
        }}));
        response.insert("id".into(), id);
        (response, params["client"] == "relay")
    }
}

/// The answer to every request an owner receives once it is closing.
pub(crate) fn not_accepted(id: OwnedValue) -> OwnedValue {
    let mut response = OwnedValue(json!({"jsonrpc": "2.0", "error": {
        "code": owner::NOT_ACCEPTED,
        "message": "not accepted: this background figmog is shutting down; reconnect and retry",
    }}));
    response.insert("id".into(), id);
    response
}

/// Parse without a depth cutoff and retain iterative owners for unbounded ids/params.
pub(crate) fn request_parts(line: &str) -> Option<(OwnedValue, String, OwnedValue)> {
    let mut value = crate::deep_json::from_str(line).ok()?;
    let method = value.get("method")?.as_str()?.to_string();
    let id = OwnedValue(std::mem::take(value.0.get_mut("id")?));
    let params = OwnedValue(
        value
            .0
            .get_mut("params")
            .map(std::mem::take)
            .unwrap_or(Value::Null),
    );
    Some((id, method, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closes_after_the_idle_timeout_only_while_idle_and_on_a_lost_socket() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OwnerPaths::ensure(dir.path()).unwrap();
        let lock = Arc::new(OwnerLock::try_acquire(&paths).unwrap().unwrap());
        let socket = dir.path().join("o.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let guard = super::super::SocketGuard::new(socket.clone()).unwrap();
        let settings = Settings {
            watch: false,
            interval: 10,
            upstream: None,
        };
        let idle = Duration::from_millis(100);
        let mut state = OwnerState::new(paths, lock, &guard, "g".into(), settings.clone(), idle);
        let t0 = Instant::now();
        state.tick(t0, false);
        state.tick(t0 + Duration::from_millis(90), true);
        // Busy reset the clock.
        state.tick(t0 + Duration::from_millis(150), false);
        assert!(!state.closing);
        state.tick(t0 + Duration::from_millis(249), false);
        assert!(!state.closing);
        state.tick(t0 + Duration::from_millis(250), false);
        assert!(state.closing);

        // A replaced socket closes a busy owner at its next check.
        let paths = OwnerPaths::ensure(&dir.path().join("b")).unwrap();
        let lock = Arc::new(OwnerLock::try_acquire(&paths).unwrap().unwrap());
        let mut state = OwnerState::new(paths, lock, &guard, "g".into(), settings, idle);
        std::fs::remove_file(&socket).unwrap();
        state.tick(Instant::now() + CHECK_EVERY, true);
        assert!(state.closing);
    }

    #[test]
    fn not_accepted_is_a_distinct_error_for_any_request() {
        let answer = not_accepted(OwnedValue(json!(7)));
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["error"]["code"], owner::NOT_ACCEPTED);
        assert_eq!(
            request_parts(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
                .map(|(id, method, _)| (id.into_inner(), method)),
            Some((json!(1), "tools/list".to_string()))
        );
        assert!(request_parts(r#"{"jsonrpc":"2.0","method":"notifications/x"}"#).is_none());
        assert!(request_parts("not json").is_none());
    }
}
