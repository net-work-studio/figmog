//! Directory traversal is the privacy boundary for mirror payloads.

use std::collections::VecDeque;
use std::fs::{self, DirBuilder, Metadata, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Create missing directories privately, then tighten the selected directory.
/// Reject replaceable ancestors rather than changing their permissions. The
/// returned absolute path must be used for all subsequent filesystem access.
/// Call before opening a store: fold may write journals during open itself.
pub(crate) fn ensure(path: &Path) -> Result<PathBuf, String> {
    let protect = || -> io::Result<PathBuf> {
        let resolved = resolve(path)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&resolved)?;
        directory.set_permissions(Permissions::from_mode(0o700))?;
        Ok(resolved)
    };
    protect().map_err(|e| format!("protecting mirror directory {}: {e}", path.display()))
}

/// Resolve one component at a time, checking its parent before looking up or
/// creating the child. Trusted ownership plus non-writable (or sticky) parents
/// prevents another OS user from substituting any already-checked component.
/// Symlink targets re-enter the same walk; canonicalizing the whole path first
/// would lose unsafe ancestors traversed along the way to the final target.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty mirror path",
        ));
    }
    // SAFETY: geteuid has no preconditions and does not modify process state.
    let uid = unsafe { libc::geteuid() };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Components drops trailing separators and `/.` without following links.
    // Thus `db/` and `db/.` receive the same final-symlink check as `db`.
    let mut pending: VecDeque<_> = absolute
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        if component == "/" {
            resolved = PathBuf::from("/");
            continue;
        }
        let parent = fs::symlink_metadata(&resolved)?;
        check_owner(&resolved, &parent, uid)?;
        if parent.mode() & 0o022 != 0 && parent.mode() & 0o1000 == 0 {
            return Err(unsafe_ancestor(&resolved, "writable by other users"));
        }
        if component == "." {
            continue;
        }
        if component == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(component);
        let metadata = match fs::symlink_metadata(&next) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                DirBuilder::new().mode(0o700).create(&next)?;
                fs::symlink_metadata(&next)?
            }
            Err(error) => return Err(error),
        };
        // Sticky parents only protect children owned by us or root. Other
        // owners could also chmod their directories or rewrite their links.
        check_owner(&next, &metadata, uid)?;
        if metadata.is_symlink() {
            if pending.is_empty() {
                return Err(io::Error::other("final mirror directory is a symlink"));
            }
            links += 1;
            if links > 40 {
                return Err(io::Error::other("too many mirror path symlinks"));
            }
            let target = fs::read_link(&next)?;
            for component in target.components().rev() {
                pending.push_front(component.as_os_str().to_owned());
            }
        } else if metadata.is_dir() {
            resolved = next;
        } else {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                next.display()
            )));
        }
    }
    Ok(resolved)
}

fn check_owner(path: &Path, metadata: &Metadata, uid: libc::uid_t) -> io::Result<()> {
    if metadata.uid() != uid && metadata.uid() != 0 {
        return Err(unsafe_ancestor(
            path,
            "not owned by the current user or root",
        ));
    }
    Ok(())
}

fn unsafe_ancestor(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsafe mirror ancestor {}: {reason}", path.display()),
    )
}
