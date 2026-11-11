//! Project config (`figmog.toml` / `figmog.local.toml`) and mirror-root
//! discovery (spec §16).
//!
//! * **Project root:** the nearest directory holding a `figmog.toml`,
//!   walking up from the working directory but never past the Git top
//!   level (so a stray `~/figmog.toml` cannot capture every repository
//!   below it); else the Git top level; else the working directory.
//! * **Mirror root:** inside Git, `<git-common-dir>/figmog`, shared by every
//!   worktree of the repository; outside Git, `<project root>/.figmog`.
//!   A group-shared Git common directory uses the per-checkout location
//!   instead, preserving the private mirror ancestor checks.
//! * **Local root:** `<project root>/.figmog`, per checkout. It holds the
//!   `current` pointer (and an in-process `--figmog-root` serve's socket),
//!   and was where every mirror lived before the shared root existed (the
//!   "legacy" location [`Layout::active_store`] still honors). Existing
//!   `.figmog` roots from the working directory up to the project root are
//!   also honored, because older binaries wrote relative to the cwd. The shared
//!   background owner's lock, discovery record and log live in the mirror
//!   root ([`crate::owner`]).
//!
//! The config format is a deliberately tiny TOML subset parsed by hand (no
//! `toml` dependency): `#` comments, blank lines, table headers, and
//! `key = "string"` pairs inside `[files]`. Everything outside `[files]`
//! is skipped unparsed so later settings never break this binary — except
//! that multi-line strings there are tracked, so a `[files]` line inside
//! one is never mistaken for the table.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ident::{is_file_key, parse_file_ref};

/// Committed project config file name.
pub(crate) const CONFIG_FILE: &str = "figmog.toml";
/// Machine-specific overrides, kept out of Git.
pub(crate) const LOCAL_CONFIG_FILE: &str = "figmog.local.toml";
/// Per-checkout directory name under the project root.
pub(crate) const LOCAL_DIR: &str = ".figmog";
/// Left in a migrated store directory until `figmog migrate` has removed
/// the old copy; its presence marks an interrupted migration.
pub(crate) const MIGRATED_MARKER: &str = "migrated-from";

/// The parsed `[files]` table: entry name → Figma file key (URLs are
/// reduced to their key when parsed). `default` names the project's
/// default file; other names are reserved for later features.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectConfig {
    pub(crate) files: BTreeMap<String, String>,
}

impl ProjectConfig {
    pub(crate) fn default_key(&self) -> Option<&str> {
        self.files.get("default").map(String::as_str)
    }

    /// Later entries (from `figmog.local.toml`) replace same-named ones.
    fn merge(&mut self, other: ProjectConfig) {
        self.files.extend(other.files);
    }
}

/// Parse one config file's text. `name` prefixes every error as
/// `<name>:<line>: <message>`.
pub(crate) fn parse_config(text: &str, name: &str) -> Result<ProjectConfig, String> {
    let mut config = ProjectConfig::default();
    let mut in_files = false;
    let mut seen_files = false;
    let mut at_root = true;
    let mut skipped_depth = 0usize;
    // Inside a skipped multi-line string (and the line it opened on): its
    // lines are content, never headers or keys.
    let mut multiline: Option<(Multiline, usize)> = None;
    for (index, raw) in text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .enumerate()
    {
        let err = |message: String| format!("{name}:{}: {message}", index + 1);
        if let Some((open, line)) = multiline {
            multiline = skip_strings(raw, Some(open), &mut skipped_depth).map(|open| (open, line));
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if skipped_depth > 0 {
            multiline = skip_strings(raw, None, &mut skipped_depth).map(|open| (open, index));
            continue;
        }
        if line.starts_with('[') {
            at_root = false;
            let (header, array) = parse_header(line).map_err(err)?;
            in_files = header == ["files"] && !array;
            if header.first().is_some_and(|name| name == "files") && !in_files {
                return Err(err(
                    "use a [files] table; arrays and nested files tables are unsupported".into(),
                ));
            }
            if in_files {
                if seen_files {
                    return Err(err("duplicate [files] table".into()));
                }
                seen_files = true;
            }
            continue;
        }
        if !in_files {
            // Top-level keys and other tables belong to later features;
            // skipped unparsed, including multi-line values — but a
            // multi-line string's lines are tracked so a `[files]` inside
            // one is never taken for the table.
            if let Ok((key, _)) = parse_simple_key(line)
                && at_root
                && key == "files"
            {
                return Err(err(
                    "use a [files] table; dotted keys and inline files tables are unsupported"
                        .into(),
                ));
            }
            multiline = skip_strings(raw, None, &mut skipped_depth).map(|open| (open, index));
            continue;
        }
        let (key, rest) = parse_key(line).map_err(err)?;
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            return Err(err(format!("expected `{key} = \"...\"`")));
        };
        let (value, trailing) = parse_string(rest.trim_start()).map_err(err)?;
        let trailing = trailing.trim();
        if !(trailing.is_empty() || trailing.starts_with('#')) {
            return Err(err(format!("unexpected text after the value of {key}")));
        }
        let file_key = parse_file_ref(&value).ok_or_else(|| {
            err(format!(
                "files.{key}: not a Figma file key or URL: {value:?}"
            ))
        })?;
        if config.files.insert(key.clone(), file_key).is_some() {
            return Err(err(format!("duplicate key files.{key}")));
        }
    }
    if let Some((_, line)) = multiline {
        return Err(format!(
            "{name}:{}: unterminated multi-line string",
            line + 1
        ));
    }
    if skipped_depth > 0 {
        return Err(format!(
            "{name}:{}: unterminated array or inline table",
            text.lines().count().max(1)
        ));
    }
    Ok(config)
}

/// The kind of multi-line string a skipped line left open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Multiline {
    /// `"""...""" ` (backslash escapes apply).
    Basic,
    /// `'''...'''`.
    Literal,
}

/// Scan one skipped line for strings, starting inside `open` (if any), and
/// return the multi-line string still open at its end. Single-line strings
/// and a trailing `#` comment are passed over, so their quotes never count.
fn skip_strings(line: &str, mut open: Option<Multiline>, depth: &mut usize) -> Option<Multiline> {
    let bytes = line.as_bytes();
    let mut i = 0;
    // A closing delimiter may be followed by up to two more quotes that
    // belong to the string's content (`""""` ends with one `"` of content).
    let close = |i: usize, quote: u8| {
        let mut end = i + 3;
        while end < bytes.len() && end < i + 5 && bytes[end] == quote {
            end += 1;
        }
        end
    };
    while i < bytes.len() {
        match open {
            Some(Multiline::Basic) => match bytes[i] {
                b'\\' => i += 2,
                b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                    i = close(i, b'"');
                    open = None;
                }
                _ => i += 1,
            },
            Some(Multiline::Literal) => {
                if bytes[i..].starts_with(b"'''") {
                    i = close(i, b'\'');
                    open = None;
                } else {
                    i += 1;
                }
            }
            None => match bytes[i] {
                b'[' | b'{' => {
                    *depth += 1;
                    i += 1;
                }
                b']' | b'}' => {
                    *depth = depth.saturating_sub(1);
                    i += 1;
                }
                b'#' => return None,
                b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                    open = Some(Multiline::Basic);
                    i += 3;
                }
                b'\'' if bytes[i..].starts_with(b"'''") => {
                    open = Some(Multiline::Literal);
                    i += 3;
                }
                b'"' => {
                    // Single-line basic string: to the unescaped closing quote.
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        i += if bytes[i] == b'\\' { 2 } else { 1 };
                    }
                    i += 1;
                }
                b'\'' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                }
                _ => i += 1,
            },
        }
    }
    open
}

/// Parse dotted table names without treating a quoted `]` as the end.
fn parse_header(line: &str) -> Result<(Vec<String>, bool), String> {
    let array = line.starts_with("[[");
    let mut rest = &line[if array { 2 } else { 1 }..];
    let mut names = Vec::new();
    loop {
        rest = rest.trim_start();
        let (name, tail) = parse_simple_key(rest)
            .map_err(|_| "unterminated or malformed table header".to_string())?;
        names.push(name);
        rest = tail.trim_start();
        if let Some(tail) = rest.strip_prefix('.') {
            rest = tail;
            continue;
        }
        let end = if array { "]]" } else { "]" };
        let tail = rest
            .strip_prefix(end)
            .ok_or_else(|| "unterminated table header".to_string())?
            .trim();
        if !(tail.is_empty() || tail.starts_with('#')) {
            return Err("unexpected text after table header".into());
        }
        return Ok((names, array));
    }
}

/// A bare (`A-Za-z0-9_-`) or quoted key, and the text after it.
fn parse_key(line: &str) -> Result<(String, &str), String> {
    let (key, rest) = parse_simple_key(line)?;
    if rest.trim_start().starts_with('.') {
        return Err("dotted keys are not supported in [files]".into());
    }
    Ok((key, rest))
}

fn parse_simple_key(line: &str) -> Result<(String, &str), String> {
    if line.starts_with('"') || line.starts_with('\'') {
        let (key, rest) = parse_string(line)?;
        if key.is_empty() {
            return Err("empty key".into());
        }
        return Ok((key, rest));
    }
    let end = line
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(line.len());
    if end == 0 {
        return Err("expected a key".into());
    }
    Ok((line[..end].to_string(), &line[end..]))
}

/// A basic (`"..."`, with `\" \\ \n \t \r \uXXXX` escapes) or literal
/// (`'...'`) single-line string, and the text after it.
fn parse_string(input: &str) -> Result<(String, &str), String> {
    let mut chars = input.char_indices();
    let quote = match chars.next() {
        Some((_, q @ ('"' | '\''))) => q,
        _ => return Err("expected a quoted string".into()),
    };
    if input.starts_with("\"\"\"") || input.starts_with("'''") {
        return Err("multi-line strings are not supported in [files]".into());
    }
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        if c == quote {
            return Ok((out, &input[i + 1..]));
        }
        if c == '\\' && quote == '"' {
            match chars.next().map(|(_, c)| c) {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                    let code = u32::from_str_radix(&hex, 16)
                        .ok()
                        .filter(|_| hex.len() == 4)
                        .and_then(char::from_u32)
                        .ok_or_else(|| format!("invalid escape \\u{hex}"))?;
                    out.push(code);
                }
                Some(other) => return Err(format!("invalid escape \\{other}")),
                None => break,
            }
        } else {
            out.push(c);
        }
    }
    Err("unterminated string".into())
}

/// Where this invocation's project, config and mirrors live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) project_root: PathBuf,
    pub(crate) config: ProjectConfig,
    /// Where mirrors should live: shared by every worktree inside Git.
    pub(crate) mirror_root: PathBuf,
    /// `<project root>/.figmog`: `current`, legacy mirrors.
    pub(crate) local_root: PathBuf,
    /// Existing legacy roots, nearest working directory first. Empty in
    /// synthetic layouts means just `local_root`.
    pub(crate) legacy_roots: Vec<PathBuf>,
}

/// Where one file's mirror is right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveStore {
    pub(crate) path: PathBuf,
    /// The mirror is still in the old per-checkout location; `figmog
    /// migrate` moves it to the shared root.
    pub(crate) migration_pending: bool,
}

impl Layout {
    /// Discover the layout for `cwd` (absolute). Fails only on an
    /// unreadable or malformed config, or failed Git discovery inside a
    /// repository. Git failures never select a second mirror location.
    pub(crate) fn discover(cwd: &Path) -> Result<Self, String> {
        let git = git_paths(cwd)?;
        let boundary = git.as_ref().map(|g| g.toplevel.as_path());
        let config_dir = find_config_dir(cwd, boundary);
        let project_root = config_dir
            .clone()
            .or_else(|| git.as_ref().map(|g| g.toplevel.clone()))
            .unwrap_or_else(|| cwd.to_path_buf());
        let config = match &config_dir {
            Some(dir) => load_config(dir)?,
            None => ProjectConfig::default(),
        };
        let local_root = project_root.join(LOCAL_DIR);
        let legacy_roots = cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(&project_root))
            .map(|dir| dir.join(LOCAL_DIR))
            .filter(|root| exists(root))
            .collect();
        let mirror_root = match git {
            Some(g) if !g.group_shared => g.common_dir.join("figmog"),
            _ => local_root.clone(),
        };
        Ok(Self {
            project_root,
            config,
            mirror_root,
            local_root,
            legacy_roots,
        })
    }

    /// Discover from the process's working directory.
    pub(crate) fn discover_cwd() -> Result<Self, String> {
        let cwd = std::env::current_dir().map_err(|e| format!("reading working directory: {e}"))?;
        Self::discover(&cwd)
    }

    /// `<mirror root>/<key>` — the shared store directory.
    pub(crate) fn shared_dir(&self, key: &str) -> PathBuf {
        self.mirror_root.join(key)
    }

    /// The existing old per-checkout store directory nearest the cwd,
    /// falling back to `<local root>/<key>` when none exists.
    pub(crate) fn legacy_dir(&self, key: &str) -> PathBuf {
        self.legacy_roots()
            .into_iter()
            .map(|root| root.join(key))
            .find(|dir| {
                exists(&dir.join("db")) || exists(&dir.with_file_name(format!(".{key}.deleting")))
            })
            .unwrap_or_else(|| self.local_root.join(key))
    }

    pub(crate) fn legacy_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.legacy_roots.clone();
        if !roots.contains(&self.local_root) {
            roots.push(self.local_root.clone());
        }
        roots
    }

    pub(crate) fn legacy_stores(&self, key: &str) -> Vec<PathBuf> {
        let mut stores: Vec<_> = self
            .legacy_roots()
            .into_iter()
            .map(|root| root.join(key))
            .filter(|dir| dir != &self.shared_dir(key) && exists(&dir.join("db")))
            .collect();
        stores.sort();
        stores.dedup();
        stores
    }

    /// Whether any old cwd-relative root differs from the selected mirror
    /// root (usually only inside Git, or below an explicit project config).
    pub(crate) fn has_legacy_root(&self) -> bool {
        self.legacy_roots()
            .iter()
            .any(|root| root != &self.mirror_root)
    }

    /// The `current` pointer file (fallback default file).
    #[cfg(test)]
    pub(crate) fn current_file(&self) -> PathBuf {
        self.local_root.join("current")
    }

    /// The project's default file: `figmog.toml`'s `[files] default`, else
    /// the nearest legacy `.figmog/current` (migration fallback). Nonempty malformed
    /// `current` contents are an error, never silently skipped.
    pub(crate) fn default_key(&self) -> Result<Option<String>, String> {
        if let Some(key) = self.config.default_key() {
            return Ok(Some(key.to_string()));
        }
        for root in self.legacy_roots() {
            if let Some(key) = read_current(&root.join("current"))? {
                return Ok(Some(key));
            }
        }
        Ok(None)
    }

    /// Publish the fallback default and retire older nested pointers that
    /// would otherwise mask it. Before the first publication, discovery
    /// still honors those legacy pointers in nearest-directory order.
    pub(crate) fn write_current(&self, key: &str) -> Result<(), String> {
        write_current(&self.local_root, key)?;
        let local = std::fs::canonicalize(&self.local_root)
            .map_err(|error| format!("resolving {}: {error}", self.local_root.display()))?;
        for root in self
            .legacy_roots()
            .into_iter()
            .take_while(|root| root != &self.local_root)
        {
            // A discovered directory alias must not unlink the pointer we
            // just published through its other spelling.
            if std::fs::canonicalize(&root).ok().as_ref() == Some(&local) {
                continue;
            }
            let pointer = root.join("current");
            match std::fs::remove_file(&pointer) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "retiring legacy current pointer {}: {error}",
                        pointer.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Resolve where `key`'s mirror is right now:
    ///
    /// * only the old `.figmog/<key>/db` exists: use it, migration pending
    ///   (never create a second, empty mirror in the shared root);
    /// * only the shared store exists, or neither (a new mirror): shared;
    /// * both exist: an error naming both paths — never pick one silently.
    ///   If the shared copy carries [`MIGRATED_MARKER`], the error points
    ///   at rerunning `figmog migrate` to finish the interrupted move.
    pub(crate) fn active_store(&self, key: &str) -> Result<ActiveStore, String> {
        let shared = self.shared_dir(key);
        let shared_db = shared.join("db");
        if !self.has_legacy_root() {
            return Ok(ActiveStore {
                path: shared_db,
                migration_pending: false,
            });
        }
        let stores = self.legacy_stores(key);
        if stores.len() > 1 {
            return Err(format!(
                "multiple legacy mirrors exist for {key}: {}; move or remove redundant copies before continuing",
                stores
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let legacy_db = self.legacy_dir(key).join("db");
        let legacy = !stores.is_empty();
        match (legacy, exists(&shared_db)) {
            (true, true) if exists(&shared.join(MIGRATED_MARKER)) => Err(format!(
                "an interrupted `figmog migrate` left two mirrors for {key}: {} (old) and {} \
                 (shared); rerun `figmog migrate` to finish",
                legacy_db.display(),
                shared_db.display()
            )),
            (true, true) => Err(format!(
                "two mirrors exist for {key}: {} (old, per-checkout) and {} (shared mirror \
                 root); figmog will not pick one — remove or move one of them",
                legacy_db.display(),
                shared_db.display()
            )),
            (true, false) => Ok(ActiveStore {
                path: legacy_db,
                migration_pending: true,
            }),
            (false, _) => Ok(ActiveStore {
                path: shared_db,
                migration_pending: false,
            }),
        }
    }
}

/// A store resolved to the old per-checkout location (NWS-2964). The path
/// was chosen before the command took the store's lock, and a `figmog
/// migrate` may have moved the store away since: opening one never creates
/// it, and re-checks under its lock that it is still the active store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyStore {
    layout: Layout,
    key: String,
    path: PathBuf,
}

impl Layout {
    /// `Some` when `path` belongs to a discovered old per-checkout root.
    /// Keep that classification after migration removes the database:
    /// callers may have resolved it before reading or fetching input.
    pub(crate) fn legacy_store(&self, key: &str, path: &Path) -> Option<LegacyStore> {
        (path != self.shared_dir(key).join("db")
            && self
                .legacy_roots()
                .into_iter()
                .any(|root| path == root.join(key).join("db")))
        .then(|| LegacyStore {
            layout: self.clone(),
            key: key.to_string(),
            path: path.to_path_buf(),
        })
    }
}

impl LegacyStore {
    /// The store vanished between resolution and opening.
    pub(crate) fn moved_msg(&self) -> String {
        format!(
            "the mirror for {} moved while this command started (did `figmog migrate` run?); \
             nothing was written — rerun the command",
            self.key
        )
    }

    /// Under the store's lock: the old store is still the active one.
    pub(crate) fn still_active(&self) -> Result<(), String> {
        let store = self.layout.active_store(&self.key)?;
        if store.path == self.path {
            Ok(())
        } else {
            Err(self.moved_msg())
        }
    }
}

/// One-line stderr note for a mirror still in the old location.
pub(crate) fn migration_pending_warning(key: &str, store: &ActiveStore) -> String {
    serde_json::json!({
        "warning": format!(
            "migration pending: the mirror for {key} is still at {}; run `figmog migrate` to \
             move it to the shared mirror root",
            store.path.display()
        )
    })
    .to_string()
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Record a fallback default in one explicitly selected local root.
/// Project-aware callers should use [`Layout::write_current`] instead.
pub(crate) fn write_current(local_root: &Path, key: &str) -> Result<(), String> {
    let root = crate::private_dir::ensure(local_root)?;
    std::fs::write(root.join("current"), key).map_err(|error| error.to_string())
}

/// `.figmog/current` contents: trimmed bare key, `None` when absent/empty.
fn read_current(path: &Path) -> Result<Option<String>, String> {
    let Ok(key) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let key = key.trim();
    if key.is_empty() {
        Ok(None)
    } else if !is_file_key(key) {
        Err(
            "invalid mirror key in .figmog/current: expected at least 10 ASCII alphanumeric characters"
                .to_string(),
        )
    } else {
        Ok(Some(key.to_string()))
    }
}

fn find_config_dir(cwd: &Path, boundary: Option<&Path>) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        if std::fs::metadata(dir.join(CONFIG_FILE)).is_ok_and(|m| m.is_file()) {
            return Some(dir.to_path_buf());
        }
        if boundary == Some(dir) {
            break;
        }
    }
    None
}

/// `figmog.toml`, then `figmog.local.toml` (if present) on top of it.
fn load_config(dir: &Path) -> Result<ProjectConfig, String> {
    // Removed since `find_config_dir` saw it: treat as empty, not a panic.
    let mut config = read_config_file(&dir.join(CONFIG_FILE), CONFIG_FILE)?.unwrap_or_default();
    if let Some(local) = read_config_file(&dir.join(LOCAL_CONFIG_FILE), LOCAL_CONFIG_FILE)? {
        config.merge(local);
    }
    Ok(config)
}

fn read_config_file(path: &Path, name: &str) -> Result<Option<ProjectConfig>, String> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(format!("{}: not a regular file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse_config(&text, name).map(Some)
}

struct GitPaths {
    toplevel: PathBuf,
    common_dir: PathBuf,
    group_shared: bool,
}

/// `git rev-parse --path-format=absolute --show-toplevel --git-common-dir`
/// run in `cwd`. `None` only when no ancestor has a `.git` marker.
///
/// Git older than 2.31 doesn't know `--path-format` and echoes it back as
/// an output line, then prints `--git-common-dir` relative to `cwd`; that
/// echo is dropped and any relative common dir is resolved against `cwd`
/// here, so older Git still yields the same absolute path.
fn git_paths(cwd: &Path) -> Result<Option<GitPaths>, String> {
    git_paths_using(cwd, std::ffi::OsStr::new("git"))
}

fn git_paths_using(cwd: &Path, program: &std::ffi::OsStr) -> Result<Option<GitPaths>, String> {
    // A repository marker is authoritative even when Git is missing,
    // rejects ownership, or cannot decode a damaged gitdir file.
    if !cwd.ancestors().any(|dir| exists(&dir.join(".git"))) {
        return Ok(None);
    }
    let output = Command::new(program)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!("discovering Git repository (refusing a local mirror fallback): {e}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "discovering Git repository (refusing a local mirror fallback): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut lines = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty() && *line != b"--path-format=absolute");
    let mut path = || {
        lines
            .next()
            .map(|bytes| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
            .ok_or_else(|| "Git returned incomplete repository paths".to_string())
    };
    let toplevel = path()?;
    let common_dir = path()?;
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        cwd.join(common_dir)
    };
    let canonical = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    let common_dir = canonical(common_dir);
    use std::os::unix::fs::MetadataExt;
    let writable_group = std::fs::metadata(&common_dir)
        .map_err(|e| format!("reading Git common directory {}: {e}", common_dir.display()))?
        .mode()
        & 0o020
        != 0;
    let group_shared = if writable_group {
        let output = Command::new(program)
            .args(["config", "--get", "core.sharedRepository"])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("reading core.sharedRepository: {e}"))?;
        let setting = String::from_utf8_lossy(&output.stdout);
        output.status.success() && !matches!(setting.trim(), "" | "0" | "false" | "umask")
    } else {
        false
    };
    Ok(Some(GitPaths {
        toplevel: canonical(toplevel),
        common_dir,
        group_shared,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "SyntheticKey0001";
    const OTHER: &str = "SyntheticKey0002";

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=figmog",
                "-c",
                "user.email=figmog@example.invalid",
            ])
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap()
    }

    #[test]
    fn parses_files_table_comments_and_unknown_sections() {
        let text = format!(
            "# project\nname = 'ignored'\n\n[settings]\nlist = [\n  \"a\",\n]\n\
             [files] # mirrored\ndefault = \"{KEY}\"  # main\n\
             design-system = 'https://www.figma.com/design/{OTHER}/Name?node-id=1-2'\n\
             [[later]]\nx = 1\n"
        );
        let config = parse_config(&text, CONFIG_FILE).unwrap();
        assert_eq!(config.default_key(), Some(KEY));
        assert_eq!(
            config.files,
            BTreeMap::from([
                ("default".to_string(), KEY.to_string()),
                ("design-system".to_string(), OTHER.to_string()),
            ])
        );
        assert_eq!(
            parse_config("", CONFIG_FILE).unwrap(),
            ProjectConfig::default()
        );
    }

    /// NWS-2964 finding 5: a `[files]` line inside a skipped multi-line
    /// string is content, not the table.
    #[test]
    fn headers_inside_skipped_multiline_strings_are_content() {
        for (open, close) in [
            (r#"""""#, r#"""""#),
            ("'''", "'''"),
            (r#"""""#, r#"\""""""#),
        ] {
            let text = format!(
                "[settings]\nnotes = {open}\n[files]\ndefault = \"{OTHER}\"\n{close}\n\
                 quoted = \"{open}\" # {open}\n\
                 [files]\ndefault = \"{KEY}\"\n"
            );
            let config = parse_config(&text, CONFIG_FILE).unwrap();
            assert_eq!(
                config.files,
                BTreeMap::from([("default".to_string(), KEY.to_string())]),
                "{text}"
            );
        }
        // Opened and closed on one line, and inside a skipped array.
        let text = format!(
            "a = \"\"\"x\"\"\"\nlist = [\n  '''\n[files]\n''',\n]\n[files]\ndefault = '{KEY}'\n"
        );
        assert_eq!(
            parse_config(&text, CONFIG_FILE).unwrap().default_key(),
            Some(KEY)
        );
        // Never closed: an error at the opening line.
        let error = parse_config("[x]\n\nv = '''\n[files]\n", CONFIG_FILE).unwrap_err();
        assert_eq!(error, "figmog.toml:3: unterminated multi-line string");
        // Multi-line values in [files] itself stay a clear error.
        let error =
            parse_config("[files]\ndefault = \"\"\"\nx\n\"\"\"\n", CONFIG_FILE).unwrap_err();
        assert!(
            error.contains("multi-line strings are not supported"),
            "{error}"
        );
    }

    #[test]
    fn malformed_config_errors_name_the_file_and_line() {
        for (text, line, needle) in [
            ("[files\n", 1, "unterminated table header"),
            ("[files]\ndefault = \"abc\n", 2, "unterminated string"),
            (
                "[files]\n\ndefault \"x\"\n",
                3,
                "expected `default = \"...\"`",
            ),
            ("[files]\ndefault = 42\n", 2, "expected a quoted string"),
            (
                "[files]\ndefault = \"not a key\"\n",
                2,
                "not a Figma file key or URL",
            ),
            (
                &format!("[files]\ndefault = \"{KEY}\"\ndefault = \"{KEY}\"\n"),
                3,
                "duplicate key files.default",
            ),
            (
                &format!("[files]\ndefault = \"{KEY}\" extra\n"),
                2,
                "unexpected text",
            ),
            ("[files]\n[x]\n[files]\n", 3, "duplicate [files] table"),
        ] {
            let error = parse_config(text, CONFIG_FILE).unwrap_err();
            assert!(
                error.starts_with(&format!("figmog.toml:{line}: ")) && error.contains(needle),
                "{text:?}: {error}"
            );
        }
    }

    #[test]
    fn local_config_overrides_and_malformed_local_names_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            format!("[files]\ndefault = \"{KEY}\"\nother = \"{KEY}\"\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(LOCAL_CONFIG_FILE),
            format!("[files]\ndefault = \"{OTHER}\"\n"),
        )
        .unwrap();
        let layout = Layout::discover(dir.path()).unwrap();
        assert_eq!(layout.config.default_key(), Some(OTHER));
        assert_eq!(layout.config.files["other"], KEY);
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(OTHER));

        std::fs::write(dir.path().join(LOCAL_CONFIG_FILE), "[files]\nx = 1\n").unwrap();
        let error = Layout::discover(dir.path()).unwrap_err();
        assert!(error.starts_with("figmog.local.toml:2: "), "{error}");
    }

    #[test]
    fn without_git_the_project_dot_figmog_is_the_mirror_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        // No config: the working directory is the project root.
        let layout = Layout::discover(&nested).unwrap();
        assert_eq!(layout.project_root, nested);
        assert_eq!(layout.mirror_root, nested.join(".figmog"));
        assert!(!layout.has_legacy_root());
        // A figmog.toml above makes its directory the project root.
        std::fs::write(root.join(CONFIG_FILE), "").unwrap();
        let layout = Layout::discover(&nested).unwrap();
        assert_eq!(layout.project_root, root);
        assert_eq!(layout.mirror_root, root.join(".figmog"));
        assert_eq!(layout.local_root, layout.mirror_root);
        assert_eq!(
            layout.active_store(KEY).unwrap(),
            ActiveStore {
                path: root.join(".figmog").join(KEY).join("db"),
                migration_pending: false
            }
        );
    }

    #[test]
    fn main_worktree_subdirectory_and_linked_worktree_share_one_mirror_root() {
        let dir = tempfile::tempdir().unwrap();
        let base = canonical(dir.path());
        let main = base.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        git(&main, &["commit", "--quiet", "--allow-empty", "-m", "init"]);
        let sub = main.join("packages/app");
        std::fs::create_dir_all(&sub).unwrap();
        let linked = base.join("linked");
        git(
            &main,
            &["worktree", "add", "--quiet", linked.to_str().unwrap()],
        );

        let expected = main.join(".git/figmog");
        let from_main = Layout::discover(&main).unwrap();
        let from_sub = Layout::discover(&sub).unwrap();
        let from_linked = Layout::discover(&linked).unwrap();
        for layout in [&from_main, &from_sub, &from_linked] {
            assert!(layout.mirror_root.is_absolute());
            assert_eq!(layout.mirror_root, expected);
            assert!(layout.has_legacy_root());
        }
        assert_eq!(from_main.project_root, main);
        assert_eq!(from_sub.project_root, main);
        assert_eq!(from_linked.project_root, linked);
        assert_eq!(from_linked.local_root, linked.join(".figmog"));

        // figmog.toml inside the repo picks the project root, not the
        // mirror root; one above the repo is never consulted.
        std::fs::write(sub.join(CONFIG_FILE), "").unwrap();
        std::fs::write(base.join(CONFIG_FILE), "[files\n").unwrap();
        let layout = Layout::discover(&sub).unwrap();
        assert_eq!(layout.project_root, sub);
        assert_eq!(layout.mirror_root, expected);
        assert_eq!(Layout::discover(&main).unwrap().project_root, main);
    }

    fn layout_with_legacy(dir: &Path) -> Layout {
        Layout {
            project_root: dir.to_path_buf(),
            config: ProjectConfig::default(),
            mirror_root: dir.join(".git/figmog"),
            local_root: dir.join(".figmog"),
            legacy_roots: Vec::new(),
        }
    }

    #[test]
    fn active_store_rules_old_only_new_only_both_and_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_with_legacy(dir.path());
        let legacy_db = layout.legacy_dir(KEY).join("db");
        let shared_db = layout.shared_dir(KEY).join("db");

        // Neither: a new mirror goes to the shared root.
        assert_eq!(
            layout.active_store(KEY).unwrap(),
            ActiveStore {
                path: shared_db.clone(),
                migration_pending: false
            }
        );
        // Old only: used, migration pending; nothing created in the shared root.
        std::fs::create_dir_all(&legacy_db).unwrap();
        assert_eq!(
            layout.active_store(KEY).unwrap(),
            ActiveStore {
                path: legacy_db.clone(),
                migration_pending: true
            }
        );
        assert!(!layout.mirror_root.exists());
        // Both: an error naming both paths.
        std::fs::create_dir_all(&shared_db).unwrap();
        let error = layout.active_store(KEY).unwrap_err();
        assert!(error.contains(&legacy_db.display().to_string()), "{error}");
        assert!(error.contains(&shared_db.display().to_string()), "{error}");
        assert!(error.contains("will not pick one"), "{error}");
        // Both, mid-migration: points at rerunning migrate.
        std::fs::write(layout.shared_dir(KEY).join(MIGRATED_MARKER), "x").unwrap();
        let error = layout.active_store(KEY).unwrap_err();
        assert!(error.contains("rerun `figmog migrate`"), "{error}");
        assert!(error.contains(&legacy_db.display().to_string()), "{error}");
        // New only (old removed): shared, even with the marker left behind.
        std::fs::remove_dir_all(layout.legacy_dir(KEY)).unwrap();
        assert_eq!(
            layout.active_store(KEY).unwrap(),
            ActiveStore {
                path: shared_db,
                migration_pending: false
            }
        );
    }

    #[test]
    fn config_default_wins_over_current_which_remains_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout_with_legacy(dir.path());
        assert_eq!(layout.default_key().unwrap(), None);
        std::fs::create_dir_all(&layout.local_root).unwrap();
        std::fs::write(layout.current_file(), format!("{OTHER}\n")).unwrap();
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(OTHER));
        layout
            .config
            .files
            .insert("default".to_string(), KEY.to_string());
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(KEY));
        layout.config.files.clear();
        std::fs::write(layout.current_file(), "../escape").unwrap();
        assert!(
            layout
                .default_key()
                .unwrap_err()
                .contains(".figmog/current")
        );
    }
    #[test]
    fn publishing_current_retires_every_preceding_legacy_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout_with_legacy(dir.path());
        let deep = dir.path().join("packages/app/.figmog");
        let parent = dir.path().join("packages/.figmog");
        for root in [&deep, &parent] {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(root.join("current"), KEY).unwrap();
        }
        layout.legacy_roots = vec![deep.clone(), parent.clone()];
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(KEY));
        layout.write_current(OTHER).unwrap();
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(OTHER));
        assert!(!deep.join("current").exists());
        assert!(!parent.join("current").exists());
        layout.config.files.insert("default".into(), KEY.into());
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(KEY));
    }

    #[test]
    fn publishing_current_preserves_a_directory_alias_of_the_root_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout_with_legacy(dir.path());
        std::fs::create_dir(&layout.local_root).unwrap();
        let alias = dir.path().join("legacy-alias");
        std::os::unix::fs::symlink(&layout.local_root, &alias).unwrap();
        layout.legacy_roots = vec![alias];
        layout.write_current(OTHER).unwrap();
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(OTHER));
        assert_eq!(
            std::fs::read_to_string(layout.current_file()).unwrap(),
            OTHER
        );
    }

    #[test]
    fn publishing_current_reports_a_legacy_pointer_that_cannot_be_retired() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout_with_legacy(dir.path());
        let legacy = dir.path().join("packages/app/.figmog");
        std::fs::create_dir_all(legacy.join("current")).unwrap();
        layout.legacy_roots = vec![legacy.clone()];
        let error = layout.write_current(OTHER).unwrap_err();
        assert!(error.contains("retiring legacy current pointer"), "{error}");
        assert!(error.contains(&legacy.display().to_string()), "{error}");
    }

    #[test]
    fn config_files_spellings_never_silently_fall_back() {
        for text in [
            "name = 'x'\n[files\n",
            "files.default = 'SyntheticKey0001'\n",
            "files = { default = 'SyntheticKey0001' }\n",
            "'files'.default = 'SyntheticKey0001'\n",
            "[[files]]\ndefault = 'SyntheticKey0001'\n",
            "[files.nested]\ndefault = 'SyntheticKey0001'\n",
        ] {
            assert!(parse_config(text, CONFIG_FILE).is_err(), "{text}");
        }
        let config = parse_config(
            "\u{feff}[\"files\"]\ndefault = 'SyntheticKey0001'\n[tool.\"a]b\"]\nx = 2\n",
            CONFIG_FILE,
        )
        .unwrap();
        assert_eq!(config.default_key(), Some(KEY));
        let config = parse_config(
            "[tool]\nfiles = [\n  [1, 2],\n  [3, 4],\n]\n[files]\ndefault = 'SyntheticKey0001'\n",
            CONFIG_FILE,
        )
        .unwrap();
        assert_eq!(config.default_key(), Some(KEY));
    }

    #[test]
    fn repository_discovery_failures_do_not_select_a_local_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let missing = std::ffi::OsStr::new("/nonexistent/figmog-test-git");
        assert!(git_paths_using(dir.path(), missing).unwrap().is_none());
        git(dir.path(), &["init", "--quiet"]);
        let error = git_paths_using(dir.path(), missing).err().unwrap();
        assert!(
            error.contains("refusing a local mirror fallback"),
            "{error}"
        );
        std::fs::remove_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git"),
            "gitdir: /nonexistent/figmog-gitdir",
        )
        .unwrap();
        let error = Layout::discover(dir.path()).unwrap_err();
        assert!(error.contains("not a git repository"), "{error}");
    }

    #[cfg(target_os = "linux")] // APFS rejects non-UTF-8 directory names.
    #[test]
    fn repository_discovery_preserves_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"repo-\xff".to_vec()));
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        let layout = Layout::discover(&root).unwrap();
        assert_eq!(layout.mirror_root, canonical(&root).join(".git/figmog"));
    }

    #[test]
    fn group_shared_git_uses_private_per_checkout_mirrors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet", "--shared=group"]);
        std::fs::set_permissions(
            dir.path().join(".git"),
            std::fs::Permissions::from_mode(0o2775),
        )
        .unwrap();
        let layout = Layout::discover(dir.path()).unwrap();
        assert_eq!(layout.mirror_root, layout.local_root);
        assert!(!layout.has_legacy_root());
        crate::private_dir::ensure(&layout.shared_dir(KEY).join("db")).unwrap();
    }

    #[test]
    fn nested_legacy_roots_supply_current_and_detect_duplicate_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        git(&root, &["init", "--quiet"]);
        let app = root.join("packages/app");
        let nested = app.join("nested");
        let old = app.join(".figmog");
        std::fs::create_dir_all(old.join(KEY).join("db")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(old.join("current"), KEY).unwrap();
        let layout = Layout::discover(&nested).unwrap();
        assert_eq!(layout.default_key().unwrap().as_deref(), Some(KEY));
        assert_eq!(
            layout.active_store(KEY).unwrap().path,
            old.join(KEY).join("db")
        );
        assert!(layout.active_store(KEY).unwrap().migration_pending);
        std::fs::create_dir_all(layout.local_root.join(KEY).join("db")).unwrap();
        let error = layout.active_store(KEY).unwrap_err();
        assert!(error.contains("multiple legacy mirrors"), "{error}");
        assert!(error.contains(&old.display().to_string()), "{error}");
    }

    #[test]
    fn selected_per_checkout_store_is_never_classified_as_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let mut layout = layout_with_legacy(dir.path());
        // Non-Git and group-shared Git select this mirror root. An older
        // cwd-relative root may still exist below the project directory.
        layout.mirror_root = layout.local_root.clone();
        let nested_root = layout.project_root.join("packages/app/.figmog");
        std::fs::create_dir_all(&nested_root).unwrap();
        layout.legacy_roots = vec![nested_root.clone()];
        assert!(layout.has_legacy_root());

        let selected = layout.shared_dir(KEY).join("db");
        std::fs::create_dir_all(&selected).unwrap();
        assert_eq!(layout.legacy_dir(KEY).join("db"), selected);
        let active = layout.active_store(KEY).unwrap();
        assert_eq!(active.path, selected);
        assert!(!active.migration_pending);
        assert!(layout.legacy_store(KEY, &selected).is_none());

        // An actual legacy store for another key retains its guard.
        let legacy = nested_root.join(OTHER).join("db");
        std::fs::create_dir_all(&legacy).unwrap();
        assert!(layout.legacy_store(OTHER, &legacy).is_some());
        assert!(
            layout
                .legacy_store(OTHER, &layout.shared_dir(OTHER).join("db"))
                .is_none()
        );
    }
}
