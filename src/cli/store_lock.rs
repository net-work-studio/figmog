//! Writer ownership that survives replacement of the database directory.

use std::fs::{File, OpenOptions, TryLockError};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) struct StoreOwner {
    pub(super) path: PathBuf,
    // Never unlink the sidecar: replacing its inode would let a second
    // writer acquire a different lock for the same database path.
    _lock: File,
}

impl StoreOwner {
    pub(super) fn acquire(path: &Path) -> Result<Self, String> {
        let path = crate::private_dir::ensure(path)?;
        let mut name = path
            .file_name()
            .ok_or_else(|| "the filesystem root cannot be a mirror directory".to_string())?
            .to_os_string();
        name.push(".figmog-lock");
        let lock_path = path.with_file_name(name);
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

/// Keep ownership for exactly as long as the store. Field order matters:
/// close the database (including its journals) before releasing the sidecar.
pub(crate) struct OwnedStore<T> {
    store: T,
    owner: StoreOwner,
}

impl<T> OwnedStore<T> {
    pub(super) fn new(store: T, owner: StoreOwner) -> Self {
        Self { store, owner }
    }

    /// The initial open already checked fjall's own lock, including stores
    /// owned by older figmog binaries that don't acquire the sidecar yet.
    /// Keep the sidecar held while closing, deleting, and reopening.
    pub(super) fn reset<U>(
        self,
        open: impl FnOnce(&Path) -> U + std::panic::UnwindSafe,
    ) -> Result<OwnedStore<U>, String> {
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
        let lock_path = dir.path().join("db.figmog-lock");
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
    fn sidecar_rejects_symlinks_hardlinks_and_non_regular_files() {
        for kind in ["symlink", "hardlink", "directory"] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("unrelated");
            std::fs::write(&target, "keep synthetic contents").unwrap();
            let lock = dir.path().join("db.figmog-lock");
            match kind {
                "symlink" => std::os::unix::fs::symlink(&target, &lock).unwrap(),
                "hardlink" => std::fs::hard_link(&target, &lock).unwrap(),
                _ => std::fs::create_dir(&lock).unwrap(),
            }
            let error = StoreOwner::acquire(&dir.path().join("db")).err().unwrap();
            assert!(error.contains("locking"), "{kind}: {error}");
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "keep synthetic contents"
            );
        }
    }
}
