//! CLI writers through a running serve (NWS-2963): `figmog pull` (plain,
//! `--geometry`, `--from-file`, `--fresh`) and `figmog import-variables`
//! become operations of the process that owns the store.
//!
//! **Tool surface.** The operations are internal tools of the socket
//! control plane only: they are never listed by `tools/list` (an agent's
//! tool surface is unchanged — agents keep `figmog_open`/`figmog_sync`) and
//! stdio refuses them as unknown tools. The CLI is their only intended
//! client.
//!
//! **Operations and receipts.** Each accepted write is an operation with id
//! `<generation>-<seq>`: the generation names this serve process (pid and
//! start time, hex), the sequence counts from 1. Before anything else the
//! requester's connection gets a `notifications/figmog/operation` frame
//! carrying that id; the `tools/call` itself is answered (a deferred
//! response, NWS-2955) only once the write committed and its view was
//! published. Validation failures change nothing; a failure after a fresh
//! clear is explicitly reported as an unknown outcome. The answer is
//! `{"operation": id, "result": ...}`. Every finished operation's outcome
//! is also kept as a receipt for [`RECEIPT_RETENTION`], readable with
//! [`OPERATION`] `{id}` from any connection (a reconnecting client); a
//! running one reads as `running`. Distinct writes are never merged; only
//! identical plain pulls that have not started and are adjacent in the
//! queue share one worker run (each keeps its own operation id). Nothing
//! here retries a write; at shutdown a write still queued is answered "not
//! accepted" (nothing applied), which the CLI may retry.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::dispatch;
use crate::sessions::{PullSource, WriteOp};

/// `figmog pull` through serve. Arguments: `file` (key, required),
/// `geometry`, `fresh` (booleans) and `from_file` (a saved
/// `GET /v1/files/:key` response, sent as content; omitted ⇒ network).
pub(crate) const PULL: &str = "figmog_internal_pull";
/// `figmog import-variables` through serve. Arguments: `file` (key),
/// `content` (the export's text, exactly as read) and `name` (how
/// validation errors name it — the CLI's path).
pub(crate) const IMPORT_VARIABLES: &str = "figmog_internal_import_variables";
/// A write's receipt by operation id: `{id}`.
pub(crate) const OPERATION: &str = "figmog_internal_operation";

/// How long a finished operation's receipt is kept.
pub(crate) const RECEIPT_RETENTION: Duration = Duration::from_secs(600);

/// The notification announcing an accepted write's operation id to its
/// requester, ahead of the write's response.
pub(crate) const OPERATION_NOTIFICATION: &str = "notifications/figmog/operation";

/// Whether `name` is one of the hidden socket-only tools.
pub(crate) fn is_internal(name: &str) -> bool {
    matches!(name, PULL | IMPORT_VARIABLES | OPERATION)
}

/// Store paths cross the local socket as Unix bytes, preserving non-UTF8
/// checkout names. The string spelling remains accepted for older clients.
pub(crate) fn expected_db(args: &Value) -> Result<std::path::PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    if let Some(value) = args.get("db_bytes") {
        let bytes = value
            .as_array()
            .ok_or_else(|| "db_bytes must be an array of bytes".to_string())?
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|byte| u8::try_from(byte).ok())
                    .ok_or_else(|| "db_bytes must contain bytes between 0 and 255".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if bytes.is_empty() || bytes.contains(&0) {
            return Err("db_bytes must be a non-empty Unix path without NUL bytes".into());
        }
        return Ok(std::ffi::OsString::from_vec(bytes).into());
    }
    dispatch::require_str(args, "db").map(Into::into)
}

/// Parse a write tool's arguments: the target file key and the operation.
pub(crate) fn parse(name: &str, args: &Value) -> Result<(String, WriteOp), String> {
    let file = dispatch::require_str(args, "file")?;
    let op = if name == PULL {
        WriteOp::Pull {
            source: match args.get("from_file") {
                None => PullSource::Network,
                Some(response) => PullSource::File(
                    crate::deep_json::from_str(
                        &crate::deep_json::to_string(response)
                            .map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?,
                ),
            },
            fresh: dispatch::arg_bool(args, "fresh")?,
            geometry: dispatch::arg_bool(args, "geometry")?,
        }
    } else {
        WriteOp::Import {
            content: dispatch::require_str(args, "content")?.into_bytes(),
            label: dispatch::require_str(args, "name")?,
            allow_empty: dispatch::arg_bool(args, "allow_empty")?,
            dry_run: dispatch::arg_bool(args, "dry_run")?,
        }
    };
    Ok((file, op))
}

/// Preview an import when there is no destination database to inspect.
/// Runs off the serve loop and never acquires or initializes a store.
pub(crate) fn preview_empty_import(op: WriteOp) -> Result<Value, String> {
    let WriteOp::Import { content, label, .. } = op else {
        unreachable!("import preview")
    };
    let invalid = |error| format!("invalid variables file {label}: {error}");
    let value: Value =
        serde_json::from_slice(&content).map_err(|error| invalid(error.to_string()))?;
    crate::vars::check_import_entries(&value).map_err(|error| invalid(error.to_string()))?;
    let parsed = crate::vars::parse_variables_snapshot(
        &value,
        crate::vars::SOURCE_FILE,
        crate::vars::snapshot_digest(&content),
    )
    .map_err(|error| invalid(error.to_string()))?;
    let imported = parsed
        .recs
        .iter()
        .filter(|(id, _)| matches!(id, crate::model::Id::Variable(_)))
        .count();
    Ok(json!({"imported": imported, "removed": 0, "removed_ids": []}))
}

/// Operation ids and receipts for this process.
pub(crate) struct Operations {
    generation: String,
    next_seq: u64,
    /// Running operations by deferred-call token.
    running: BTreeMap<u64, String>,
    /// Finished operations: when they finished and their receipt.
    receipts: BTreeMap<String, (Instant, Value)>,
    retention: Duration,
}

/// This process's generation: its pid and start time (hex). Names its
/// operations and, for a background owner (NWS-2953), the owner itself.
pub(crate) fn new_generation() -> String {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{:x}{:x}", std::process::id(), started)
}

impl Default for Operations {
    fn default() -> Self {
        Self::with_generation(new_generation())
    }
}

impl Operations {
    /// Operations named after `generation`.
    pub(crate) fn with_generation(generation: String) -> Self {
        Self {
            generation,
            next_seq: 1,
            running: BTreeMap::new(),
            receipts: BTreeMap::new(),
            retention: RECEIPT_RETENTION,
        }
    }
}

impl Operations {
    /// A new operation for the deferred call `token`.
    pub(crate) fn begin(&mut self, token: u64) -> String {
        let id = format!("{}-{}", self.generation, self.next_seq);
        self.next_seq += 1;
        self.running.insert(token, id.clone());
        id
    }

    /// Record the receipt of the operation behind `token`, if it is one.
    /// `outcome` is its answer: `{"operation", "result"}` or the error.
    pub(crate) fn finish(&mut self, token: u64, outcome: Result<&Value, &str>) {
        let Some(id) = self.running.remove(&token) else {
            return;
        };
        let now = Instant::now();
        self.prune(now);
        let receipt = match outcome {
            Ok(value) => json!({"id": id, "state": "succeeded", "result": value["result"]}),
            Err(error) => json!({"id": id, "state": "failed", "error": error}),
        };
        self.receipts.insert(id, (now, receipt));
    }

    /// Whether `token` is a running write's.
    pub(crate) fn is_running(&self, token: u64) -> bool {
        self.running.contains_key(&token)
    }

    /// [`OPERATION`]: the receipt, `running`, or an error for an id this
    /// process never issued or no longer keeps.
    pub(crate) fn query(&mut self, args: &Value) -> Result<Value, String> {
        let id = dispatch::require_str(args, "id")?;
        self.prune(Instant::now());
        if let Some((_, receipt)) = self.receipts.get(&id) {
            return Ok(receipt.clone());
        }
        if self.running.values().any(|running| *running == id) {
            return Ok(json!({"id": id, "state": "running"}));
        }
        Err(format!(
            "unknown operation {id}: not issued by this serve process, or its receipt expired (receipts are kept {}s)",
            self.retention.as_secs()
        ))
    }

    fn prune(&mut self, now: Instant) {
        let retention = self.retention;
        self.receipts
            .retain(|_, (finished, _)| now.duration_since(*finished) < retention);
    }
}

/// The notification frame announcing operation `id`.
pub(crate) fn notification(id: &str) -> Value {
    json!({"jsonrpc": "2.0", "method": OPERATION_NOTIFICATION, "params": {"id": id}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids_are_distinct_and_receipts_expire() {
        let mut ops = Operations::default();
        let a = ops.begin(1);
        let b = ops.begin(2);
        assert_ne!(a, b);
        assert!(a.ends_with("-1") && b.ends_with("-2"));
        assert_eq!(
            ops.query(&json!({"id": a})).unwrap(),
            json!({"id": a, "state": "running"})
        );
        ops.finish(1, Ok(&json!({"operation": a, "result": {"added": 1}})));
        ops.finish(2, Err("boom"));
        assert_eq!(
            ops.query(&json!({"id": a})).unwrap(),
            json!({"id": a, "state": "succeeded", "result": {"added": 1}})
        );
        assert_eq!(
            ops.query(&json!({"id": b})).unwrap(),
            json!({"id": b, "state": "failed", "error": "boom"})
        );
        ops.retention = Duration::ZERO;
        assert!(ops.query(&json!({"id": a})).is_err());
        assert!(ops.query(&json!({"id": "nope"})).is_err());
    }

    #[test]
    fn saved_null_is_file_content_even_for_fresh_pulls() {
        for fresh in [false, true] {
            let (_, op) = parse(
                PULL,
                &json!({"file": "ABC1234567890", "fresh": fresh, "from_file": null}),
            )
            .unwrap();
            assert!(matches!(
                op,
                WriteOp::Pull { source: PullSource::File(response), .. } if response.is_null()
            ));
        }
        let (_, op) = parse(PULL, &json!({"file": "ABC1234567890"})).unwrap();
        assert!(matches!(
            op,
            WriteOp::Pull {
                source: PullSource::Network,
                ..
            }
        ));
    }

    #[test]
    fn write_arguments_parse_into_distinct_operations() {
        let (file, op) = parse(
            PULL,
            &json!({"file": "ABC1234567890", "fresh": true, "from_file": {"name": "x"}}),
        )
        .unwrap();
        assert_eq!(file, "ABC1234567890");
        assert!(matches!(
            op,
            WriteOp::Pull {
                source: PullSource::File(_),
                fresh: true,
                geometry: false
            }
        ));
        let (_, op) = parse(
            IMPORT_VARIABLES,
            &json!({"file": "ABC1234567890", "content": "{}", "name": "v.json"}),
        )
        .unwrap();
        assert!(matches!(op, WriteOp::Import { .. }));
        assert!(parse(PULL, &json!({})).is_err());
        assert!(parse(PULL, &json!({"file": "ABC1234567890", "fresh": "yes"})).is_err());
    }
}

#[cfg(test)]
mod deep_tests {
    use super::*;

    #[test]
    fn saved_file_request_parsing_and_drop_are_safe_for_deep_geometry() {
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let value = format!("{}0{}", "[".repeat(4096), "]".repeat(4096));
                let args = crate::deep_json::from_str(&format!(
                    r#"{{"file":"ABC1234567890","from_file":{{"geometry":{value}}}}}"#
                ))
                .unwrap();
                let (_, op) = parse(PULL, &args).unwrap();
                assert!(matches!(
                    op,
                    WriteOp::Pull {
                        source: PullSource::File(_),
                        ..
                    }
                ));
                drop(op);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn expected_database_path_preserves_unix_bytes_and_rejects_invalid_byte_arrays() {
        let bytes = vec![b'/', b't', b'm', b'p', b'/', 255];
        assert_eq!(
            expected_db(&json!({"db_bytes": bytes}))
                .unwrap()
                .as_os_str()
                .as_bytes(),
            bytes
        );
        for value in [
            json!([]),
            json!([0]),
            json!([256]),
            json!([-1]),
            json!("path"),
        ] {
            assert!(expected_db(&json!({"db_bytes": value})).is_err());
        }
    }
}
