//! Post-write store compaction (NWS-2941): move a mirror's data out of
//! fjall's journal into its tables, so a cold open stops replaying it.
//!
//! fold opens fjall with default settings and never flushes memtables
//! itself, so after a large pull everything it wrote lives only in the
//! journal and every later open re-reads all of it (a 100,001-node mirror:
//! a 95 MB journal, ~1.35 s before `figmog path 0:1` could answer). This
//! module reopens a *closed* store with fjall's own API and:
//!
//! 1. flushes every keyspace's memtable into tables;
//! 2. commits one tiny row to [`MAINTENANCE_KEYSPACE`] — after a reopen
//!    the append-mode journal's stream position is 0 until something is
//!    written, and fjall seals the active journal only during a flush that
//!    finds that position past its 64 MB threshold
//!    ([`JOURNAL_SEAL_BYTES`], `worker_pool.rs`);
//! 3. flushes that keyspace, so fjall seals the old journal and — every
//!    keyspace now having persisted past it — deletes it;
//! 4. fsyncs and closes.
//!
//! Every step is an ordinary fjall operation with fjall's own crash
//! safety: an interruption at any point leaves a store fjall recovers
//! normally (at worst with the old journal still in place, replayed as
//! before). The result is ordinary fjall data that any figmog (including
//! one without this module) opens unchanged; `figmog_maintenance` is not a
//! pipeline sink (fold names those `sink_*`) and fold never opens it, so
//! the frozen sink schema is untouched.
//!
//! A journal under the threshold can't be sealed this way; it is left
//! alone (no reopen beyond the size check, no flush), since its replay is
//! proportionally cheap. `FIGMOG_NO_COMPACT=1` disables compaction.
//!
//! `rotate_memtable` is `#[doc(hidden)]` in fjall 3.1.9 (public,
//! undocumented); fjall is pinned `=3.1.9`, and the pin-bump checklist in
//! `AGENTS.md` requires re-verifying it — `journal_shrinks_after_compaction`
//! below fails if the journal stops being retired.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use fjall::{KeyspaceCreateOptions, PersistMode, SingleWriterTxDatabase};

/// figmog's own keyspace for store maintenance bookkeeping. Not a pipeline
/// sink; fold never opens it.
pub(crate) const MAINTENANCE_KEYSPACE: &str = "figmog_maintenance";

/// fjall 3.1.9 seals the active journal during a flush only once the
/// journal writer's position exceeds this (`worker_pool.rs`). Journals
/// smaller than this are left alone.
pub(crate) const JOURNAL_SEAL_BYTES: u64 = 64_000_000;

/// Disables compaction when set to anything but empty or `0`.
pub(crate) const DISABLE_ENV: &str = "FIGMOG_NO_COMPACT";

/// The one row step 2 writes (constant, so repeated compactions store the
/// same content).
const MARKER_KEY: &str = "compacted";

/// How long to wait for fjall's flush worker to delete the sealed journal.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(15);
const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// What a [`compact`] call did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// `FIGMOG_NO_COMPACT` is set.
    Disabled,
    /// The journal is under [`JOURNAL_SEAL_BYTES`]; nothing was written.
    Small,
    /// The journal was flushed into tables and retired.
    Compacted { before: u64, after: u64 },
}

/// Whether `FIGMOG_NO_COMPACT` disables compaction.
pub(crate) fn disabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Bytes the fjall journal files (`<n>.jnl`) directly in `path` occupy on
/// disk. fjall preallocates a new journal's length (64 MiB) with
/// `set_len`, which leaves it sparse until written, so each file counts
/// the smaller of its length and its allocated blocks — on a filesystem
/// without sparse files this over-counts a never-reopened journal, which
/// only costs the post-open check in [`compact`].
pub(crate) fn journal_bytes(path: &Path) -> Result<u64, String> {
    let mut total = 0;
    let entries = std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", path.display()))?;
        if entry.file_name().to_string_lossy().ends_with(".jnl") {
            let metadata = entry.metadata().map_err(|e| e.to_string())?;
            if metadata.is_file() {
                total += metadata.len().min(metadata.blocks() * 512);
            }
        }
    }
    Ok(total)
}

/// Compact the closed store at `path` (see the module docs).
///
/// The caller must hold the store's ownership anchor (`.figmog-lock`) for
/// the whole call, and no fold/fjall handle on the store may be open — an
/// open one (in this process or another) makes fjall's own open fail with
/// its lock error, and nothing is touched.
pub(crate) fn compact(path: &Path) -> Result<Outcome, String> {
    if disabled() {
        return Ok(Outcome::Disabled);
    }
    // Cheap pre-check before paying for a journal replay: an over-count at
    // worst (see `journal_bytes`), so a journal this small can't be sealed.
    if journal_bytes(path)? < JOURNAL_SEAL_BYTES {
        return Ok(Outcome::Small);
    }
    fn err(step: &'static str) -> impl Fn(fjall::Error) -> String {
        move |e| format!("compacting store ({step}): {e}")
    }
    // fold's own configuration: the default builder.
    let db = SingleWriterTxDatabase::builder(path)
        .open()
        .map_err(err("open"))?;
    // Recovery truncated any preallocated tail: the journal's real size.
    let before = journal_bytes(path)?;
    if before < JOURNAL_SEAL_BYTES {
        return Ok(Outcome::Small);
    }

    let deadline = Instant::now() + RETIRE_TIMEOUT;

    // 1. Flush every keyspace's memtable into tables.
    let mut names: Vec<String> = db
        .list_keyspace_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    names.sort();
    for name in &names {
        let keyspace = db
            .keyspace(name, KeyspaceCreateOptions::default)
            .map_err(err("keyspace"))?;
        flush(&db, keyspace.inner(), deadline)?;
    }
    crash_point("flushed");

    // 2. One tiny write, moving the journal writer's position past fjall's
    //    sealing threshold.
    let maintenance = db
        .keyspace(MAINTENANCE_KEYSPACE, KeyspaceCreateOptions::default)
        .map_err(err("maintenance keyspace"))?;
    let mut tx = db.write_tx();
    tx.insert(&maintenance, MARKER_KEY, "1");
    tx.commit().map_err(err("marker"))?;
    crash_point("marked");

    // 3. Flush again: fjall seals the old journal, then deletes it once
    //    every keyspace has persisted past it.
    flush(&db, maintenance.inner(), deadline)?;
    wait_for_progress(deadline, || {
        db.persist(PersistMode::Buffer)
            .map_err(err("journal retirement"))?;
        Ok(db.journal_count())
    })?;
    crash_point("retired");

    // 4. Persist and close.
    db.persist(PersistMode::SyncAll).map_err(err("persist"))?;
    drop(db);
    Ok(Outcome::Compacted {
        before,
        after: journal_bytes(path)?,
    })
}

/// Rotate without fjall's unbounded wait helper. Persist checks the public
/// poison signal as well as reporting journal I/O errors.
fn flush(
    db: &SingleWriterTxDatabase,
    keyspace: &fjall::Keyspace,
    deadline: Instant,
) -> Result<(), String> {
    db.persist(PersistMode::Buffer)
        .map_err(|e| format!("compacting store (flush): {e}"))?;
    keyspace
        .rotate_memtable()
        .map_err(|e| format!("compacting store (flush): {e}"))?;
    wait_for_progress(deadline, || {
        db.persist(PersistMode::Buffer)
            .map_err(|e| format!("compacting store (flush): {e}"))?;
        // Add one so the same bounded wait also serves journal retirement.
        Ok(keyspace.sealed_memtable_count() + 1)
    })
}

fn wait_for_progress(
    deadline: Instant,
    mut remaining: impl FnMut() -> Result<usize, String>,
) -> Result<(), String> {
    let mut previous = usize::MAX;
    let mut progressed = Instant::now();
    loop {
        let count = remaining()?;
        if count <= 1 {
            return Ok(());
        }
        if count < previous {
            progressed = Instant::now();
            previous = count;
        }
        if Instant::now() >= deadline || progressed.elapsed() >= STALL_TIMEOUT {
            return Err("compacting store: flush did not finish within its time budget; leaving the recoverable journal in place".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Test-only: abort the process at `step` when the crash-safety test's
/// child asks for it (`FIGMOG_TEST_COMPACT_ABORT`).
fn crash_point(step: &str) {
    #[cfg(test)]
    if std::env::var("FIGMOG_TEST_COMPACT_ABORT").as_deref() == Ok(step) {
        std::process::abort();
    }
    #[cfg(not(test))]
    let _ = step;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Id, ProxyCacheRec, Rec};
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn failed_or_expired_flush_stops_without_waiting_out_the_budget() {
        let started = Instant::now();
        let error = wait_for_progress(started + Duration::from_secs(15), || Err("Poisoned".into()))
            .unwrap_err();
        assert_eq!(error, "Poisoned");
        assert!(started.elapsed() < Duration::from_secs(1));
        let error = wait_for_progress(Instant::now(), || Ok(2)).unwrap_err();
        assert!(error.contains("time budget"));
    }

    const FIXTURE_PATH_ENV: &str = "FIGMOG_TEST_COMPACT_FIXTURE_PATH";

    fn large_rows() -> Vec<(Id, Rec)> {
        (0..48)
            .map(|i| {
                let key = format!("synthetic-{i:03}");
                let rec = ProxyCacheRec {
                    key_hash: key.clone(),
                    tool: "synthetic".into(),
                    args_canonical: "{}".into(),
                    file_version: "1".into(),
                    // Incompressible-ish synthetic payload, ~1 MB per row.
                    content: (0..1_000_000u32)
                        .map(|j| char::from(b'a' + ((j * 7 + i) % 26) as u8))
                        .collect(),
                };
                (Id::ProxyCache(key), Rec::ProxyCache(rec))
            })
            .collect()
    }

    /// Build the committed fixture without depending on fjall's Drop:
    /// under load, its blocking Close send was observed stuck after the
    /// last worker exited. Checkpoint then exit the child; the parent still
    /// exercises the real compaction, locking and recovery assertions.
    fn large_store(path: &Path) -> Vec<(Id, Rec)> {
        let rows = large_rows();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "compact::tests::populate_large_store_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FIXTURE_PATH_ENV, path)
            .env_remove("FIGMOG_TEST_COMPACT_CHILD")
            .env_remove("FIGMOG_TEST_COMPACT_ABORT")
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start compaction fixture builder");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    assert!(
                        status.success(),
                        "compaction fixture builder failed: {status}"
                    );
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                outcome => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "compaction fixture builder did not finish within its budget: {outcome:?}"
                    );
                }
            }
        }
        rows
    }

    #[test]
    fn populate_large_store_fixture() {
        let Some(path) = std::env::var_os(FIXTURE_PATH_ENV) else {
            return;
        };
        let rows = large_rows();
        let mut st = crate::open_store!(Path::new(&path));
        st.wtx(|tx| {
            for (id, rec) in &rows {
                tx.upsert(id, rec);
            }
            tx.upsert(
                &Id::Meta,
                &Rec::Meta(crate::model::FileMeta {
                    name: "synthetic".into(),
                    version: "1".into(),
                    last_modified: String::new(),
                    synced_at_unix_ms: 0,
                }),
            );
        });
        st.checkpoint();
        std::process::exit(0);
    }

    fn manager(dir: &Path, db: &Path) -> crate::sessions::SessionManager {
        let key = "AAA1234567890".to_string();
        let session =
            crate::sessions::open_session_at(db.to_path_buf(), key.clone(), None, None).unwrap();
        crate::sessions::SessionManager {
            sessions: vec![session],
            root: dir.to_path_buf().into(),
            token: None,
            default_key: Some(key),
            sole_default: true,
            api: None,
        }
    }

    /// `figmog serve`'s shutdown compacts each store once its session and
    /// views are gone, under the store's ownership.
    #[test]
    fn serve_shutdown_compacts_its_stores() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let rows = large_store(&db);
        let mut manager = manager(dir.path(), &db);
        manager.close_reads(Instant::now() + Duration::from_secs(5));
        manager.close_and_compact(Instant::now() + Duration::from_secs(30));
        assert!(manager.sessions.is_empty());
        assert!(journal_bytes(&db).unwrap() < 1_000_000);
        let expected: Vec<_> = rows.iter().map(|(_, rec)| Some(rec.clone())).collect();
        assert_eq!(read_all(&db, &rows), expected);
    }

    /// A read still holding a view at shutdown keeps fjall's own lock, so
    /// compaction leaves that store exactly as it was.
    #[test]
    fn serve_shutdown_skips_a_store_a_detached_read_still_holds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let rows = large_store(&db);
        let mut manager = manager(dir.path(), &db);
        let lease = manager.sessions[0].lease().unwrap();
        let before = journal_bytes(&db).unwrap();
        manager.close_and_compact(Instant::now() + Duration::from_secs(30));
        assert_eq!(journal_bytes(&db).unwrap(), before);
        drop(lease);
        let expected: Vec<_> = rows.iter().map(|(_, rec)| Some(rec.clone())).collect();
        assert_eq!(read_all(&db, &rows), expected);
    }

    fn read_all(path: &Path, rows: &[(Id, Rec)]) -> Vec<Option<Rec>> {
        let st = crate::open_store!(path);
        rows.iter().map(|(id, _)| st.get(id)).collect()
    }

    #[test]
    fn journal_shrinks_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let rows = large_store(&db);
        let outcome = compact(&db).unwrap();
        let Outcome::Compacted { before, after } = outcome else {
            panic!("expected a compaction, got {outcome:?}");
        };
        assert!(before >= JOURNAL_SEAL_BYTES, "{before}");
        assert!(
            after < 1_000_000,
            "journal was not retired: {before} -> {after} bytes"
        );
        // The data survives, and a store compacted again stays small.
        let expected: Vec<_> = rows.iter().map(|(_, rec)| Some(rec.clone())).collect();
        assert_eq!(read_all(&db, &rows), expected);
        assert!(journal_bytes(&db).unwrap() < 1_000_000);
        assert_eq!(compact(&db).unwrap(), Outcome::Small);
    }

    #[test]
    fn small_journals_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let mut st = crate::open_store!(&db);
        st.wtx(|tx| {
            tx.upsert(
                &Id::Meta,
                &Rec::Meta(crate::model::FileMeta {
                    name: "synthetic".into(),
                    version: "1".into(),
                    last_modified: String::new(),
                    synced_at_unix_ms: 0,
                }),
            )
        });
        drop(st);
        assert_eq!(compact(&db).unwrap(), Outcome::Small);
        let st = crate::open_store!(&db);
        assert!(st.get(&Id::Meta).is_some());
    }

    #[test]
    fn compaction_refuses_a_store_that_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        large_store(&db);
        let _open = crate::open_store!(&db);
        let error = compact(&db).unwrap_err();
        assert!(error.contains("Locked"), "{error}");
    }

    /// A process killed at each step of a compaction leaves a store fjall
    /// opens normally, with every row intact.
    #[test]
    fn a_crash_during_compaction_leaves_a_readable_store() {
        if let Some(path) = std::env::var_os("FIGMOG_TEST_COMPACT_CHILD") {
            compact(Path::new(&path)).unwrap();
            return;
        }
        for step in ["flushed", "marked", "retired"] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("db");
            let rows = large_store(&db);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "compact::tests::a_crash_during_compaction_leaves_a_readable_store",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("FIGMOG_TEST_COMPACT_CHILD", &db)
                .env("FIGMOG_TEST_COMPACT_ABORT", step)
                .env_remove(DISABLE_ENV)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(
                status.signal(),
                Some(libc::SIGABRT),
                "{step}: child did not reach the abort point: {status}"
            );
            let expected: Vec<_> = rows.iter().map(|(_, rec)| Some(rec.clone())).collect();
            assert_eq!(read_all(&db, &rows), expected, "after a crash at {step}");
            // And a later compaction still completes the job.
            assert!(matches!(
                compact(&db).unwrap(),
                Outcome::Compacted { .. } | Outcome::Small
            ));
            assert!(journal_bytes(&db).unwrap() < 1_000_000, "{step}");
            assert_eq!(read_all(&db, &rows), expected, "{step}");
        }
    }
}
