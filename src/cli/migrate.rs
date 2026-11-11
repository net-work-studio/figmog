//! `figmog migrate`: move old per-checkout mirrors (`<project>/.figmog/<key>/`)
//! into the shared mirror root (spec §16).
//!
//! Per file key, with `L` the old store directory, `D` the shared one and
//! `S` = `<mirror root>/.<key>.migrating` a staging directory beside `D`
//! (same filesystem, so the final rename is atomic):
//!
//! 0. take the repository-wide migration lock `<mirror root>/<key>.migrate.lock`
//!    (every worktree shares `D` and `S`), held until the end — a second
//!    migration of the key, from any worktree, is refused while it is held;
//! 1. refuse if `D` already exists (never overwrite a mirror);
//! 2. lock `L/db` (figmog's anchor *and* fjall's own writer lock, so a
//!    running serve — even an older binary — makes this fail cleanly), then
//!    close/quiesce the store, then retain the anchor and an exclusive lock
//!    on fjall's `db/lock` file (a competing opener winning that transition
//!    makes migration fail before copying);
//! 3. remove any `S` left by an interrupted run (safe: the migration lock
//!    excludes a live one), copy `L` into `S` (the lock anchor itself is not
//!    copied), lock `S/db` with a fresh anchor and write the
//!    [`MIGRATED_MARKER`] naming `L`;
//! 4. rename `S` to `D` — the held anchor moves with it, so `D` is owned
//!    from the moment it appears;
//! 5. rename `L` to its tombstone `T` = `<checkout>/.figmog/.<key>.deleting`
//!    (same directory, so atomic; both writer exclusions remain held),
//!    remove `T` recursively, then the
//!    marker.
//!
//! Interrupted states and what a rerun does: a crash in 3 leaves `L`
//! intact (still the active store) plus a partial `S`, which the rerun
//! discards and redoes; a crash after 4 leaves `D` with the marker, which
//! the active-store rules report as an interrupted migration and a rerun
//! finishes by doing 5 under both stores' anchors and fjall writer locks —
//! but only from the checkout the marker names: another worktree's own old
//! mirror is an independent copy, so there the rerun refuses and names the
//! checkout that must finish (or, if the recorded source is already gone,
//! just removes its tombstone, if any, and the marker). A tombstone is never
//! opened as a store, only removed, so a deletion interrupted half way is
//! simply finished (NWS-2964). An `L` an older binary's interrupted step 5
//! left half deleted in place is still checked for live writers before it
//! is retired, but a failure to open it as a store is not one.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::ident::is_file_key;
use crate::project::{Layout, MIGRATED_MARKER};

#[cfg(test)]
use super::open_store_checked;
use super::store_lock::{LOCK_NAME, MigrationLock, MigrationOwner, StoreOwner};

/// One moved (or finished) mirror.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Moved {
    pub(super) key: String,
    pub(super) from: PathBuf,
    pub(super) to: PathBuf,
    /// Finished a previously interrupted move rather than starting one.
    pub(super) resumed: bool,
}

pub(super) fn cmd_migrate(layout: &Layout, key: Option<String>) -> Result<Value, String> {
    let keys = match key {
        Some(key) => BTreeSet::from([key]),
        None => candidate_keys(layout)?,
    };
    let mut migrated = Vec::new();
    let mut failed = Vec::new();
    for key in keys {
        match migrate_key(layout, &key) {
            Ok(Some(moved)) => migrated.push(json!({
                "file": moved.key,
                "from": moved.from.display().to_string(),
                "to": moved.to.display().to_string(),
                "resumed": moved.resumed,
            })),
            Ok(None) => {}
            Err(error) => failed.push(json!({"file": key, "error": error})),
        }
    }
    Ok(json!({
        "mirror_root": layout.mirror_root.display().to_string(),
        "migrated": migrated,
        "failed": failed,
    }))
}

fn staging_dir(layout: &Layout, key: &str) -> PathBuf {
    layout.mirror_root.join(format!(".{key}.migrating"))
}

/// Where step 5 moves the old store `legacy` before deleting it:
/// `.<name>.deleting` beside it, derived from the path the marker records.
fn tombstone(legacy: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(legacy.file_name().unwrap_or_default());
    name.push(".deleting");
    legacy.with_file_name(name)
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Old stores, interrupted moves and leftover staging dirs, sorted.
fn candidate_keys(layout: &Layout) -> Result<BTreeSet<String>, String> {
    let mut keys = BTreeSet::new();
    if !layout.has_legacy_root() {
        return Ok(keys);
    }
    let names = |dir: &Path| -> Result<Vec<String>, String> {
        match fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|e| {
                    e.map(|e| e.file_name().to_string_lossy().into_owned())
                        .map_err(|e| format!("reading {}: {e}", dir.display()))
                })
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("reading {}: {e}", dir.display())),
        }
    };
    for root in layout.legacy_roots() {
        if root == layout.mirror_root {
            continue;
        }
        for name in names(&root)? {
            if is_file_key(&name) && exists(&root.join(&name).join("db")) {
                keys.insert(name);
            } else if let Some(key) = name
                .strip_prefix('.')
                .and_then(|n| n.strip_suffix(".deleting"))
                .filter(|k| is_file_key(k))
            {
                keys.insert(key.to_string());
            }
        }
    }
    for name in names(&layout.mirror_root)? {
        if is_file_key(&name) && exists(&layout.shared_dir(&name).join(MIGRATED_MARKER)) {
            keys.insert(name);
        } else if let Some(key) = name
            .strip_prefix('.')
            .and_then(|n| n.strip_suffix(".migrating"))
            .filter(|k| is_file_key(k))
        {
            keys.insert(key.to_string());
        }
    }
    Ok(keys)
}

/// `<mirror root>/<key>.migrate.lock`: the repository-wide per-key
/// migration lock (not a file key, so never mistaken for a mirror).
fn migration_lock_path(layout: &Layout, key: &str) -> PathBuf {
    layout.mirror_root.join(format!("{key}.migrate.lock"))
}

/// Recover and close a store, retaining figmog's anchor and then taking
/// fjall's raw writer lock without reopening background workers. A raw
/// opener that wins this transition makes the lock attempt fail safely.
fn lock_store(db: &Path) -> Result<MigrationOwner, String> {
    lock_store_after_close(db, || Ok(()))
}

fn lock_store_after_close(
    db: &Path,
    closed: impl FnOnce() -> Result<(), String>,
) -> Result<MigrationOwner, String> {
    let owner = StoreOwner::acquire_existing(db)?
        .ok_or_else(|| format!("store moved while migration started: {}", db.display()))?;
    let owner = super::open_store_owned(owner, |path| crate::open_store!(path))?.into_owner();
    closed()?;
    MigrationOwner::acquire(owner, false)
}

/// Take both writer exclusions for cleanup without opening or repairing
/// potentially torn data. If deletion already removed `db` or `db/lock`,
/// recreate only that lock barrier inside the existing tree: raw fjall
/// create-new openers must also be excluded before the tree is retired.
/// Never recreate an absent tree or its ancestors.
fn lock_leftover(db: &Path) -> Result<Option<MigrationOwner>, String> {
    let owner = match StoreOwner::acquire_existing(db)? {
        Some(owner) => owner,
        None => {
            let parent = db.parent().ok_or("store has no parent directory")?;
            let Some(parent) = crate::private_dir::existing(parent)? else {
                return Ok(None);
            };
            let db = parent.join("db");
            match fs::DirBuilder::new().mode(0o700).create(&db) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!("preparing cleanup lock {}: {error}", db.display()));
                }
            }
            StoreOwner::acquire_existing(&db)?
                .ok_or_else(|| format!("store moved while cleanup started: {}", db.display()))?
        }
    };
    MigrationOwner::acquire(owner, true).map(Some)
}

/// Step 5: atomically move the (locked, checked) old store `legacy` to its
/// tombstone — replacing a stale one — then remove the tombstone.
fn retire(legacy: &Path, owner: &MigrationOwner) -> Result<(), String> {
    retire_with_hook(legacy, owner, || Ok(()))
}

fn retire_with_hook(
    legacy: &Path,
    owner: &MigrationOwner,
    renamed: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let tomb = tombstone(legacy);
    remove_tombstone(&tomb)?;
    owner.validate_at(&legacy.join("db"))?;
    fs::rename(legacy, &tomb).map_err(|e| format!("renaming {}: {e}", legacy.display()))?;
    if let Some(parent) = legacy.parent() {
        sync_path(parent)?;
    }
    renamed()?;
    // The original inode is now under T. Never revisit L: an older binary
    // may create a new independent store there after the rename.
    owner.validate_at(&tomb.join("db"))?;
    remove_tree(&tomb)
}

/// Remove a leftover tree while excluding raw fjall openers. It is never
/// opened as a database, so partially deleted trees remain recoverable.
fn remove_tombstone(tomb: &Path) -> Result<(), String> {
    if exists(tomb) {
        let owner = if fs::symlink_metadata(tomb).is_ok_and(|metadata| metadata.is_dir()) {
            lock_leftover(&tomb.join("db"))?
        } else {
            None
        };
        if let Some(owner) = &owner {
            owner.validate_at(&tomb.join("db"))?;
        }
        remove_tree(tomb)?;
    }
    Ok(())
}

fn remove_tree(tree: &Path) -> Result<(), String> {
    fs::remove_dir_all(tree).map_err(|error| format!("removing {}: {error}", tree.display()))
}

/// The source path a [`MIGRATED_MARKER`] records (raw path bytes).
fn read_marker(marker: &Path) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = fs::read(marker).map_err(|e| format!("reading {}: {e}", marker.display()))?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// Same directory, whether spelled identically or through equivalent paths.
fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (fs::canonicalize(left), fs::canonicalize(right)),
            (Ok(l), Ok(r)) if l == r
        )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrationEvent {
    SourceClosed,
    BeforeCopy,
    BeforePublish,
    BeforeRetire,
}

pub(super) fn migrate_key(layout: &Layout, key: &str) -> Result<Option<Moved>, String> {
    migrate_key_with_hook(layout, key, |_| Ok(()))
}

fn migrate_key_with_hook(
    layout: &Layout,
    key: &str,
    mut admitted: impl FnMut(MigrationEvent) -> Result<(), String>,
) -> Result<Option<Moved>, String> {
    if !is_file_key(key) {
        return Err(format!("not a Figma file key: {key}"));
    }
    if !layout.has_legacy_root() {
        // Outside Git the old and shared locations are the same directory.
        return Ok(None);
    }
    let sources = layout.legacy_stores(key);
    if sources.len() > 1 {
        return Err(format!(
            "refusing to migrate {key}: multiple legacy mirrors exist at {}; move or remove redundant copies first",
            sources
                .iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let legacy = layout.legacy_dir(key);
    let dest = layout.shared_dir(key);
    let staging = staging_dir(layout, key);
    let io = |what: &str, path: &Path, e: std::io::Error| format!("{what} {}: {e}", path.display());

    // Every worktree shares the destination and the staging dir, so one
    // migration per key at a time, repository-wide, held until the end.
    crate::private_dir::ensure(&layout.mirror_root)?;
    let _migrating = MigrationLock::acquire(&migration_lock_path(layout, key), || {
        format!(
            "another `figmog migrate` of {key} is running (possibly from another worktree); \
             rerun once it finishes"
        )
    })?;

    let marker = dest.join(MIGRATED_MARKER);
    if exists(&marker) {
        // Step 5 of an interrupted run. The marker names the checkout whose
        // old mirror was moved; this checkout's own old mirror may be an
        // independent copy that must survive.
        let source = read_marker(&marker)?;
        let ours = layout
            .legacy_roots()
            .iter()
            .any(|root| root != &layout.mirror_root && same_path(&source, &root.join(key)));
        let legacy = source.clone();
        if !ours && exists(&source) {
            return Err(format!(
                "an interrupted `figmog migrate` of {key} moved the old mirror at {} into {}; \
                 finish it by running `figmog migrate` in that checkout",
                source.display(),
                dest.display()
            ));
        }
        // Finish under both stores' anchors and fjall writer locks, checked
        // on the old store before it moves to its tombstone.
        let dest_owner = lock_store(&dest.join("db"))?;
        if ours {
            let legacy_owner = lock_leftover(&legacy.join("db"))?;
            if exists(&legacy) {
                admitted(MigrationEvent::BeforeRetire)?;
                let owner = legacy_owner
                    .as_ref()
                    .ok_or("legacy store changed before cleanup")?;
                owner.validate_at(&legacy.join("db"))?;
                dest_owner.validate_at(&dest.join("db"))?;
                retire(&legacy, owner)?;
            }
        }
        // A tombstone is past the point of no return, whichever checkout
        // it is in: finish deleting it without opening it.
        remove_tombstone(&tombstone(&source))?;
        // The recorded source is gone now (or was already): drop the marker.
        fs::remove_file(&marker).map_err(|e| io("removing", &marker, e))?;
        return Ok(Some(Moved {
            key: key.to_string(),
            from: source,
            to: dest,
            resumed: true,
        }));
    }

    if !exists(&legacy.join("db")) {
        // Nothing to move; under the migration lock, a staging dir or a
        // tombstone is debris from an interrupted run.
        if exists(&staging) {
            remove_tombstone(&staging)?;
        }
        remove_tombstone(&tombstone(&legacy))?;
        return Ok(None);
    }
    if exists(&dest) {
        return Err(format!(
            "refusing to migrate {key}: {} already exists (old mirror left at {})",
            dest.display(),
            legacy.display()
        ));
    }

    let source = lock_store_after_close(&legacy.join("db"), || {
        admitted(MigrationEvent::SourceClosed)
    })?;
    // No other migration of this key can be running: any staging dir is
    // left by an interrupted one.
    if exists(&staging) {
        remove_tombstone(&staging)?;
    }
    admitted(MigrationEvent::BeforeCopy)?;
    source.validate_at(&legacy.join("db"))?;
    copy_tree(&legacy, &staging)?;
    let staged = MigrationOwner::acquire(StoreOwner::acquire(&staging.join("db"))?, false)?;
    let staged_marker = staging.join(MIGRATED_MARKER);
    {
        use std::os::unix::ffi::OsStrExt;
        fs::write(&staged_marker, legacy.as_os_str().as_bytes())
            .map_err(|e| io("writing", &staged_marker, e))?;
    }
    sync_path(&staged_marker)?;
    sync_path(&staging)?;
    admitted(MigrationEvent::BeforePublish)?;
    source.validate_at(&legacy.join("db"))?;
    staged.validate_at(&staging.join("db"))?;
    if exists(&dest) {
        return Err(format!(
            "refusing to migrate {key}: {} appeared during the copy",
            dest.display()
        ));
    }
    fs::rename(&staging, &dest).map_err(|e| io("renaming into", &dest, e))?;
    sync_path(&layout.mirror_root)?;
    admitted(MigrationEvent::BeforeRetire)?;
    staged.validate_at(&dest.join("db"))?;
    retire(&legacy, &source)?;
    drop(source);
    fs::remove_file(&marker).map_err(|e| io("removing", &marker, e))?;
    drop(staged);
    Ok(Some(Moved {
        key: key.to_string(),
        from: legacy,
        to: dest,
        resumed: false,
    }))
}

/// Copy a closed store directory (regular files and directories only; the
/// lock anchor is recreated by the destination's own owner), syncing every
/// file so the rename that publishes it never exposes unwritten data.
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(to)
        .map_err(|e| format!("creating {}: {e}", to.display()))?;
    let mut entries = fs::read_dir(from)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("reading {}: {e}", from.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let source = entry.path();
        let target = to.join(&name);
        let metadata = fs::symlink_metadata(&source)
            .map_err(|e| format!("reading {}: {e}", source.display()))?;
        if metadata.is_dir() {
            copy_tree(&source, &target)?;
        } else if metadata.is_file() {
            if name == LOCK_NAME {
                continue;
            }
            fs::copy(&source, &target).map_err(|e| format!("copying {}: {e}", source.display()))?;
            sync_path(&target)?;
        } else {
            return Err(format!(
                "refusing to migrate {}: not a regular file or directory",
                source.display()
            ));
        }
    }
    sync_path(to)
}

fn sync_path(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("syncing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{ActiveStore, ProjectConfig};

    const KEY: &str = "SyntheticKey0001";

    fn layout(dir: &Path) -> Layout {
        let dir = crate::private_dir::ensure(dir).unwrap();
        Layout {
            project_root: dir.clone(),
            config: ProjectConfig::default(),
            mirror_root: dir.join("git/figmog"),
            local_root: dir.join(".figmog"),
            legacy_roots: Vec::new(),
        }
    }

    /// A real (synthetic) store at the old location with a sentinel file.
    fn legacy_store(layout: &Layout) -> PathBuf {
        let db = layout.legacy_dir(KEY).join("db");
        drop(open_store_checked(&db, |path| crate::open_store!(path)).unwrap());
        fs::write(db.join("sentinel"), "synthetic").unwrap();
        db
    }

    #[test]
    fn migrate_moves_the_whole_store_and_the_shared_copy_opens() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        legacy_store(&layout);
        assert!(layout.active_store(KEY).unwrap().migration_pending);

        let out = cmd_migrate(&layout, None).unwrap();
        let dest = layout.shared_dir(KEY);
        assert_eq!(
            out,
            json!({
                "mirror_root": layout.mirror_root.display().to_string(),
                "failed": [],
                "migrated": [{
                    "file": KEY,
                    "from": layout.legacy_dir(KEY).display().to_string(),
                    "to": dest.display().to_string(),
                    "resumed": false,
                }],
            })
        );
        assert!(!layout.legacy_dir(KEY).exists());
        assert!(!dest.join(MIGRATED_MARKER).exists());
        assert!(!staging_dir(&layout, KEY).exists());
        assert_eq!(
            fs::read_to_string(dest.join("db/sentinel")).unwrap(),
            "synthetic"
        );
        assert_eq!(
            layout.active_store(KEY).unwrap(),
            ActiveStore {
                path: dest.join("db"),
                migration_pending: false
            }
        );
        drop(open_store_checked(&dest.join("db"), |path| crate::open_store!(path)).unwrap());
        // Idempotent: nothing left to do.
        assert_eq!(cmd_migrate(&layout, None).unwrap()["migrated"], json!([]));
    }

    #[test]
    fn migrate_refuses_to_overwrite_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let dest_db = layout.shared_dir(KEY).join("db");
        fs::create_dir_all(&dest_db).unwrap();
        fs::write(dest_db.join("other"), "keep").unwrap();
        let error = migrate_key(&layout, KEY).unwrap_err();
        assert!(error.contains("refusing to migrate"), "{error}");
        assert!(error.contains(&layout.shared_dir(KEY).display().to_string()));
        assert_eq!(fs::read_to_string(dest_db.join("other")).unwrap(), "keep");
        assert!(legacy_db.join("sentinel").exists());
    }

    #[test]
    fn migrate_refuses_while_the_old_store_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        // Held exactly like a running serve holds it.
        let held = open_store_checked(&legacy_db, |path| crate::open_store!(path)).unwrap();
        let error = migrate_key(&layout, KEY).unwrap_err();
        assert_eq!(error, super::super::STORE_LOCKED_MSG);
        assert!(!layout.shared_dir(KEY).exists());
        assert!(!staging_dir(&layout, KEY).exists());
        drop(held);
        // And fjall's own lock (an older binary without the anchor).
        let raw = crate::open_store!(&legacy_db);
        let error = migrate_key(&layout, KEY).unwrap_err();
        assert_eq!(error, super::super::STORE_LOCKED_MSG);
        drop(raw);
        assert!(migrate_key(&layout, KEY).unwrap().is_some());
    }

    #[test]
    fn rerun_completes_an_interrupted_copy() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        legacy_store(&layout);
        // Crash mid-copy: partial staging, old store intact and active.
        let staging = staging_dir(&layout, KEY);
        fs::create_dir_all(staging.join("db")).unwrap();
        fs::write(staging.join("db/partial"), "torn").unwrap();
        assert!(layout.active_store(KEY).unwrap().migration_pending);

        let moved = migrate_key(&layout, KEY).unwrap().unwrap();
        assert!(!moved.resumed);
        assert!(!staging.exists());
        assert!(!layout.shared_dir(KEY).join("db/partial").exists());
        assert!(layout.shared_dir(KEY).join("db/sentinel").exists());
        assert!(!layout.legacy_dir(KEY).exists());
    }

    /// A crash after the rename: the shared copy carries the marker naming
    /// `layout`'s old store, which is still there.
    fn interrupted_after_rename(layout: &Layout) -> PathBuf {
        let dest = layout.shared_dir(KEY);
        crate::private_dir::ensure(&layout.mirror_root).unwrap();
        copy_tree(&layout.legacy_dir(KEY), &dest).unwrap();
        fs::write(
            dest.join(MIGRATED_MARKER),
            layout.legacy_dir(KEY).display().to_string(),
        )
        .unwrap();
        dest
    }

    #[test]
    fn rerun_completes_an_interrupted_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let dest = interrupted_after_rename(&layout);
        let error = layout.active_store(KEY).unwrap_err();
        assert!(error.contains("rerun `figmog migrate`"), "{error}");

        // A held destination blocks the finish, too.
        let held = StoreOwner::acquire(&dest.join("db")).unwrap();
        assert_eq!(
            migrate_key(&layout, KEY).unwrap_err(),
            super::super::STORE_LOCKED_MSG
        );
        drop(held);

        let out = cmd_migrate(&layout, None).unwrap();
        assert_eq!(out["migrated"][0]["resumed"], true);
        assert!(!legacy_db.exists());
        assert!(!dest.join(MIGRATED_MARKER).exists());
        assert!(!layout.active_store(KEY).unwrap().migration_pending);
    }

    /// NWS-2964 finding 1: an older binary holds only fjall's writer lock,
    /// on either store; resuming must not delete under it.
    #[test]
    fn resumed_cleanup_checks_fjall_writer_locks_on_both_stores() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let dest = interrupted_after_rename(&layout);
        for db in [legacy_db.clone(), dest.join("db")] {
            let raw = crate::open_store!(&db);
            assert_eq!(
                migrate_key(&layout, KEY).unwrap_err(),
                super::super::STORE_LOCKED_MSG,
                "{}",
                db.display()
            );
            assert!(legacy_db.join("sentinel").exists());
            assert!(dest.join(MIGRATED_MARKER).exists());
            drop(raw);
        }
        assert!(migrate_key(&layout, KEY).unwrap().unwrap().resumed);
        assert!(!legacy_db.exists());
        assert!(!dest.join(MIGRATED_MARKER).exists());
    }

    fn raw_opener_is_refused(db: &Path) -> bool {
        match std::panic::catch_unwind(|| crate::open_store!(db)) {
            Ok(store) => {
                drop(store);
                false
            }
            Err(payload) => super::super::panic_message(&*payload).contains("Locked"),
        }
    }

    fn check_late_raw_openers(resumed: bool) {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let dest_db = layout.shared_dir(KEY).join("db");
        if resumed {
            interrupted_after_rename(&layout);
        }
        let (events, phases) = mpsc::channel();
        let (resume, proceed) = mpsc::channel();
        let migration = std::thread::spawn(move || {
            migrate_key_with_hook(&layout, KEY, |phase| {
                events.send(phase).map_err(|error| error.to_string())?;
                proceed
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|error| error.to_string())
            })
        });
        let mut excluded = Vec::new();
        loop {
            let phase = phases.recv_timeout(Duration::from_secs(30)).unwrap();
            if phase != MigrationEvent::SourceClosed {
                excluded.push((phase, legacy_db.clone(), raw_opener_is_refused(&legacy_db)));
                if phase == MigrationEvent::BeforeRetire {
                    excluded.push((phase, dest_db.clone(), raw_opener_is_refused(&dest_db)));
                }
            }
            resume.send(()).unwrap();
            if phase == MigrationEvent::BeforeRetire {
                break;
            }
        }
        assert!(migration.join().unwrap().unwrap().is_some());
        assert!(
            excluded.iter().all(|(_, _, refused)| *refused),
            "an older-style raw opener entered after migration admission: {excluded:?}"
        );
        assert!(!legacy_db.exists());
        assert!(dest_db.exists());
    }

    #[test]
    fn migration_excludes_late_raw_openers_during_copy_and_retirement() {
        check_late_raw_openers(false);
    }

    #[test]
    fn resumed_cleanup_excludes_late_raw_openers_before_retirement() {
        check_late_raw_openers(true);
    }

    #[test]
    fn an_older_writer_winning_the_close_to_lock_transition_stops_before_copying() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let mut older = None;
        let error = migrate_key_with_hook(&layout, KEY, |phase| {
            if phase == MigrationEvent::SourceClosed {
                let mut store = crate::open_store!(&legacy_db);
                crate::store::upsert_mirror_config(&mut store, true);
                older = Some(store);
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error, super::super::STORE_LOCKED_MSG);
        assert!(!staging_dir(&layout, KEY).exists());
        assert!(!layout.shared_dir(KEY).exists());
        assert!(legacy_db.join("sentinel").exists());
        drop(older);
        migrate_key(&layout, KEY).unwrap().unwrap();
        let store = crate::open_store!(layout.shared_dir(KEY).join("db"));
        assert!(store.rtx(|(.., config, _, _, _)| crate::store::read_geometry(&config)));
    }

    #[test]
    fn a_source_replaced_after_copy_is_not_published_or_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let legacy = layout.legacy_dir(KEY);
        let original = dir.path().join("original");
        let error = migrate_key_with_hook(&layout, KEY, |phase| {
            if phase == MigrationEvent::BeforePublish {
                fs::rename(&legacy, &original).unwrap();
                fs::create_dir_all(&legacy_db).unwrap();
                fs::write(legacy_db.join("replacement"), "keep").unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("store ownership changed"), "{error}");
        assert!(!layout.shared_dir(KEY).exists());
        assert!(original.join("db/sentinel").exists());
        assert_eq!(
            fs::read_to_string(legacy_db.join("replacement")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn retirement_never_removes_an_older_openers_new_legacy_directory() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let legacy = layout.legacy_dir(KEY);
        let held = lock_store(&legacy_db).unwrap();
        let mut recreated = None;
        retire_with_hook(&legacy, &held, || {
            let mut store = crate::open_store!(&legacy_db);
            crate::store::upsert_mirror_config(&mut store, true);
            recreated = Some(store);
            Ok(())
        })
        .unwrap();
        drop(recreated);
        assert!(!tombstone(&legacy).exists());
        let store = crate::open_store!(&legacy_db);
        assert!(store.rtx(|(.., config, _, _, _)| crate::store::read_geometry(&config)));
    }

    #[test]
    fn leftover_staging_and_tombstone_cleanup_exclude_raw_openers() {
        for tomb in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let layout = layout(dir.path());
            crate::private_dir::ensure(&layout.local_root).unwrap();
            let leftover = if tomb {
                tombstone(&layout.legacy_dir(KEY))
            } else {
                staging_dir(&layout, KEY)
            };
            let db = leftover.join("db");
            drop(open_store_checked(&db, |path| crate::open_store!(path)).unwrap());
            fs::write(db.join("sentinel"), "keep").unwrap();
            let older = crate::open_store!(&db);
            assert_eq!(
                migrate_key(&layout, KEY).unwrap_err(),
                super::super::STORE_LOCKED_MSG
            );
            assert_eq!(fs::read_to_string(db.join("sentinel")).unwrap(), "keep");
            drop(older);
            assert_eq!(migrate_key(&layout, KEY).unwrap(), None);
            assert!(!leftover.exists());
        }
    }

    #[test]
    fn resumed_cleanup_handles_a_source_whose_database_directory_is_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let dest = interrupted_after_rename(&layout);
        fs::remove_dir_all(&legacy_db).unwrap();
        assert!(migrate_key(&layout, KEY).unwrap().unwrap().resumed);
        assert!(!layout.legacy_dir(KEY).exists());
        assert!(!dest.join(MIGRATED_MARKER).exists());
        assert!(dest.join("db/sentinel").exists());
    }

    /// Delete roughly half of a store directory's entries (every other one,
    /// sorted, recursively), as an interrupted `remove_dir_all` would.
    fn half_delete(dir: &Path) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for (i, entry) in entries.iter().enumerate() {
            if entry.is_dir() {
                half_delete(entry);
            } else if i % 2 == 0 {
                fs::remove_file(entry).unwrap();
            }
        }
    }

    /// NWS-2964 follow-up: step 5 retires the old store to a tombstone
    /// before deleting it. A crash part way through that deletion leaves a
    /// half-deleted tombstone, which a rerun removes without opening it.
    #[test]
    fn rerun_finishes_a_partially_deleted_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let dest = interrupted_after_rename(&layout);
        let tomb = tombstone(&layout.legacy_dir(KEY));
        assert_eq!(tomb, layout.local_root.join(format!(".{KEY}.deleting")));
        fs::rename(layout.legacy_dir(KEY), &tomb).unwrap();
        half_delete(&tomb.join("db"));
        assert!(!legacy_db.exists());

        let moved = migrate_key(&layout, KEY).unwrap().unwrap();
        assert!(moved.resumed);
        assert!(!tomb.exists());
        assert!(!layout.legacy_dir(KEY).exists());
        assert!(!dest.join(MIGRATED_MARKER).exists());
        assert!(!layout.active_store(KEY).unwrap().migration_pending);

        // A stray tombstone with no marker left is debris, too.
        fs::create_dir_all(tomb.join("db")).unwrap();
        fs::write(tomb.join("db/torn"), "x").unwrap();
        assert_eq!(cmd_migrate(&layout, None).unwrap()["migrated"], json!([]));
        assert!(!tomb.exists());
    }

    /// An older binary deleted the old store in place and was interrupted:
    /// the half-deleted store is still checked for writers, but failing to
    /// open as a store does not stop the rerun from retiring it.
    #[test]
    fn rerun_finishes_an_old_versions_partially_deleted_source() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let dest = interrupted_after_rename(&layout);
        // Torn beyond what fjall recovers from: its journal directories and
        // every other file are gone, and one file is garbage.
        half_delete(&legacy_db);
        for entry in fs::read_dir(&legacy_db).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() && path.file_name().unwrap() != LOCK_NAME {
                fs::write(&path, b"torn").unwrap();
            }
        }
        // A held destination still refuses before anything moves.
        let error = {
            let held = crate::open_store!(&dest.join("db"));
            let error = migrate_key(&layout, KEY).unwrap_err();
            drop(held);
            error
        };
        assert_eq!(error, super::super::STORE_LOCKED_MSG);
        assert!(legacy_db.exists());

        let moved = migrate_key(&layout, KEY).unwrap().unwrap();
        assert!(moved.resumed);
        assert!(!layout.legacy_dir(KEY).exists());
        assert!(!tombstone(&layout.legacy_dir(KEY)).exists());
        assert!(!dest.join(MIGRATED_MARKER).exists());
    }

    /// A second worktree of the same repository: same mirror root, its own
    /// per-checkout root.
    fn other_worktree(layout: &Layout, dir: &Path) -> Layout {
        let root = crate::private_dir::ensure(&dir.join("worktree-b")).unwrap();
        Layout {
            project_root: root.clone(),
            config: ProjectConfig::default(),
            mirror_root: layout.mirror_root.clone(),
            local_root: root.join(".figmog"),
            legacy_roots: Vec::new(),
        }
    }

    /// NWS-2964 finding 2: the marker names worktree A's old mirror;
    /// worktree B's own old mirror is independent and must survive.
    #[test]
    fn resume_only_cleans_up_the_checkout_the_marker_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = layout(dir.path());
        let b = other_worktree(&a, dir.path());
        let a_db = legacy_store(&a);
        let b_db = legacy_store(&b);
        fs::write(b_db.join("sentinel"), "worktree b").unwrap();
        let dest = interrupted_after_rename(&a);

        let error = migrate_key(&b, KEY).unwrap_err();
        assert!(error.contains("in that checkout"), "{error}");
        assert!(error.contains(&a.legacy_dir(KEY).display().to_string()));
        assert_eq!(
            fs::read_to_string(b_db.join("sentinel")).unwrap(),
            "worktree b"
        );
        assert!(a_db.join("sentinel").exists());
        assert!(dest.join(MIGRATED_MARKER).exists());

        // A finishes its own move; B's copy is untouched.
        assert!(migrate_key(&a, KEY).unwrap().unwrap().resumed);
        assert!(!a_db.exists());
        assert_eq!(
            fs::read_to_string(b_db.join("sentinel")).unwrap(),
            "worktree b"
        );

        // A marker whose source is already gone is just removed from B,
        // still without touching B's own old mirror.
        fs::write(
            dest.join(MIGRATED_MARKER),
            a.legacy_dir(KEY).display().to_string(),
        )
        .unwrap();
        let moved = migrate_key(&b, KEY).unwrap().unwrap();
        assert!(moved.resumed);
        assert_eq!(moved.from, a.legacy_dir(KEY));
        assert!(!dest.join(MIGRATED_MARKER).exists());
        assert!(b_db.join("sentinel").exists());
    }

    /// NWS-2964 finding 3: the migration lock is repository-wide; while it
    /// is held a live migration's staging dir is never discarded.
    #[test]
    fn a_held_migration_lock_refuses_and_leaves_staging_alone() {
        let dir = tempfile::tempdir().unwrap();
        let a = layout(dir.path());
        let b = other_worktree(&a, dir.path());
        let b_db = legacy_store(&b);
        crate::private_dir::ensure(&a.mirror_root).unwrap();
        let held = MigrationLock::acquire(&migration_lock_path(&a, KEY), String::new).unwrap();
        let staging = staging_dir(&a, KEY);
        fs::create_dir_all(staging.join("db")).unwrap();
        fs::write(staging.join("db/live"), "worktree a").unwrap();

        let error = migrate_key(&b, KEY).unwrap_err();
        assert!(error.contains("another `figmog migrate`"), "{error}");
        assert!(staging.join("db/live").exists());
        assert!(b_db.join("sentinel").exists());
        assert!(!a.shared_dir(KEY).exists());
        drop(held);
    }

    /// NWS-2964 finding 3: two worktrees migrating the same key at once
    /// with different data — exactly one wins and the shared copy is
    /// entirely its data; the other's old mirror is left intact.
    #[test]
    fn concurrent_migrations_from_two_worktrees_never_mix_data() {
        for _ in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let a = layout(dir.path());
            let b = other_worktree(&a, dir.path());
            for (layout, text) in [(&a, "worktree a"), (&b, "worktree b")] {
                let db = legacy_store(layout);
                fs::write(db.join("sentinel"), text).unwrap();
            }
            let barrier = std::sync::Barrier::new(2);
            let (ra, rb) = std::thread::scope(|scope| {
                let barrier = &barrier;
                let run = move |layout: &Layout| {
                    barrier.wait();
                    migrate_key(layout, KEY)
                };
                let (a, b) = (&a, &b);
                let ta = scope.spawn(move || run(a));
                let tb = scope.spawn(move || run(b));
                (ta.join().unwrap(), tb.join().unwrap())
            });
            let (winner, loser, loser_err) = match (&ra, &rb) {
                (Ok(Some(_)), Err(e)) => (("worktree a", &a), &b, e),
                (Err(e), Ok(Some(_))) => (("worktree b", &b), &a, e),
                other => panic!("expected exactly one success: {other:?}"),
            };
            assert!(
                loser_err.contains("another `figmog migrate`")
                    || loser_err.contains("refusing to migrate"),
                "{loser_err}"
            );
            let dest = a.shared_dir(KEY);
            assert_eq!(
                fs::read_to_string(dest.join("db/sentinel")).unwrap(),
                winner.0
            );
            assert!(!dest.join(MIGRATED_MARKER).exists());
            assert!(!staging_dir(&a, KEY).exists());
            assert!(!winner.1.legacy_dir(KEY).exists());
            assert!(loser.legacy_dir(KEY).join("db/sentinel").exists());
        }
    }

    /// NWS-2964 finding 4: a command resolved the old store, then a
    /// migration moved it before the command took the lock. Opening must
    /// fail asking for a rerun and never recreate the old store.
    #[test]
    fn opening_a_migrated_away_legacy_store_never_recreates_it() {
        use super::super::open_store_resolved;
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let legacy_db = legacy_store(&layout);
        let store = layout.active_store(KEY).unwrap();
        assert_eq!(store.path, legacy_db);
        let guard = layout.legacy_store(KEY, &store.path);
        assert!(guard.is_some());
        // Still in place: opens normally.
        drop(open_store_resolved(&store.path, guard.as_ref(), |p| crate::open_store!(p)).unwrap());

        migrate_key(&layout, KEY).unwrap().unwrap();
        let error = open_store_resolved(&store.path, guard.as_ref(), |p| crate::open_store!(p))
            .err()
            .unwrap();
        assert!(error.contains("rerun the command"), "{error}");
        assert!(!layout.legacy_dir(KEY).exists());
        assert!(!layout.local_root.join(KEY).exists());

        // Old store back but a shared copy present (another two-mirror
        // state): refused under the lock, nothing created or opened.
        drop(open_store_checked(&legacy_db, |p| crate::open_store!(p)).unwrap());
        let error = open_store_resolved(&store.path, guard.as_ref(), |p| crate::open_store!(p))
            .err()
            .unwrap();
        assert!(error.contains("two mirrors"), "{error}");
        // The shared store is never treated as a legacy one.
        assert!(
            layout
                .legacy_store(KEY, &layout.shared_dir(KEY).join("db"))
                .is_none()
        );
    }

    #[test]
    fn outside_git_there_is_nothing_to_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout(dir.path());
        layout.mirror_root = layout.local_root.clone();
        legacy_store(&layout);
        assert_eq!(cmd_migrate(&layout, None).unwrap()["migrated"], json!([]));
        assert!(layout.legacy_dir(KEY).join("db/sentinel").exists());
    }
    #[test]
    fn migrate_all_reports_failures_and_still_moves_later_keys() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout(dir.path());
        let failed = "AaaaSyntheticKey";
        let successful = "ZzzzSyntheticKey";
        for key in [failed, successful] {
            drop(
                open_store_checked(&layout.legacy_dir(key).join("db"), |p| {
                    crate::open_store!(p)
                })
                .unwrap(),
            );
        }
        fs::create_dir_all(layout.shared_dir(failed).join("db")).unwrap();
        let out = cmd_migrate(&layout, None).unwrap();
        assert_eq!(out["failed"][0]["file"], failed);
        assert_eq!(out["failed"].as_array().unwrap().len(), 1);
        assert_eq!(out["migrated"][0]["file"], successful);
        assert!(layout.local_root.join(failed).join("db").exists());
        assert!(!layout.local_root.join(successful).exists());
        assert!(layout.shared_dir(successful).join("db").exists());
    }

    #[test]
    fn migrate_discovers_and_moves_nested_legacy_stores() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout(dir.path());
        let nested_root = layout.project_root.join("packages/app/.figmog");
        let old = nested_root.join(KEY);
        drop(open_store_checked(&old.join("db"), |p| crate::open_store!(p)).unwrap());
        layout.legacy_roots = vec![nested_root];
        let out = cmd_migrate(&layout, None).unwrap();
        assert_eq!(out["migrated"][0]["from"], old.display().to_string());
        assert!(out["failed"].as_array().unwrap().is_empty());
        assert!(!old.exists());
        assert!(layout.shared_dir(KEY).join("db").exists());
    }
    #[test]
    fn interrupted_nested_cleanup_recognizes_a_source_without_its_db() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout(dir.path());
        let nested = layout.project_root.join("packages/app/.figmog");
        layout.legacy_roots = vec![nested.clone()];
        let source = nested.join(KEY);
        drop(open_store_checked(&source.join("db"), |p| crate::open_store!(p)).unwrap());
        let dest = interrupted_after_rename(&layout);
        fs::remove_dir_all(source.join("db")).unwrap();
        let moved = migrate_key(&layout, KEY).unwrap().unwrap();
        assert!(moved.resumed);
        assert_eq!(moved.from, source);
        assert!(!source.exists());
        assert!(!dest.join(MIGRATED_MARKER).exists());
    }
}
