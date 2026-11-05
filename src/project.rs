//! Project config (`figmog.toml` / `figmog.local.toml`) and mirror-root
//! discovery (spec §16).
//!
//! * **Project root:** the nearest directory holding a `figmog.toml`,
//!   walking up from the working directory but never past the Git top
//!   level (so a stray `~/figmog.toml` cannot capture every repository
//!   below it); else the Git top level; else the working directory.
//! * **Mirror root:** inside Git, `<git-common-dir>/figmog`, shared by every
//!   worktree of the repository; outside Git, `<project root>/.figmog`.
//! * **Local root:** `<project root>/.figmog`, per checkout. It holds the
//!   `current` pointer and serve's control socket, and was where every
//!   mirror lived before the shared root existed (the "legacy" location
//!   [`Layout::active_store`] still honors).
//!
//! The config format is a deliberately tiny TOML subset parsed by hand (no
//! `toml` dependency): `#` comments, blank lines, table headers, and
//! `key = "string"` pairs inside `[files]`. Everything outside `[files]`
//! is skipped unparsed so later settings never break this binary — except
//! that multi-line strings there are tracked, so a `[files]` line inside
//! one is never mistaken for the table.

use std::collections::BTreeMap;
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
    // Inside content this parser skips (other tables, top-level keys and
    // their possibly multi-line values), a `[`-led line may be a value
    // continuation rather than a header, so a malformed one is skipped too.
    let mut skipping = false;
    // Inside a skipped multi-line string (and the line it opened on): its
    // lines are content, never headers or keys.
    let mut multiline: Option<(Multiline, usize)> = None;
    for (index, raw) in text.lines().enumerate() {
        let err = |message: String| format!("{name}:{}: {message}", index + 1);
        if let Some((open, line)) = multiline {
            multiline = skip_strings(raw, Some(open)).map(|open| (open, line));
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("[[") {
            // Array of tables: never `[files]`, so always skipped.
            in_files = false;
            skipping = true;
            continue;
        }
        if line.starts_with('[') {
            let header = match parse_header(line) {
                Ok(header) => header,
                Err(_) if skipping => {
                    // A value continuation (e.g. inside an array), which
                    // may open a multi-line string.
                    multiline = skip_strings(raw, None).map(|open| (open, index));
                    continue;
                }
                Err(message) => return Err(err(message)),
            };
            in_files = header == "files";
            skipping = !in_files;
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
            skipping = true;
            multiline = skip_strings(raw, None).map(|open| (open, index));
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
fn skip_strings(line: &str, mut open: Option<Multiline>) -> Option<Multiline> {
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

/// `[name]` (optionally followed by a comment) → `name`.
fn parse_header(line: &str) -> Result<&str, String> {
    let rest = &line[1..];
    let close = rest
        .find(']')
        .ok_or_else(|| "unterminated table header".to_string())?;
    let header = rest[..close].trim();
    let trailing = rest[close + 1..].trim();
    if !(trailing.is_empty() || trailing.starts_with('#')) {
        return Err(format!("unexpected text after [{header}]"));
    }
    if header.is_empty() {
        return Err("empty table header".into());
    }
    Ok(header)
}

/// A bare (`A-Za-z0-9_-`) or quoted key, and the text after it.
fn parse_key(line: &str) -> Result<(String, &str), String> {
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
    if line[end..].trim_start().starts_with('.') {
        return Err("dotted keys are not supported in [files]".into());
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
    /// `<project root>/.figmog`: `current`, `serve.sock`, legacy mirrors.
    pub(crate) local_root: PathBuf,
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
    /// unreadable or malformed config file; a missing or failing `git`
    /// silently means "not in a repository".
    pub(crate) fn discover(cwd: &Path) -> Result<Self, String> {
        let git = git_paths(cwd);
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
        let mirror_root = match git {
            Some(g) => g.common_dir.join("figmog"),
            None => local_root.clone(),
        };
        Ok(Self {
            project_root,
            config,
            mirror_root,
            local_root,
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

    /// `<local root>/<key>` — the old per-checkout store directory.
    pub(crate) fn legacy_dir(&self, key: &str) -> PathBuf {
        self.local_root.join(key)
    }

    /// Whether old per-checkout mirrors can differ from shared ones at all
    /// (outside Git both are `<project root>/.figmog`).
    pub(crate) fn has_legacy_root(&self) -> bool {
        self.mirror_root != self.local_root
    }

    /// The `current` pointer file (fallback default file).
    pub(crate) fn current_file(&self) -> PathBuf {
        self.local_root.join("current")
    }

    /// The project's default file: `figmog.toml`'s `[files] default`, else
    /// `.figmog/current` (migration fallback). Nonempty malformed
    /// `current` contents are an error, never silently skipped.
    pub(crate) fn default_key(&self) -> Result<Option<String>, String> {
        if let Some(key) = self.config.default_key() {
            return Ok(Some(key.to_string()));
        }
        read_current(&self.current_file())
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
        let legacy_db = self.legacy_dir(key).join("db");
        let legacy = exists(&legacy_db);
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
}

impl Layout {
    /// `Some` exactly when `path` is `key`'s old per-checkout store.
    pub(crate) fn legacy_store(&self, key: &str, path: &Path) -> Option<LegacyStore> {
        (self.has_legacy_root() && path == self.legacy_dir(key).join("db")).then(|| LegacyStore {
            layout: self.clone(),
            key: key.to_string(),
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
        if store.path == self.layout.legacy_dir(&self.key).join("db") {
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
}

/// `git rev-parse --path-format=absolute --show-toplevel --git-common-dir`
/// run in `cwd`. `None` when Git is missing or `cwd` isn't in a work tree.
///
/// Git older than 2.31 doesn't know `--path-format` and echoes it back as
/// an output line, then prints `--git-common-dir` relative to `cwd`; that
/// echo is dropped and any relative common dir is resolved against `cwd`
/// here, so older Git still yields the same absolute path.
fn git_paths(cwd: &Path) -> Option<GitPaths> {
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout
        .lines()
        .filter(|line| !line.is_empty() && *line != "--path-format=absolute");
    let toplevel = PathBuf::from(lines.next()?);
    let common_dir = PathBuf::from(lines.next()?);
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        cwd.join(common_dir)
    };
    let canonical = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    Some(GitPaths {
        toplevel: canonical(toplevel),
        common_dir: canonical(common_dir),
    })
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
}
