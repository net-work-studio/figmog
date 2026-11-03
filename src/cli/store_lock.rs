//! Writer ownership that survives replacement of the database directory.

use std::fs::{File, OpenOptions, TryLockError};
use std::ops::{Deref, DerefMut};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) struct StoreOwner {
    pub(super) path: PathBuf,
    // Never unlink the external lock: replacing its inode would let a second
    // writer acquire a different lock for the same database path.
    _lock: File,
}

impl StoreOwner {
    pub(super) fn acquire(path: &Path) -> Result<Self, String> {
        let path = crate::private_dir::ensure(path)?;
        if path.parent().is_none() {
            return Err("the filesystem root cannot be a mirror directory".into());
        }
        let lock_path = lock_path(&path)?;
        let acquire = || -> Result<File, String> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&lock_path)
                .map_err(|e| e.to_string())?;
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            // SAFETY: geteuid has no preconditions or side effects.
            let uid = unsafe { libc::geteuid() };
            if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != uid {
                return Err(
                    "lock must be a regular, unshared file owned by the current user".into(),
                );
            }
            file.try_lock().map_err(|e| match e {
                TryLockError::WouldBlock => super::STORE_LOCKED_MSG.to_string(),
                TryLockError::Error(e) => e.to_string(),
            })?;
            Ok(file)
        };
        let lock = acquire().map_err(|e| {
            if e == super::STORE_LOCKED_MSG {
                e
            } else {
                format!("locking {}: {e}", lock_path.display())
            }
        })?;
        Ok(Self { path, _lock: lock })
    }
}

/// A fixed per-user location keeps ownership independent of parent write
/// access, cwd, HOME, and TMPDIR. The directory is private, owned by this
/// user (or root), rejects final symlinks, and sits under a trusted sticky
/// parent; the same traversal checks protect mirrors and this directory.
fn lock_path(verified_path: &Path) -> Result<PathBuf, String> {
    // SAFETY: geteuid has no preconditions or side effects.
    let uid = unsafe { libc::geteuid() };
    let directory =
        crate::private_dir::ensure(&Path::new("/tmp").join(format!("figmog-locks-{uid}")))?;
    Ok(directory.join(lock_name(verified_path)))
}

/// FNV-1a 64 over the verified absolute path's raw Unix bytes. Keep this
/// algorithm stable across binaries: every opener must choose the same
/// file. A hash collision only serializes unrelated stores; the database
/// path itself is never derived from this key. Avoid lossy UTF-8 conversion.
fn lock_name(verified_path: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in verified_path.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}.lock")
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

    /// The initial open already checked fjall's own lock, including stores
    /// owned by older figmog binaries that don't acquire the external lock.
    /// Keep ownership while closing, deleting, and reopening.
    pub(super) fn reset<U>(
        self,
        open: impl FnOnce(&Path) -> U + std::panic::UnwindSafe,
    ) -> Result<OwnedStore<U>, String> {
        // remove_dir_all can empty the writable database before discovering
        // that its parent won't permit unlinking the directory itself.
        // Reject that known failure before dropping or touching the store.
        let parent = self
            .owner
            .path
            .parent()
            .expect("store paths exclude the root");
        let c_parent = std::ffi::CString::new(parent.as_os_str().as_bytes())
            .map_err(|e| format!("checking reset parent {}: {e}", parent.display()))?;
        // SAFETY: c_parent is a valid NUL-terminated path; faccessat only
        // inspects access. AT_EACCESS checks the effective identity, as the
        // later filesystem operations do. Errors during reset still propagate.
        if unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                c_parent.as_ptr(),
                libc::W_OK | libc::X_OK,
                libc::AT_EACCESS,
            )
        } != 0
        {
            return Err(format!(
                "checking reset parent {}: {}",
                parent.display(),
                std::io::Error::last_os_error()
            ));
        }
        let Self { store, owner } = self;
        drop(store);
        std::fs::remove_dir_all(&owner.path)
            .map_err(|e| format!("removing {}: {e}", owner.path.display()))?;
        super::open_store_owned(owner, open)
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

    #[test]
    fn ownership_survives_reset_and_is_released_with_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let st = super::super::open_store_checked(&db, |path| crate::open_store!(path)).unwrap();
        let lock_path = lock_path(&crate::private_dir::ensure(&db).unwrap()).unwrap();
        let inode = std::fs::metadata(&lock_path).unwrap().ino();

        let st = st
            .reset(|path| {
                // The old fjall handle is closed and its directory replaced,
                // but no second opener may slip into this gap, even via aliases.
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
    fn external_lock_rejects_symlinks_hardlinks_and_non_regular_files() {
        for kind in ["symlink", "hardlink", "directory"] {
            // Hard links need the target and external lock on the same
            // filesystem, even when TMPDIR points at a separate mount.
            let dir = tempfile::tempdir_in("/tmp").unwrap();
            let target = dir.path().join("unrelated");
            std::fs::write(&target, "keep synthetic contents").unwrap();
            let db = crate::private_dir::ensure(&dir.path().join("db")).unwrap();
            let lock = lock_path(&db).unwrap();
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
    fn lock_keys_preserve_non_utf8_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        // Some filesystems require valid UTF-8 filenames, so exercise raw
        // key generation without attempting to create these directories.
        let first = Path::new("/synthetic").join(OsString::from_vec(vec![0xff]));
        let second = Path::new("/synthetic").join(OsString::from_vec(vec![0xfe]));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(lock_name(&first), lock_name(&second));
    }
}
