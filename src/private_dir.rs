//! Directory traversal is the privacy boundary for mirror payloads.

use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Create missing directories privately, then tighten the selected directory
/// even if it already exists. Existing ancestors retain their permissions.
/// Call before opening a store: fold may write journals during open itself.
pub(crate) fn ensure(path: &Path) -> Result<(), String> {
    let protect = || -> std::io::Result<()> {
        DirBuilder::new().recursive(true).mode(0o700).create(path)?;
        // Refuse a final symlink and chmod through the descriptor so a
        // replacement cannot redirect chmod to an unrelated target.
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)?;
        directory.set_permissions(Permissions::from_mode(0o700))
    };
    protect().map_err(|e| format!("protecting mirror directory {}: {e}", path.display()))
}
