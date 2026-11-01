//! Keep cwd-relative mirrors out of Git without changing global ignore
//! settings or touching unrelated ancestors outside the repository.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn repository_root(cwd: &Path) -> Option<&Path> {
    // A normal checkout has a .git directory; worktrees and submodules
    // have a .git file. Both mark the boundary for our upward search.
    cwd.ancestors().find(|dir| dir.join(".git").exists())
}

pub(super) fn ignore_local_mirror(cwd: &Path) -> Result<(), String> {
    let Some(root) = repository_root(cwd) else {
        return Ok(());
    };

    // Respect existing patterns, including broader ignores, nested
    // negations, and .git/info/exclude. --no-index also handles mirrors
    // that were already tracked (ignore rules never untrack files).
    let status = Command::new("git")
        .args(["check-ignore", "--no-index", "--quiet", "--", ".figmog/"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => return Ok(()),
        Ok(status) if status.code() == Some(1) => {}
        _ => return Err("could not check Git ignore rules; ensure Git is available".into()),
    }

    let path = closest_ignore(cwd, root);
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let newline = if contents.windows(2).any(|pair| pair == b"\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut addition = String::new();
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        addition.push_str(newline);
    }
    addition.push_str(".figmog/");
    addition.push_str(newline);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(addition.as_bytes()))
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

fn closest_ignore(cwd: &Path, root: &Path) -> PathBuf {
    for dir in cwd.ancestors() {
        let path = dir.join(".gitignore");
        if fs::symlink_metadata(&path).is_ok() || dir == root {
            return path;
        }
    }
    unreachable!("repository root is an ancestor of cwd")
}
