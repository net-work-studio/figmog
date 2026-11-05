//! Writer ownership anchored in the mirror, shared by every filesystem view.

use std::fs::{File, OpenOptions, TryLockError};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) const LOCK_NAME: &str = ".figmog-lock";

pub(super) struct StoreOwner {
    pub(super) path: PathBuf,
    // Fresh resets preserve both this inode and its containing directory.
    // Namespaces and filesystem-equivalent spellings resolve to this same
    // anchor instead of choosing ownership from an opener-local path hash.
    lock: File,
}

/// Why [`lock_file`] could not take its lock.
enum Failure {
    /// Held by another opener, with its owner record if it left one.
    Locked(Option<String>),
    Failed(String),
}

/// Open (creating if needed, never following a symlink) and exclusively
/// lock `lock_path`, a regular, unshared file owned by the current user,
/// then record this process as its owner. Never blocks.
fn lock_file(lock_path: &Path) -> Result<File, Failure> {
    use Failure::{Failed, Locked};
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)
        .map_err(|e| Failed(e.to_string()))?;
    let metadata = file.metadata().map_err(|e| Failed(e.to_string()))?;
    // SAFETY: geteuid has no preconditions or side effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != uid {
        return Err(Failed(
            "lock must be a regular, unshared file owned by the current user".into(),
        ));
    }
    if let Err(e) = file.try_lock() {
        return Err(match e {
            TryLockError::WouldBlock => Locked(owner_record(&file)),
            TryLockError::Error(e) => Failed(e.to_string()),
        });
    }
    // Name this process as the owner (NWS-2963): a serve records
    // itself so another command can say who holds the store; any
    // other opener clears a record a previous owner left behind.
    let record = super::OWNER_RECORD.get().map_or("", String::as_str);
    file.set_len(0)
        .and_then(|()| file.write_all_at(record.as_bytes(), 0))
        .map_err(|e| Failed(e.to_string()))?;
    Ok(file)
}

impl StoreOwner {
    pub(super) fn acquire(path: &Path) -> Result<Self, String> {
        let path = crate::private_dir::ensure(path)?;
        Self::lock(path)
    }

    /// [`Self::acquire`] for a store that must already exist: never creates
    /// the store directory or any ancestor. `Ok(None)` when the directory
    /// is missing — before the lock, or unlinked by the time it was taken
    /// (a `figmog migrate` that moved it away).
    pub(super) fn acquire_existing(path: &Path) -> Result<Option<Self>, String> {
        let path = match crate::private_dir::existing(path)? {
            Some(path) => path,
            None => return Ok(None),
        };
        let owner = match Self::lock(path.clone()) {
            Ok(owner) => owner,
            Err(_) if std::fs::symlink_metadata(&path).is_err() => return Ok(None),
            Err(e) => return Err(e),
        };
        // The anchor must still be linked where it was opened: a remover
        // that held it until after unlinking leaves us an orphaned inode.
        let held = owner.lock.metadata().map_err(|e| e.to_string())?;
        match std::fs::symlink_metadata(owner.path.join(LOCK_NAME)) {
            Ok(linked) if same_file(&held, &linked) => Ok(Some(owner)),
            _ => Ok(None),
        }
    }

    fn lock(path: PathBuf) -> Result<Self, String> {
        if path.parent().is_none() {
            return Err("the filesystem root cannot be a mirror directory".into());
        }
        let lock_path = path.join(LOCK_NAME);
        let lock = lock_file(&lock_path).map_err(|e| match e {
            Failure::Locked(Some(owner)) => super::store_owned_msg(&owner),
            Failure::Locked(None) => super::STORE_LOCKED_MSG.to_string(),
            Failure::Failed(e) => format!("locking {}: {e}", lock_path.display()),
        })?;
        Ok(Self { path, lock })
    }

    pub(super) fn clear_contents(&self) -> Result<(), String> {
        let clear = || -> std::io::Result<()> {
            let held = self.lock.metadata()?;
            let linked = std::fs::symlink_metadata(self.path.join(LOCK_NAME))?;
            if !linked.is_file() || held.nlink() != 1 || !same_file(&held, &linked) {
                return Err(std::io::Error::other("store ownership anchor changed"));
            }
            let mut entries = std::fs::read_dir(&self.path)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let metadata = std::fs::symlink_metadata(entry.path())?;
                // Compare identity, not spelling: the anchor itself can be
                // opened through a case/normalization-equivalent filename.
                if same_file(&metadata, &held) {
                    continue;
                }
                if metadata.is_dir() {
                    std::fs::remove_dir_all(entry.path())?;
                } else {
                    // Includes symlinks; never traverse their targets.
                    std::fs::remove_file(entry.path())?;
                }
            }
            Ok(())
        };
        clear().map_err(|e| format!("resetting {}: {e}", self.path.display()))
    }
}

/// Repository-wide exclusion for one key's `figmog migrate` (NWS-2964): an
/// exclusive lock on a regular file in the shared mirror root, held from
/// before the destination is checked until the final cleanup, so two
/// worktrees can never stage, publish or clean up the same key at once.
pub(super) struct MigrationLock {
    _lock: File,
}

impl MigrationLock {
    /// Take the lock at `path` (a file; created if missing and left in
    /// place afterwards) without waiting; `busy` is the error when another
    /// migration holds it.
    pub(super) fn acquire(path: &Path, busy: impl FnOnce() -> String) -> Result<Self, String> {
        match lock_file(path) {
            Ok(lock) => Ok(Self { _lock: lock }),
            Err(Failure::Locked(_)) => Err(busy()),
            Err(Failure::Failed(e)) => Err(format!("locking {}: {e}", path.display())),
        }
    }
}

/// The owner record a running serve left in a held lock file: one short
/// line of printable text, or `None` (another kind of opener, or anything
/// unexpected — the generic locked message applies then).
fn owner_record(file: &File) -> Option<String> {
    let mut buf = [0u8; 1024];
    let n = file.read_at(&mut buf, 0).ok()?;
    let text = std::str::from_utf8(&buf[..n]).ok()?.trim();
    (text.starts_with("figmog serve") && !text.chars().any(char::is_control))
        .then(|| text.to_string())
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    (left.dev(), left.ino()) == (right.dev(), right.ino())
}

/// Keep ownership for exactly as long as the store. Field order matters:
/// close the database (including its journals) before releasing ownership.
pub(crate) struct OwnedStore<T> {
    store: T,
    owner: StoreOwner,
}

impl<T> OwnedStore<T> {
    pub(super) fn new(store: T, owner: StoreOwner) -> Self {
        Self { store, owner }
    }

    /// Close the store but keep ownership: no other figmog can open it
    /// until the returned owner drops (used by `figmog migrate`, which
    /// copies a closed store's files while holding its lock).
    pub(super) fn into_owner(self) -> StoreOwner {
        let Self { store, owner } = self;
        drop(store);
        owner
    }

    /// Close the store but keep ownership until the returned
    /// [`Compaction`] runs or drops (NWS-2941): `figmog serve`'s workers
    /// close their store this way and compact it only at shutdown.
    pub(crate) fn close_for_compaction(self) -> Compaction {
        Compaction {
            owner: self.into_owner(),
        }
    }

    /// Close the store and compact it ([`crate::compact`]) while still
    /// holding its ownership anchor, then release it. Compaction is a
    /// best-effort optimization over committed data: a failure is reported
    /// on stderr and never fails the command that wrote.
    pub(crate) fn close_and_compact(self) {
        self.close_for_compaction().run();
    }

    /// The initial open already checked fjall's own lock, including stores
    /// owned by older figmog binaries that don't acquire the anchor lock.
    /// Close the store, clear its contents except the ownership anchor,
    /// and reopen it. The directory and anchor stay linked throughout.
    pub(super) fn reset<U>(
        self,
        open: impl FnOnce(&Path) -> U + std::panic::UnwindSafe,
    ) -> Result<OwnedStore<U>, String> {
        let Self { store, owner } = self;
        drop(store);
        owner.clear_contents()?;
        super::open_store_owned(owner, open)
    }
}

/// A closed store whose ownership is kept for a pending compaction
/// ([`OwnedStore::close_for_compaction`]). Dropping it unrun just releases
/// the store.
pub(crate) struct Compaction {
    owner: StoreOwner,
}

impl Compaction {
    /// Compact the closed store, then release its ownership. Any fjall
    /// handle still open on it (a detached reader's view) makes fjall's own
    /// open fail with its lock error: nothing is touched then.
    pub(crate) fn run(self) {
        if let Err(e) = crate::compact::compact(&self.owner.path) {
            eprintln!("figmog: store compaction skipped: {e}");
        }
    }
}

impl<T> Deref for OwnedStore<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.store
    }
}

impl<T> DerefMut for OwnedStore<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    fn writer(path: &Path) -> File {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join("writer.lock"))
            .unwrap();
        file.try_lock().unwrap();
        file
    }

    /// Schedule a competing opener in reset's critical interval: after the
    /// real in-store OS writer lock closes, before any files are removed.
    struct OnClose<F: FnOnce()> {
        writer: Option<File>,
        after_close: Option<F>,
    }

    impl<F: FnOnce()> Drop for OnClose<F> {
        fn drop(&mut self) {
            drop(self.writer.take());
            self.after_close.take().unwrap()();
        }
    }

    #[test]
    fn equivalent_case_and_unicode_spellings_cannot_open_during_reset() {
        let mut checked = 0;
        for (name, alias_name) in [("Project", "project"), ("Caf\u{e9}", "Cafe\u{301}")] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join(name).join("db");
            let owner = StoreOwner::acquire(&db).unwrap();
            let alias = dir.path().join(alias_name).join("db");
            let metadata = std::fs::metadata(&db).unwrap();
            let Ok(alias_metadata) = std::fs::metadata(&alias) else {
                continue;
            };
            if (metadata.dev(), metadata.ino()) != (alias_metadata.dev(), alias_metadata.ino()) {
                continue;
            }
            checked += 1;
            let store = OnClose {
                writer: Some(writer(&db)),
                after_close: Some(|| {
                    let competing = super::super::open_store_checked(&alias, writer);
                    assert_eq!(
                        competing.err().as_deref(),
                        Some(super::super::STORE_LOCKED_MSG),
                        "filesystem-equivalent spelling acquired both ownership and writer locks"
                    );
                }),
            };
            let _replacement = OwnedStore::new(store, owner).reset(writer).unwrap();
        }
        if checked == 0 {
            eprintln!("case/Unicode spellings are distinct on this test volume");
        }
    }

    #[test]
    #[ignore = "requires root for isolated chroot filesystem views"]
    fn reset_coordinates_across_chroot_views() {
        // SAFETY: geteuid has no preconditions or side effects.
        assert_eq!(unsafe { libc::geteuid() }, 0, "run this regression as root");
        if let Some(root) = std::env::var_os("FIGMOG_TEST_CHROOT") {
            let root = std::ffi::CString::new(root.as_bytes()).unwrap();
            // SAFETY: root is a valid NUL-terminated fixture directory. This
            // runs only in a dedicated child process, never the parent runner.
            assert_eq!(unsafe { libc::chroot(root.as_ptr()) }, 0);
            std::env::set_current_dir("/").unwrap();
            let db = Path::new("/db");
            let metadata = std::fs::metadata(db).unwrap();
            assert_eq!(
                format!("{}:{}", metadata.dev(), metadata.ino()),
                std::env::var("FIGMOG_TEST_DB_ID").unwrap()
            );
            assert_eq!(
                std::fs::read_to_string(db.join("sentinel")).unwrap(),
                "shared mirror"
            );
            let competing = super::super::open_store_checked(db, writer);
            assert_eq!(
                competing.err().as_deref(),
                Some(super::super::STORE_LOCKED_MSG),
                "isolated /tmp view acquired both ownership and writer locks"
            );
            return;
        }

        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir(root.path().join("tmp")).unwrap();
        let db = root.path().join("db");
        let owner = StoreOwner::acquire(&db).unwrap();
        std::fs::write(db.join("sentinel"), "shared mirror").unwrap();
        let metadata = std::fs::metadata(&db).unwrap();
        let store = OnClose {
            writer: Some(writer(&db)),
            after_close: Some(|| {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "cli::store_lock::tests::reset_coordinates_across_chroot_views",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("FIGMOG_TEST_CHROOT", root.path())
                    .env(
                        "FIGMOG_TEST_DB_ID",
                        format!("{}:{}", metadata.dev(), metadata.ino()),
                    )
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
            }),
        };
        let _replacement = OwnedStore::new(store, owner).reset(writer).unwrap();
        assert!(!db.join("sentinel").exists());
    }

    #[test]
    fn ownership_survives_reset_and_is_released_with_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let st = super::super::open_store_checked(&db, |path| crate::open_store!(path)).unwrap();
        let lock_path = db.join(LOCK_NAME);
        let inode = std::fs::metadata(&lock_path).unwrap().ino();
        let directory_inode = std::fs::metadata(&db).unwrap().ino();

        let st = st
            .reset(|path| {
                // The old fjall handle is closed and its contents removed,
                // but no second opener may slip into this gap, even via aliases.
                assert_eq!(std::fs::metadata(path).unwrap().ino(), directory_inode);
                assert_eq!(std::fs::read_dir(path).unwrap().count(), 1);
                let error = StoreOwner::acquire(&path.join(".")).err().unwrap();
                assert_eq!(error, super::super::STORE_LOCKED_MSG);
                crate::open_store!(path)
            })
            .unwrap();
        assert_eq!(std::fs::metadata(&lock_path).unwrap().ino(), inode);
        assert_eq!(
            StoreOwner::acquire(&db).err().unwrap(),
            super::super::STORE_LOCKED_MSG
        );
        drop(st);
        assert!(StoreOwner::acquire(&db).is_ok());
        assert_eq!(std::fs::metadata(&lock_path).unwrap().ino(), inode);
    }

    #[test]
    fn anchor_rejects_symlinks_hardlinks_and_non_regular_files() {
        for kind in ["symlink", "hardlink", "directory"] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("unrelated");
            std::fs::write(&target, "keep synthetic contents").unwrap();
            let db = crate::private_dir::ensure(&dir.path().join("db")).unwrap();
            let lock = db.join(LOCK_NAME);
            match kind {
                "symlink" => std::os::unix::fs::symlink(&target, &lock).unwrap(),
                "hardlink" => std::fs::hard_link(&target, &lock).unwrap(),
                _ => std::fs::create_dir(&lock).unwrap(),
            }
            let error = StoreOwner::acquire(&db).err().unwrap();
            assert!(error.contains("locking"), "{kind}: {error}");
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "keep synthetic contents"
            );
            // These are deliberately invalid, unique test fixtures, never
            // live lock files. Remove them before dropping their targets.
            if kind == "directory" {
                std::fs::remove_dir(&lock).unwrap();
            } else {
                std::fs::remove_file(&lock).unwrap();
            }
        }
    }

    #[test]
    fn ownership_resolves_aliases_without_serializing_other_stores() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("alias")).unwrap();
        let first = dir.path().join("real/db");
        let second = dir.path().join("real/other-db");
        let _first = StoreOwner::acquire(&first).unwrap();
        let _second = StoreOwner::acquire(&second).unwrap();
        let alias = dir.path().join("alias/db/.");
        assert_eq!(
            StoreOwner::acquire(&alias).err().unwrap(),
            super::super::STORE_LOCKED_MSG
        );
    }

    #[test]
    fn contents_reset_unlinks_symlinks_without_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let owner = StoreOwner::acquire(&db).unwrap();
        let target = dir.path().join("outside");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("sentinel"), "keep synthetic data").unwrap();
        std::os::unix::fs::symlink(&target, db.join("symlink")).unwrap();
        let _replacement = OwnedStore::new(writer(&db), owner).reset(writer).unwrap();
        assert!(!db.join("symlink").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("sentinel")).unwrap(),
            "keep synthetic data"
        );
    }
}
