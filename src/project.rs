//! Project config (`figmog.toml` / `figmog.local.toml`), the typed
//! project settings later features read ([`Settings`], NWS-2946) and
//! mirror-root discovery (spec §16).
//!
//! * **Project root:** the nearest directory holding a `figmog.toml`,
//!   walking up from the working directory but never past the Git top
//!   level (so a stray `~/figmog.toml` cannot capture every repository
//!   below it); else the Git top level; else the working directory.
//! * **Mirror root:** inside Git, `<git-common-dir>/figmog`, shared by every
//!   worktree of the repository; outside Git, `<project root>/.figmog`.
//! * **Local root:** `<project root>/.figmog`, per checkout. It holds the
//!   `current` pointer (and an in-process `--figmog-root` serve's socket),
//!   and was where every mirror lived before the shared root existed (the
//!   "legacy" location [`Layout::active_store`] still honors). The shared
//!   background owner's lock, discovery record and log live in the mirror
//!   root ([`crate::owner`]).
//!
//! The config format is a deliberately tiny TOML subset parsed by hand (no
//! `toml` dependency): `#` comments, blank lines, table headers,
//! `key = "string"` pairs inside `[files]`, `key = <string | boolean |
//! number>` pairs inside the settings tables `[stack]`, `[inspector]`,
//! `[verify]`, `[tokens]` and `[assets]`, and `"<collection>/<mode>" =
//! "<selector>"` pairs inside `[tokens.modes]` (spec §16; NWS-2948,
//! NWS-2952). Every other table and top-level key is skipped
//! unparsed so later settings never break this binary — except that
//! multi-line strings there are tracked, so a header line inside one is
//! never mistaken for a table. Unknown keys inside a settings table are
//! kept (as [`ProjectConfig::unknown_keys`], which `figmog doctor`
//! reports) rather than rejected, for the same reason; a known key with a
//! wrong type or an invalid value is a `<file>:<line>:` error, like any
//! malformed `[files]` line.

use std::collections::{BTreeMap, BTreeSet};
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

/// One parsed config file (or `figmog.toml` with `figmog.local.toml`
/// merged on top): the `[files]` table — entry name → Figma file key (URLs
/// are reduced to their key when parsed) — plus the settings tables.
/// `default` names the project's default file, its *product* file;
/// `library` names the optional library file holding tokens and
/// components (NWS-2946); other names are reserved for later features.
/// Read settings through [`ProjectConfig::settings`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectConfig {
    pub(crate) files: BTreeMap<String, String>,
    /// Boxed: [`Layout`] is carried by value in enums.
    settings: Box<RawSettings>,
    /// The known tables (`files`, `stack`, `inspector`, `verify`, `tokens`,
    /// `tokens.modes`) present.
    tables: BTreeSet<&'static str>,
}

impl ProjectConfig {
    pub(crate) fn default_key(&self) -> Option<&str> {
        self.files.get("default").map(String::as_str)
    }

    /// The optional library file (`[files] library`).
    pub(crate) fn library_key(&self) -> Option<&str> {
        self.files.get("library").map(String::as_str)
    }

    /// The typed settings, every key not set filled with its default.
    pub(crate) fn settings(&self) -> Settings {
        self.settings.resolve()
    }

    /// `<table>.<key>` for each key a settings table set that this binary
    /// does not know, sorted. Kept, not rejected (a newer figmog may have
    /// written it); `figmog doctor` reports them.
    pub(crate) fn unknown_keys(&self) -> &BTreeSet<String> {
        &self.settings.unknown
    }

    /// Whether the file(s) this config came from had a `[name]` table, for
    /// one of the known tables (`files`, `stack`, `inspector`, `verify`,
    /// `tokens`, `tokens.modes`).
    pub(crate) fn has_table(&self, name: &str) -> bool {
        self.tables.contains(name)
    }

    /// Later entries (from `figmog.local.toml`) replace same-named ones.
    fn merge(&mut self, other: ProjectConfig) {
        self.files.extend(other.files);
        self.settings.merge(*other.settings);
        self.tables.extend(other.tables);
    }
}

/// The table a config line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Files,
    Settings(&'static str),
    /// `[tokens.modes]`: free-form `"<collection>/<mode>" = "<selector>"`.
    Modes,
    /// Any other table, or top-level keys: skipped unparsed.
    Skipped,
}

/// Parse one config file's text. `name` prefixes every error as
/// `<name>:<line>: <message>`.
pub(crate) fn parse_config(text: &str, name: &str) -> Result<ProjectConfig, String> {
    let mut config = ProjectConfig::default();
    let mut table = Table::Skipped;
    // `<table>.<key>` for every settings key set in this file.
    let mut seen_keys = BTreeSet::new();
    // Inside content this parser skips (other tables, top-level keys and
    // their possibly multi-line values), a `[`-led line may be a value
    // continuation rather than a header, so a malformed one is skipped too.
    let mut skipping = false;
    // Inside a skipped multi-line string (and the line it opened on): its
    // lines are content, never headers or keys.
    let mut multiline: Option<(Multiline, usize)> = None;
    // Inside an unknown settings key's value that spans lines (an array,
    // inline table or multi-line string), and the line it started on.
    let mut unknown_value: Option<(ValueScan, usize)> = None;
    for (index, raw) in text.lines().enumerate() {
        let err = |message: String| format!("{name}:{}: {message}", index + 1);
        if let Some((open, line)) = multiline {
            multiline = skip_strings(raw, Some(open)).map(|open| (open, line));
            continue;
        }
        if let Some((scan, line)) = unknown_value {
            unknown_value = scan.scan(raw).open().map(|scan| (scan, line));
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("[[") {
            // Array of tables: never a known table, so always skipped.
            table = Table::Skipped;
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
            table = match header {
                "files" => Table::Files,
                TOKEN_MODES_TABLE => Table::Modes,
                TOKENS_TABLE => Table::Settings(TOKENS_TABLE),
                ASSETS_TABLE => Table::Settings(ASSETS_TABLE),
                other => match SETTINGS_TABLES.iter().find(|t| **t == other) {
                    Some(known) => Table::Settings(known),
                    None => Table::Skipped,
                },
            };
            skipping = table == Table::Skipped;
            let known = match table {
                Table::Files => Some("files"),
                Table::Settings(known) => Some(known),
                Table::Modes => Some(TOKEN_MODES_TABLE),
                Table::Skipped => None,
            };
            if let Some(known) = known
                && !config.tables.insert(known)
            {
                return Err(err(format!("duplicate [{known}] table")));
            }
            continue;
        }
        let table_name = match table {
            Table::Skipped => {
                // Top-level keys and other tables belong to later features;
                // skipped unparsed, including multi-line values — but a
                // multi-line string's lines are tracked so a header inside
                // one is never taken for a table.
                skipping = true;
                multiline = skip_strings(raw, None).map(|open| (open, index));
                continue;
            }
            Table::Files => "files",
            Table::Settings(known) => known,
            Table::Modes => TOKEN_MODES_TABLE,
        };
        let (key, rest) = parse_key(line, table_name).map_err(err)?;
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            let shape = if matches!(table, Table::Files | Table::Modes) {
                "\"...\""
            } else {
                "<value>"
            };
            return Err(err(format!("expected `{key} = {shape}`")));
        };
        let rest = rest.trim_start();
        if table == Table::Modes {
            let (selector, trailing) = parse_string(rest, table_name).map_err(err)?;
            let trailing = trailing.trim();
            if !(trailing.is_empty() || trailing.starts_with('#')) {
                return Err(err(format!("unexpected text after the value of {key:?}")));
            }
            crate::tokens::check_mode_key(&key)
                .and_then(|()| crate::tokens::check_mode_selector(&selector))
                .map_err(|e| err(format!("{TOKEN_MODES_TABLE}: {e}")))?;
            if config
                .settings
                .mode_selectors
                .insert(key.clone(), selector)
                .is_some()
            {
                return Err(err(format!("duplicate key {TOKEN_MODES_TABLE}.{key:?}")));
            }
            continue;
        }
        let (value, trailing) = if table == Table::Files {
            let (value, trailing) = parse_string(rest, table_name).map_err(err)?;
            (Scalar::Str(value), trailing)
        } else {
            let Table::Settings(known) = table else {
                unreachable!("only [files] and settings tables get here")
            };
            if !RawSettings::is_known(known, &key) {
                // A key this binary doesn't know (a newer figmog's): kept
                // by name, its value skipped unparsed whatever its shape —
                // across lines for an open array, inline table or
                // multi-line string.
                if !seen_keys.insert(format!("{known}.{key}")) {
                    return Err(err(format!("duplicate key {known}.{key}")));
                }
                if rest.is_empty() || rest.starts_with('#') {
                    return Err(err(format!("expected a value for {known}.{key}")));
                }
                config.settings.unknown.insert(format!("{known}.{key}"));
                unknown_value = ValueScan::default()
                    .scan(rest)
                    .open()
                    .map(|scan| (scan, index));
                continue;
            }
            parse_scalar(rest, table_name).map_err(err)?
        };
        let trailing = trailing.trim();
        if !(trailing.is_empty() || trailing.starts_with('#')) {
            return Err(err(format!("unexpected text after the value of {key}")));
        }
        match (table, value) {
            (Table::Files, Scalar::Str(value)) => {
                let file_key = parse_file_ref(&value).ok_or_else(|| {
                    err(format!(
                        "files.{key}: not a Figma file key or URL: {value:?}"
                    ))
                })?;
                if config.files.insert(key.clone(), file_key).is_some() {
                    return Err(err(format!("duplicate key files.{key}")));
                }
            }
            (Table::Settings(known), value) => {
                if !seen_keys.insert(format!("{known}.{key}")) {
                    return Err(err(format!("duplicate key {known}.{key}")));
                }
                config.settings.set(known, &key, value).map_err(err)?;
            }
            _ => unreachable!("[files] values are always strings; skipped lines never get here"),
        }
    }
    if let Some((_, line)) = multiline {
        return Err(format!(
            "{name}:{}: unterminated multi-line string",
            line + 1
        ));
    }
    if let Some((_, line)) = unknown_value {
        return Err(format!("{name}:{}: unterminated value", line + 1));
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

/// Where a skipped value stands at the end of a line: inside a
/// multi-line string, and how many `[` / `{` are open outside strings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ValueScan {
    string: Option<Multiline>,
    depth: usize,
}

impl ValueScan {
    /// Scan one more line of the value, like [`skip_strings`] but also
    /// counting brackets outside strings. Single-line strings and a `#`
    /// comment outside strings are passed over.
    fn scan(mut self, line: &str) -> Self {
        let bytes = line.as_bytes();
        let mut i = 0;
        let close = |i: usize, quote: u8| {
            let mut end = i + 3;
            while end < bytes.len() && end < i + 5 && bytes[end] == quote {
                end += 1;
            }
            end
        };
        while i < bytes.len() {
            match self.string {
                Some(Multiline::Basic) => match bytes[i] {
                    b'\\' => i += 2,
                    b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                        i = close(i, b'"');
                        self.string = None;
                    }
                    _ => i += 1,
                },
                Some(Multiline::Literal) => {
                    if bytes[i..].starts_with(b"'''") {
                        i = close(i, b'\'');
                        self.string = None;
                    } else {
                        i += 1;
                    }
                }
                None => match bytes[i] {
                    b'#' => return self,
                    b'[' | b'{' => {
                        self.depth += 1;
                        i += 1;
                    }
                    b']' | b'}' => {
                        self.depth = self.depth.saturating_sub(1);
                        i += 1;
                    }
                    b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                        self.string = Some(Multiline::Basic);
                        i += 3;
                    }
                    b'\'' if bytes[i..].starts_with(b"'''") => {
                        self.string = Some(Multiline::Literal);
                        i += 3;
                    }
                    b'"' => {
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
        self
    }

    /// `Some(self)` while the value continues on the next line.
    fn open(self) -> Option<Self> {
        (self.string.is_some() || self.depth > 0).then_some(self)
    }
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
fn parse_key<'a>(line: &'a str, table: &str) -> Result<(String, &'a str), String> {
    if line.starts_with('"') || line.starts_with('\'') {
        let (key, rest) = parse_string(line, table)?;
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
        return Err(format!("dotted keys are not supported in [{table}]"));
    }
    Ok((line[..end].to_string(), &line[end..]))
}

/// A basic (`"..."`, with `\" \\ \n \t \r \uXXXX` escapes) or literal
/// (`'...'`) single-line string, and the text after it.
fn parse_string<'a>(input: &'a str, table: &str) -> Result<(String, &'a str), String> {
    let mut chars = input.char_indices();
    let quote = match chars.next() {
        Some((_, q @ ('"' | '\''))) => q,
        _ => return Err("expected a quoted string".into()),
    };
    if input.starts_with("\"\"\"") || input.starts_with("'''") {
        return Err(format!("multi-line strings are not supported in [{table}]"));
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

// ---- settings (NWS-2946) ----

/// The settings tables, in the order `figmog init` writes them.
pub(crate) const SETTINGS_TABLES: [&str; 3] = ["stack", "inspector", "verify"];
/// `[tokens]` (NWS-2948): how `figmog tokens` renders. Optional — `figmog
/// init` does not write it; every key has a default.
pub(crate) const TOKENS_TABLE: &str = "tokens";
/// `[tokens.modes]`: `"<collection>/<mode>" = "<selector>"` — `":root"`
/// for a collection's root mode, `""` to leave a mode out.
pub(crate) const TOKEN_MODES_TABLE: &str = "tokens.modes";

/// `[assets]` (NWS-2952): where and how `figmog assets export` writes.
/// Optional — `figmog init` does not write it; every key has a default.
pub(crate) const ASSETS_TABLE: &str = "assets";
/// `[assets] icons` default: the icon components' folder.
pub(crate) const DEFAULT_ICONS_DIR: &str = "src/icons";
/// `[assets] images` default: the image fills' folder.
pub(crate) const DEFAULT_IMAGES_DIR: &str = "public/figma";
/// `[assets] manifest` default: the generated asset index.
pub(crate) const DEFAULT_ASSET_MANIFEST: &str = "figmog.assets.json";
/// `[assets] prefix` default: icon component names start with it.
pub(crate) const DEFAULT_ICON_PREFIX: &str = "Icon";

/// `[stack] tokens` default: where `figmog tokens export` output lives.
pub(crate) const DEFAULT_TOKENS_PATH: &str = "src/figmog-tokens.css";
/// `[inspector] shortcut` default.
pub(crate) const DEFAULT_SHORTCUT: &str = "Alt+Shift+F";
/// `[verify] dev_server` default (Vite's default dev server).
pub(crate) const DEFAULT_DEV_SERVER: &str = "http://localhost:5173";
/// `[verify] threshold` default: the advisory match score (0–100, the
/// screenshot comparison's `score`) below which a verify run is flagged.
pub(crate) const DEFAULT_THRESHOLD: f64 = 95.0;
/// The v1 verify contract (not configurable): viewport widths in CSS px.
pub(crate) const VERIFY_VIEWPORTS: [u32; 2] = [1440, 390];
/// The v1 verify contract: color scheme.
pub(crate) const VERIFY_COLOR_SCHEME: &str = "light";
/// The v1 verify contract: component state.
pub(crate) const VERIFY_STATE: &str = "default";

/// `[stack] framework`. v1 supports React only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Framework {
    #[default]
    React,
}

impl Framework {
    const ALL: [Framework; 1] = [Framework::React];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Framework::React => "react",
        }
    }

    /// The config spelling (`"react"`) to its value.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|known| known.as_str() == value)
            .ok_or_else(|| format!("unsupported framework {value:?} (supported: react)"))
    }
}

/// `[stack] styling`. v1 supports Tailwind only (CSS modules and vanilla
/// CSS are planned).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Styling {
    #[default]
    Tailwind,
}

impl Styling {
    const ALL: [Styling; 1] = [Styling::Tailwind];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Styling::Tailwind => "tailwind",
        }
    }

    /// The config spelling (`"tailwind"`) to its value.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|known| known.as_str() == value)
            .ok_or_else(|| format!("unsupported styling {value:?} (supported: tailwind)"))
    }
}

/// The project's settings with every default filled in — what later
/// features (token export, spec, assets, the inspector and the verify
/// loop) read, through [`ProjectConfig::settings`] /
/// [`Layout::settings`], or `figmog settings` for tools outside this
/// binary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Settings {
    pub(crate) stack: StackSettings,
    pub(crate) inspector: InspectorSettings,
    pub(crate) verify: VerifySettings,
    pub(crate) tokens: TokenSettings,
    pub(crate) assets: AssetSettings,
}

/// `[assets] components`: the generated icon components' language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ComponentLang {
    /// TypeScript (`.tsx`, typed `SVGProps<SVGSVGElement>` props).
    #[default]
    Tsx,
    /// JavaScript (`.jsx`).
    Jsx,
}

impl ComponentLang {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ComponentLang::Tsx => "tsx",
            ComponentLang::Jsx => "jsx",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "tsx" => Ok(ComponentLang::Tsx),
            "jsx" => Ok(ComponentLang::Jsx),
            _ => Err(format!(
                "unsupported language {value:?} (supported: tsx, jsx)"
            )),
        }
    }
}

/// `[assets]`: where `figmog assets export` writes icons, images and the
/// manifest, and how icons are named (NWS-2952). Every path is relative
/// to the project root and stays inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssetSettings {
    /// Icon components (and `svg/` beneath it).
    pub(crate) icons: String,
    /// Image fills, named by content.
    pub(crate) images: String,
    /// The generated asset manifest `figmog spec` reads.
    pub(crate) manifest: String,
    /// Every icon component's name starts with it (`Icon` →
    /// `IconArrowRight`).
    pub(crate) prefix: String,
    /// Convert single-color icons to `currentColor` (opt-in).
    pub(crate) current_color: bool,
    /// `.tsx` or `.jsx` components.
    pub(crate) components: ComponentLang,
}

/// `[tokens]` and `[tokens.modes]`: how `figmog tokens` renders the
/// variable snapshot (the file itself is `[stack] tokens`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenSettings {
    /// `tailwind` (CSS variables plus a Tailwind v4 `@theme`) or `css`.
    pub(crate) format: crate::tokens::Format,
    /// `"<collection>/<mode>"` → selector; modes not named get the default
    /// mapping (light-like/default → `:root`, the dark-like mode →
    /// `[data-theme="dark"]`, others → `[data-<collection>="<mode>"]`).
    pub(crate) modes: BTreeMap<String, String>,
}

impl TokenSettings {
    /// The renderer options these settings select.
    pub(crate) fn export_options(&self) -> crate::tokens::ExportOptions {
        crate::tokens::ExportOptions {
            format: self.format,
            modes: self.modes.clone(),
        }
    }
}

/// `[stack]`: what code figmog's output should be shaped for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StackSettings {
    pub(crate) framework: Framework,
    pub(crate) styling: Styling,
    /// The token file, relative to the project root (`/`-separated, never
    /// absolute, never leaving the project).
    pub(crate) tokens: String,
}

impl StackSettings {
    /// [`StackSettings::tokens`] under `project_root`.
    pub(crate) fn tokens_path(&self, project_root: &Path) -> PathBuf {
        project_root.join(&self.tokens)
    }
}

/// `[inspector]`: the in-page inspector of the dev-server plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InspectorSettings {
    /// On in development builds (never in production builds).
    pub(crate) enabled: bool,
    /// `Modifier+…+Key`, e.g. `Alt+Shift+F`.
    pub(crate) shortcut: String,
}

/// `[verify]`: the visual verify loop. The viewport, color scheme and
/// state contract is fixed for v1 ([`VERIFY_VIEWPORTS`] …).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VerifySettings {
    /// The loop on or off (`loop` in the file).
    pub(crate) enabled: bool,
    /// The local dev server's URL (loopback hosts only).
    pub(crate) dev_server: String,
    /// Advisory match score, 0–100.
    pub(crate) threshold: f64,
}

impl Default for Settings {
    fn default() -> Self {
        RawSettings::default().resolve()
    }
}

impl Settings {
    /// The settings as `figmog settings` prints them (fixed key order).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "stack": {
                "framework": self.stack.framework.as_str(),
                "styling": self.stack.styling.as_str(),
                "tokens": self.stack.tokens,
            },
            "inspector": {
                "enabled": self.inspector.enabled,
                "shortcut": self.inspector.shortcut,
            },
            "verify": {
                "loop": self.verify.enabled,
                "dev_server": self.verify.dev_server,
                "threshold": self.verify.threshold,
                "viewports": VERIFY_VIEWPORTS,
                "color_scheme": VERIFY_COLOR_SCHEME,
                "state": VERIFY_STATE,
            },
            "tokens": {
                "format": self.tokens.format.as_str(),
                "modes": self.tokens.modes,
            },
            "assets": {
                "icons": self.assets.icons,
                "images": self.assets.images,
                "manifest": self.assets.manifest,
                "prefix": self.assets.prefix,
                "current_color": self.assets.current_color,
                "components": self.assets.components.as_str(),
            },
        })
    }

    /// The `[name]` settings table (one of [`SETTINGS_TABLES`]) as `figmog
    /// init` writes it: every key, set to this value, with a comment.
    /// Parses back to the same values.
    pub(crate) fn render_table(&self, name: &str) -> String {
        match name {
            "stack" => format!(
                "[stack]\n\
                 framework = {}  # react (v1)\n\
                 styling = {}  # tailwind (v1)\n\
                 tokens = {}  # token file, relative to this file\n",
                quote(self.stack.framework.as_str()),
                quote(self.stack.styling.as_str()),
                quote(&self.stack.tokens),
            ),
            "inspector" => format!(
                "[inspector]\n\
                 enabled = {}  # in development builds only\n\
                 shortcut = {}\n",
                self.inspector.enabled,
                quote(&self.inspector.shortcut),
            ),
            "verify" => format!(
                "[verify]\n\
                 loop = {}\n\
                 dev_server = {}  # local only\n\
                 threshold = {}  # advisory match score, 0-100\n",
                self.verify.enabled,
                quote(&self.verify.dev_server),
                self.verify.threshold,
            ),
            other => unreachable!("not a settings table: {other}"),
        }
    }
}

/// The `[files]` table as `figmog init` writes it.
pub(crate) fn render_files_table(product: &str, library: Option<&str>) -> String {
    let mut table = format!(
        "[files]\ndefault = {}  # the product file\n",
        quote(product)
    );
    if let Some(library) = library {
        table.push_str(&format!(
            "library = {}  # tokens and components\n",
            quote(library)
        ));
    }
    table
}

/// A TOML basic string.
fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A settings value as written.
#[derive(Debug, Clone, PartialEq)]
enum Scalar {
    Str(String),
    Bool(bool),
    Num(f64),
}

impl Scalar {
    fn kind(&self) -> &'static str {
        match self {
            Scalar::Str(_) => "a string",
            Scalar::Bool(_) => "a boolean",
            Scalar::Num(_) => "a number",
        }
    }
}

/// A string (as [`parse_string`]), `true`/`false`, or a decimal integer or
/// float (`_` separators allowed), and the text after it.
fn parse_scalar<'a>(input: &'a str, table: &str) -> Result<(Scalar, &'a str), String> {
    if input.starts_with('"') || input.starts_with('\'') {
        let (value, rest) = parse_string(input, table)?;
        return Ok((Scalar::Str(value), rest));
    }
    let end = input
        .find(|c: char| c.is_whitespace() || c == '#')
        .unwrap_or(input.len());
    let (word, rest) = input.split_at(end);
    match word {
        "true" => return Ok((Scalar::Bool(true), rest)),
        "false" => return Ok((Scalar::Bool(false), rest)),
        _ => {}
    }
    let numeric = word
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'_' | b'e' | b'E'));
    if numeric
        && word.bytes().any(|b| b.is_ascii_digit())
        && let Ok(number) = word.replace('_', "").parse::<f64>()
        && number.is_finite()
    {
        return Ok((Scalar::Num(number), rest));
    }
    Err(format!(
        "expected a string, boolean or number (arrays and inline tables are not supported in [{table}])"
    ))
}

/// Settings as written in one file (or merged), every key optional.
#[derive(Debug, Clone, Default, PartialEq)]
struct RawSettings {
    framework: Option<Framework>,
    styling: Option<Styling>,
    tokens: Option<String>,
    inspector_enabled: Option<bool>,
    shortcut: Option<String>,
    verify_loop: Option<bool>,
    dev_server: Option<String>,
    threshold: Option<f64>,
    token_format: Option<crate::tokens::Format>,
    asset_icons: Option<String>,
    asset_images: Option<String>,
    asset_manifest: Option<String>,
    icon_prefix: Option<String>,
    current_color: Option<bool>,
    asset_components: Option<ComponentLang>,
    /// `[tokens.modes]`, as written.
    mode_selectors: BTreeMap<String, String>,
    /// `<table>.<key>` of keys this binary does not know.
    unknown: BTreeSet<String>,
}

// `threshold` is validated finite (0–100), so equality is total.
impl Eq for RawSettings {}

impl RawSettings {
    /// Set a known `<table>.<key>` ([`RawSettings::is_known`]), validating
    /// its type and value. Errors are prefixed `<table>.<key>: `.
    /// The keys [`RawSettings::set`] knows, by table.
    const KNOWN: [(&'static str, &'static str); 15] = [
        ("stack", "framework"),
        ("stack", "styling"),
        ("stack", "tokens"),
        ("inspector", "enabled"),
        ("inspector", "shortcut"),
        ("verify", "loop"),
        ("verify", "dev_server"),
        ("verify", "threshold"),
        ("tokens", "format"),
        ("assets", "icons"),
        ("assets", "images"),
        ("assets", "manifest"),
        ("assets", "prefix"),
        ("assets", "current_color"),
        ("assets", "components"),
    ];

    fn is_known(table: &str, key: &str) -> bool {
        Self::KNOWN.contains(&(table, key))
    }

    fn set(&mut self, table: &str, key: &str, value: Scalar) -> Result<(), String> {
        let path = format!("{table}.{key}");
        let wrong =
            |want: &str, value: &Scalar| format!("{path}: expected {want}, found {}", value.kind());
        let string = |value: Scalar| match value {
            Scalar::Str(s) => Ok(s),
            other => Err(wrong("a string", &other)),
        };
        let boolean = |value: Scalar| match value {
            Scalar::Bool(b) => Ok(b),
            other => Err(wrong("a boolean (true or false)", &other)),
        };
        let invalid = |message: String| format!("{path}: {message}");
        match (table, key) {
            ("stack", "framework") => {
                self.framework = Some(Framework::parse(&string(value)?).map_err(invalid)?);
            }
            ("stack", "styling") => {
                self.styling = Some(Styling::parse(&string(value)?).map_err(invalid)?);
            }
            ("stack", "tokens") => {
                let tokens = string(value)?;
                check_project_path(&tokens).map_err(invalid)?;
                self.tokens = Some(tokens);
            }
            ("inspector", "enabled") => self.inspector_enabled = Some(boolean(value)?),
            ("inspector", "shortcut") => {
                let shortcut = string(value)?;
                check_shortcut(&shortcut).map_err(invalid)?;
                self.shortcut = Some(shortcut);
            }
            ("verify", "loop") => self.verify_loop = Some(boolean(value)?),
            ("verify", "dev_server") => {
                let url = string(value)?;
                check_dev_server(&url).map_err(invalid)?;
                self.dev_server = Some(url);
            }
            ("verify", "threshold") => {
                let Scalar::Num(threshold) = value else {
                    return Err(wrong("a number", &value));
                };
                if !(0.0..=100.0).contains(&threshold) {
                    return Err(invalid(format!(
                        "must be between 0 and 100, found {threshold}"
                    )));
                }
                self.threshold = Some(threshold);
            }
            ("tokens", "format") => {
                self.token_format =
                    Some(crate::tokens::Format::parse(&string(value)?).map_err(invalid)?);
            }
            ("assets", "icons") => {
                let dir = string(value)?;
                check_project_path(&dir).map_err(invalid)?;
                self.asset_icons = Some(dir);
            }
            ("assets", "images") => {
                let dir = string(value)?;
                check_project_path(&dir).map_err(invalid)?;
                self.asset_images = Some(dir);
            }
            ("assets", "manifest") => {
                let file = string(value)?;
                check_project_path(&file).map_err(invalid)?;
                self.asset_manifest = Some(file);
            }
            ("assets", "prefix") => {
                let prefix = string(value)?;
                check_icon_prefix(&prefix).map_err(invalid)?;
                self.icon_prefix = Some(prefix);
            }
            ("assets", "current_color") => self.current_color = Some(boolean(value)?),
            ("assets", "components") => {
                self.asset_components =
                    Some(ComponentLang::parse(&string(value)?).map_err(invalid)?);
            }
            _ => unreachable!("unknown keys are skipped before set: {path}"),
        }
        Ok(())
    }

    /// `other`'s keys replace this one's.
    fn merge(&mut self, other: RawSettings) {
        fn take<T>(mine: &mut Option<T>, theirs: Option<T>) {
            if theirs.is_some() {
                *mine = theirs;
            }
        }
        take(&mut self.framework, other.framework);
        take(&mut self.styling, other.styling);
        take(&mut self.tokens, other.tokens);
        take(&mut self.inspector_enabled, other.inspector_enabled);
        take(&mut self.shortcut, other.shortcut);
        take(&mut self.verify_loop, other.verify_loop);
        take(&mut self.dev_server, other.dev_server);
        take(&mut self.threshold, other.threshold);
        take(&mut self.token_format, other.token_format);
        take(&mut self.asset_icons, other.asset_icons);
        take(&mut self.asset_images, other.asset_images);
        take(&mut self.asset_manifest, other.asset_manifest);
        take(&mut self.icon_prefix, other.icon_prefix);
        take(&mut self.current_color, other.current_color);
        take(&mut self.asset_components, other.asset_components);
        self.mode_selectors.extend(other.mode_selectors);
        self.unknown.extend(other.unknown);
    }

    fn resolve(&self) -> Settings {
        Settings {
            stack: StackSettings {
                framework: self.framework.unwrap_or_default(),
                styling: self.styling.unwrap_or_default(),
                tokens: self
                    .tokens
                    .clone()
                    .unwrap_or_else(|| DEFAULT_TOKENS_PATH.to_string()),
            },
            inspector: InspectorSettings {
                enabled: self.inspector_enabled.unwrap_or(true),
                shortcut: self
                    .shortcut
                    .clone()
                    .unwrap_or_else(|| DEFAULT_SHORTCUT.to_string()),
            },
            verify: VerifySettings {
                enabled: self.verify_loop.unwrap_or(true),
                dev_server: self
                    .dev_server
                    .clone()
                    .unwrap_or_else(|| DEFAULT_DEV_SERVER.to_string()),
                threshold: self.threshold.unwrap_or(DEFAULT_THRESHOLD),
            },
            tokens: TokenSettings {
                format: self.token_format.unwrap_or_default(),
                modes: self.mode_selectors.clone(),
            },
            assets: AssetSettings {
                icons: self
                    .asset_icons
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ICONS_DIR.to_string()),
                images: self
                    .asset_images
                    .clone()
                    .unwrap_or_else(|| DEFAULT_IMAGES_DIR.to_string()),
                manifest: self
                    .asset_manifest
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ASSET_MANIFEST.to_string()),
                prefix: self
                    .icon_prefix
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ICON_PREFIX.to_string()),
                current_color: self.current_color.unwrap_or(false),
                components: self.asset_components.unwrap_or_default(),
            },
        }
    }
}

/// A file path inside the project: relative, `/`-separated, no `..`, no
/// control characters, its last segment a file name (not `.`).
pub(crate) fn check_project_path(value: &str) -> Result<(), String> {
    use std::path::Component;
    if value.is_empty() || value.ends_with('/') || value.chars().any(char::is_control) {
        return Err(format!("{value:?} is not a file path"));
    }
    let inside = !value.contains('\\')
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !inside {
        return Err(format!(
            "{value:?} must be a relative path inside the project (no `..`, not absolute)"
        ));
    }
    // `Path::components` drops a trailing `.`, so check the text: the last
    // segment must name a file (`.` and `src/.` are directories).
    if matches!(value.rsplit('/').next(), Some("" | "." | "..")) {
        return Err(format!("{value:?} is not a file path"));
    }
    Ok(())
}

/// An icon component name prefix: an ASCII capital letter, then up to 31
/// ASCII letters or digits — so every generated name is a valid,
/// capitalized JSX identifier whatever the Figma name was.
pub(crate) fn check_icon_prefix(value: &str) -> Result<(), String> {
    let valid = value.len() <= 32
        && value.starts_with(|c: char| c.is_ascii_uppercase())
        && value.chars().all(|c| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{value:?} is not an icon prefix (an ASCII capital letter, then up to 31 ASCII letters or digits)"
        ))
    }
}

/// `Modifier+…+Key`: at least one of `Ctrl`, `Alt`, `Shift`, `Meta` (any
/// case, each once), then one letter, digit or `F1`–`F12`.
pub(crate) fn check_shortcut(value: &str) -> Result<(), String> {
    let invalid = || {
        format!(
            "{value:?} is not a shortcut like \"Alt+Shift+F\" (one or more of Ctrl, Alt, Shift, \
             Meta, then a letter, digit or F1-F12)"
        )
    };
    let parts: Vec<&str> = value.split('+').collect();
    let Some((key, modifiers)) = parts.split_last() else {
        return Err(invalid());
    };
    let mut seen = BTreeSet::new();
    for modifier in modifiers {
        let lower = modifier.to_ascii_lowercase();
        if !matches!(lower.as_str(), "ctrl" | "alt" | "shift" | "meta") || !seen.insert(lower) {
            return Err(invalid());
        }
    }
    let function_key = key
        .strip_prefix(['F', 'f'])
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n) && !key[1..].starts_with('0'));
    let plain_key = key.len() == 1 && key.bytes().all(|b| b.is_ascii_alphanumeric());
    if seen.is_empty() || !(function_key || plain_key) {
        return Err(invalid());
    }
    Ok(())
}

/// An `http`/`https` URL on a loopback host (`localhost`, `*.localhost`,
/// `127.0.0.0/8`, `[::1]`) without credentials: v1 is local only.
pub(crate) fn check_dev_server(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|e| format!("{value:?} is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{value:?} must be an http:// or https:// URL"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{value:?} must not carry credentials"));
    }
    let local = match url.host() {
        Some(url::Host::Domain(domain)) => {
            let domain = domain.to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !local {
        return Err(format!(
            "{value:?} is not a local address — v1 supports a dev server on this machine only \
             (localhost, 127.0.0.1 or [::1])"
        ));
    }
    Ok(())
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

    /// This layout with `root` as the project root (and `<root>/.figmog`
    /// as the local root) and no config — `figmog init` choosing where to
    /// create `figmog.toml` (a monorepo package). The mirror root, shared
    /// by the repository, is unchanged.
    pub(crate) fn with_project_root(&self, root: PathBuf) -> Layout {
        let local_root = root.join(LOCAL_DIR);
        let mirror_root = if self.has_legacy_root() {
            self.mirror_root.clone()
        } else {
            local_root.clone()
        };
        Layout {
            project_root: root,
            config: ProjectConfig::default(),
            mirror_root,
            local_root,
        }
    }

    /// The project's typed settings ([`ProjectConfig::settings`]).
    pub(crate) fn settings(&self) -> Settings {
        self.config.settings()
    }

    /// `<project root>/figmog.toml`.
    pub(crate) fn config_path(&self) -> PathBuf {
        self.project_root.join(CONFIG_FILE)
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
        self.current_key()
    }

    /// `.figmog/current` alone (no config): the last file pulled here.
    pub(crate) fn current_key(&self) -> Result<Option<String>, String> {
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

    /// NWS-2946: the settings tables, their defaults and validation.
    #[test]
    fn settings_default_parse_and_override_per_key() {
        let defaults = parse_config("", CONFIG_FILE).unwrap().settings();
        assert_eq!(defaults, Settings::default());
        assert_eq!(defaults.stack.framework, Framework::React);
        assert_eq!(defaults.stack.styling, Styling::Tailwind);
        assert_eq!(defaults.stack.tokens, DEFAULT_TOKENS_PATH);
        assert!(defaults.inspector.enabled);
        assert_eq!(defaults.inspector.shortcut, DEFAULT_SHORTCUT);
        assert!(defaults.verify.enabled);
        assert_eq!(defaults.verify.dev_server, DEFAULT_DEV_SERVER);
        assert_eq!(defaults.verify.threshold, DEFAULT_THRESHOLD);

        let text = format!(
            "[files]\ndefault = \"{KEY}\"\nlibrary = \"{OTHER}\"\n\
             [stack]\nframework = 'react'\nstyling = \"tailwind\" # v1\ntokens = \"app/tokens.css\"\n\
             future = \"kept\"\n\
             [inspector]\nenabled = false\nshortcut = \"ctrl+F12\"\n\
             [verify]\nloop = false\ndev_server = \"http://127.0.0.1:3000/app\"\nthreshold = 87.5\n"
        );
        let config = parse_config(&text, CONFIG_FILE).unwrap();
        assert_eq!(config.library_key(), Some(OTHER));
        for table in ["files", "stack", "inspector", "verify"] {
            assert!(config.has_table(table), "{table}");
        }
        assert_eq!(
            config.unknown_keys().iter().collect::<Vec<_>>(),
            ["stack.future"]
        );
        let settings = config.settings();
        assert_eq!(settings.stack.tokens, "app/tokens.css");
        assert_eq!(
            settings.stack.tokens_path(Path::new("/p")),
            Path::new("/p/app/tokens.css")
        );
        assert!(!settings.inspector.enabled);
        assert_eq!(settings.inspector.shortcut, "ctrl+F12");
        assert!(!settings.verify.enabled);
        assert_eq!(settings.verify.dev_server, "http://127.0.0.1:3000/app");
        assert_eq!(settings.verify.threshold, 87.5);

        // figmog.local.toml replaces single keys, not whole tables.
        let mut merged = config.clone();
        merged.merge(
            parse_config(
                "[verify]\ndev_server = \"http://localhost:4000\"\n",
                LOCAL_CONFIG_FILE,
            )
            .unwrap(),
        );
        let settings = merged.settings();
        assert_eq!(settings.verify.dev_server, "http://localhost:4000");
        assert_eq!(settings.verify.threshold, 87.5);
        assert!(!settings.verify.enabled);
    }

    /// Review finding 3: an unknown settings key's value is skipped
    /// whatever its shape — arrays, inline tables, dates, multi-line
    /// strings, across lines — so a newer figmog's settings never break
    /// this binary; the keys around it still parse.
    #[test]
    fn unknown_settings_values_of_any_shape_are_skipped() {
        let text = "[verify]\n\
             viewports = [1440, 390]\n\
             nested = [\n  [1, 2], # ] in a comment\n  { a = \"]\" },\n  '''\n[files]\n]''',\n]\n\
             when = 1979-05-27T07:32:00Z\n\
             mode = { scheme = \"light\", states = [\"default\", \"hover\"] }\n\
             notes = \"\"\"\nthreshold = 1\n[stack]\n\"\"\"\n\
             threshold = 80\n\
             [stack]\n\
             tokens = \"src/t.css\"\n";
        let config = parse_config(text, CONFIG_FILE).unwrap();
        assert_eq!(
            config.unknown_keys().iter().collect::<Vec<_>>(),
            [
                "verify.mode",
                "verify.nested",
                "verify.notes",
                "verify.viewports",
                "verify.when"
            ]
        );
        let settings = config.settings();
        assert_eq!(settings.verify.threshold, 80.0);
        assert_eq!(settings.stack.tokens, "src/t.css");
        assert!(config.has_table("stack"));
        assert!(!config.has_table("files"));
    }

    /// NWS-2948: `[tokens]` and `[tokens.modes]` parse into typed settings,
    /// figmog.local.toml overrides single mappings, and unknown `[tokens]`
    /// keys or `[tokens.*]` tables stay forward-compatible.
    #[test]
    fn token_settings_parse_merge_and_stay_forward_compatible() {
        assert_eq!(
            Settings::default().tokens,
            TokenSettings {
                format: crate::tokens::Format::Tailwind,
                modes: BTreeMap::new(),
            }
        );
        let text = "[tokens]\nformat = \"css\"\nflavor = [\"dtcg\"]\n\n\
             [tokens.modes]\n\"Theme/Dark\" = '[data-theme=\"dark\"]'  # dark\n\
             \"Brand/Acme\" = \":root\"\n\"Brand/Off\" = \"\"\n\n\
             [tokens.typescript]\nout = { path = \"x.ts\" }\n";
        let config = parse_config(text, CONFIG_FILE).unwrap();
        assert!(config.has_table("tokens") && config.has_table("tokens.modes"));
        assert_eq!(
            config.unknown_keys().iter().collect::<Vec<_>>(),
            ["tokens.flavor"]
        );
        let tokens = config.settings().tokens;
        assert_eq!(tokens.format, crate::tokens::Format::Css);
        assert_eq!(
            tokens.modes.iter().collect::<Vec<_>>(),
            [
                (&"Brand/Acme".to_string(), &":root".to_string()),
                (&"Brand/Off".to_string(), &String::new()),
                (
                    &"Theme/Dark".to_string(),
                    &"[data-theme=\"dark\"]".to_string()
                ),
            ]
        );
        let mut merged = config.clone();
        merged.merge(
            parse_config(
                "[tokens.modes]\n\"Theme/Dark\" = \".dark\"\n",
                LOCAL_CONFIG_FILE,
            )
            .unwrap(),
        );
        let tokens = merged.settings().tokens;
        assert_eq!(tokens.modes["Theme/Dark"], ".dark");
        assert_eq!(tokens.modes["Brand/Acme"], ":root");
        assert_eq!(tokens.format, crate::tokens::Format::Css);
        assert_eq!(
            merged.settings().to_json()["tokens"],
            serde_json::json!({
                "format": "css",
                "modes": {"Brand/Acme": ":root", "Brand/Off": "", "Theme/Dark": ".dark"},
            })
        );
    }

    #[test]
    fn asset_settings_parse_validate_merge_and_stay_forward_compatible() {
        assert_eq!(
            Settings::default().assets,
            AssetSettings {
                icons: "src/icons".into(),
                images: "public/figma".into(),
                manifest: "figmog.assets.json".into(),
                prefix: "Icon".into(),
                current_color: false,
                components: ComponentLang::Tsx,
            }
        );
        let text = "[assets]\nicons = \"app/icons\"\nimages = 'public/img'\n\
             prefix = \"Glyph\"\ncurrent_color = true\ncomponents = \"jsx\"\n\
             sprite = { out = \"x.svg\" }\n";
        let config = parse_config(text, CONFIG_FILE).unwrap();
        assert_eq!(
            config.unknown_keys().iter().collect::<Vec<_>>(),
            ["assets.sprite"]
        );
        let mut merged = config.clone();
        merged.merge(
            parse_config(
                "[assets]\nmanifest = \"gen/assets.json\"\n",
                LOCAL_CONFIG_FILE,
            )
            .unwrap(),
        );
        assert_eq!(
            merged.settings().assets,
            AssetSettings {
                icons: "app/icons".into(),
                images: "public/img".into(),
                manifest: "gen/assets.json".into(),
                prefix: "Glyph".into(),
                current_color: true,
                components: ComponentLang::Jsx,
            }
        );
        assert_eq!(
            merged.settings().to_json()["assets"],
            serde_json::json!({
                "icons": "app/icons", "images": "public/img", "manifest": "gen/assets.json",
                "prefix": "Glyph", "current_color": true, "components": "jsx",
            })
        );
        for (bad, message) in [
            (
                "icons = \"../up\"",
                "figmog.toml:2: assets.icons: \"../up\" must be a relative path inside the project",
            ),
            (
                "images = \"/abs\"",
                "figmog.toml:2: assets.images: \"/abs\" must be a relative path inside the project",
            ),
            (
                "manifest = \"a/\"",
                "figmog.toml:2: assets.manifest: \"a/\" is not a file path",
            ),
            (
                "prefix = \"icon\"",
                "figmog.toml:2: assets.prefix: \"icon\" is not an icon prefix",
            ),
            (
                "prefix = \"I-con\"",
                "figmog.toml:2: assets.prefix: \"I-con\" is not an icon prefix",
            ),
            (
                "current_color = \"yes\"",
                "figmog.toml:2: assets.current_color: expected a boolean",
            ),
            (
                "components = \"vue\"",
                "figmog.toml:2: assets.components: unsupported language",
            ),
        ] {
            let err = parse_config(&format!("[assets]\n{bad}\n"), CONFIG_FILE).unwrap_err();
            assert!(err.starts_with(message), "{bad}: {err}");
        }
    }

    #[test]
    fn rendered_tables_parse_back_to_the_same_settings() {
        let mut settings = Settings::default();
        settings.stack.tokens = "src/\"odd\" dir/tokens.css".into();
        settings.verify.threshold = 90.25;
        settings.inspector.enabled = false;
        let mut text = render_files_table(KEY, Some(OTHER));
        for table in SETTINGS_TABLES {
            text.push_str(&settings.render_table(table));
        }
        let config = parse_config(&text, CONFIG_FILE).unwrap();
        assert_eq!(config.settings(), settings);
        assert_eq!(config.default_key(), Some(KEY));
        assert_eq!(config.library_key(), Some(OTHER));
        assert!(config.unknown_keys().is_empty());
    }

    #[test]
    fn malformed_settings_name_the_file_line_and_key() {
        for (text, line, needle) in [
            (
                "[stack]\nframework = \"vue\"\n",
                2,
                "stack.framework: unsupported framework \"vue\"",
            ),
            (
                "[stack]\nstyling = \"css-modules\"\n",
                2,
                "unsupported styling",
            ),
            (
                "[stack]\nframework = true\n",
                2,
                "stack.framework: expected a string, found a boolean",
            ),
            (
                "[stack]\ntokens = \"../x.css\"\n",
                2,
                "relative path inside the project",
            ),
            (
                "[stack]\ntokens = \"/abs.css\"\n",
                2,
                "relative path inside the project",
            ),
            ("[stack]\ntokens = \"src/\"\n", 2, "not a file path"),
            ("[inspector]\nenabled = \"yes\"\n", 2, "expected a boolean"),
            ("[inspector]\nshortcut = \"F\"\n", 2, "not a shortcut"),
            (
                "[inspector]\nshortcut = \"Alt+Alt+F\"\n",
                2,
                "not a shortcut",
            ),
            ("[inspector]\nshortcut = \"Hyper+F\"\n", 2, "not a shortcut"),
            ("[inspector]\nshortcut = \"Alt+F13\"\n", 2, "not a shortcut"),
            (
                "[verify]\ndev_server = \"http://192.168.1.2:5173\"\n",
                2,
                "not a local address",
            ),
            (
                "[verify]\ndev_server = \"ftp://localhost\"\n",
                2,
                "http:// or https://",
            ),
            (
                "[verify]\ndev_server = \"http://u:p@localhost\"\n",
                2,
                "credentials",
            ),
            (
                "[verify]\ndev_server = \"localhost:5173\"\n",
                2,
                "http:// or https://",
            ),
            ("[verify]\nthreshold = 101\n", 2, "between 0 and 100"),
            (
                "[tokens]\nformat = \"dtcg\"\n",
                2,
                "tokens.format: unsupported token format \"dtcg\"",
            ),
            (
                "[tokens.modes]\n\"Theme/Dark\" = \".a, .b\"\n",
                2,
                "tokens.modes: \".a, .b\" is not one CSS selector",
            ),
            (
                "[tokens.modes]\nDark = \".dark\"\n",
                2,
                "tokens.modes: \"Dark\" is not \"<collection>/<mode>\"",
            ),
            (
                "[tokens.modes]\n\"Theme/Dark\" = true\n",
                2,
                "expected a quoted string",
            ),
            (
                "[tokens.modes]\n\"Theme/Dark\" = \".a\"\n'Theme/Dark' = \".b\"\n",
                3,
                "duplicate key tokens.modes.\"Theme/Dark\"",
            ),
            ("[verify]\nthreshold = \"95\"\n", 2, "expected a number"),
            (
                "[verify]\nthreshold = nan\n",
                2,
                "expected a string, boolean or number",
            ),
            (
                "[verify]\nthreshold = [95]\n",
                2,
                "arrays and inline tables are not supported",
            ),
            ("[verify]\nfuture = [\n1,\n", 2, "unterminated value"),
            (
                "[verify]\nfuture =\n",
                2,
                "expected a value for verify.future",
            ),
            (
                "[verify]\nfuture = 1\nfuture = 2\n",
                3,
                "duplicate key verify.future",
            ),
            ("[stack]\ntokens = \".\"\n", 2, "not a file path"),
            ("[stack]\ntokens = \"src/.\"\n", 2, "not a file path"),
            ("[stack]\ntokens = \"./\"\n", 2, "not a file path"),
            (
                "[verify]\nloop = true\nloop = false\n",
                3,
                "duplicate key verify.loop",
            ),
            (
                "[verify]\n[stack]\n[verify]\n",
                3,
                "duplicate [verify] table",
            ),
            ("[stack]\nframework\n", 2, "expected `framework = <value>`"),
            (
                "[stack]\nframework = \"react\" extra\n",
                2,
                "unexpected text",
            ),
            (
                "[stack]\na.b = 1\n",
                2,
                "dotted keys are not supported in [stack]",
            ),
            (
                "[stack]\ntokens = '''\n",
                2,
                "multi-line strings are not supported in [stack]",
            ),
        ] {
            let error = parse_config(text, CONFIG_FILE).unwrap_err();
            assert!(
                error.starts_with(&format!("figmog.toml:{line}: ")) && error.contains(needle),
                "{text:?}: {error}"
            );
        }
        // Accepted spellings.
        for shortcut in ["Alt+Shift+F", "ctrl+meta+7", "Shift+f1", "Alt+F12"] {
            let text = format!("[inspector]\nshortcut = \"{shortcut}\"\n");
            assert!(parse_config(&text, CONFIG_FILE).is_ok(), "{shortcut}");
        }
        for url in [
            "http://localhost:5173",
            "https://app.localhost/",
            "http://127.0.0.2:80",
            "http://[::1]:3000",
        ] {
            let text = format!("[verify]\ndev_server = \"{url}\"\n");
            assert!(parse_config(&text, CONFIG_FILE).is_ok(), "{url}");
        }
        let config =
            parse_config("[verify]\nthreshold = 1_0\nextra = -2.5e1\n", CONFIG_FILE).unwrap();
        assert_eq!(config.settings().verify.threshold, 10.0);
        // Other tables keep being skipped unparsed.
        assert!(parse_config("[later]\nlist = [\n 1,\n]\n", CONFIG_FILE).is_ok());
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
