//! `figmog tokens export` (NWS-2967): render the imported variable
//! snapshot as one CSS file — custom properties for the default modes, one
//! collection's light/dark modes as `:root` / `[data-theme="dark"]`, and a
//! Tailwind v4 `@theme` / `@theme inline` block.
//!
//! Pure: the input is exactly what `figmog vars --snapshot` returns
//! (`query::vars` with `with_snapshot`), so the command reads the store
//! through the ordinary `figmog_vars` read — socket-routed or direct — and
//! only the rendering lives here. Output is a function of that value
//! alone: every list is sorted, nothing reads a clock or a `HashMap`.
//!
//! Rules (SPEC §9 "Token export"):
//!
//! - **Names.** A variable's `codeSyntax.WEB` (`--x` or `var(--x)`) when it
//!   is a valid custom property name (`--` then ASCII letters, digits, `-`
//!   or `_`); otherwise the kebab-cased variable name path (`/` segments,
//!   lower-cased, every other run of non-alphanumerics a single `-`). An
//!   invalid WEB name falls back to the path and is reported. Two
//!   variables producing one name are an error naming both.
//! - **Values.** COLOR → `#rrggbb`, or `#rrggbbaa` when alpha < 1 (each
//!   channel `round(x * 255)`). FLOAT → `<n>px` when its scopes include a
//!   spacing/size scope (`GAP`, `WIDTH_HEIGHT`) or `CORNER_RADIUS`, else a
//!   unitless number (reported). An alias → `var(--target)`. STRING,
//!   BOOLEAN and other types are skipped and reported, as is a variable
//!   lacking a value for a mode it needs or aliasing a skipped variable.
//! - **Tailwind namespaces.** COLOR → `--color-*`, spacing/size scopes →
//!   `--spacing-*`, `CORNER_RADIUS` → `--radius-*`; a name already in its
//!   namespace is kept, otherwise the namespace is prefixed
//!   (`--brand` → `--color-brand`). Unitless FLOATs get no theme entry.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

/// Which collection's modes become `:root` / `[data-theme="dark"]`.
#[derive(Debug, Default, Clone)]
pub struct ExportOptions {
    pub collection: Option<String>,
    pub light_mode: Option<String>,
    pub dark_mode: Option<String>,
}

/// One reported variable: skipped, emitted unitless, or renamed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ReportEntry {
    pub variable_id: String,
    pub name: String,
    pub reason: String,
}

/// The chosen light/dark collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Theme {
    pub collection: String,
    pub light_mode: String,
    pub dark_mode: String,
}

/// A rendered export.
#[derive(Debug)]
pub struct Export {
    pub css: String,
    /// Variables emitted as custom properties.
    pub tokens: usize,
    pub theme: Option<Theme>,
    /// Sorted by variable id, then reason.
    pub report: Vec<ReportEntry>,
    /// Variable id → the custom property name it exports as (`--x`), for
    /// every emitted variable — what `figmog spec` (NWS-2968) shows as a
    /// bound token's `css`, so the two can't drift.
    pub names: BTreeMap<String, String>,
}

/// One authoritative variable row of `figmog vars --snapshot`.
struct Var {
    id: String,
    name: String,
    resolved_type: String,
    collection: String,
    default_mode: String,
    scopes: Vec<String>,
    web: Option<String>,
    /// Mode name → raw value.
    values: BTreeMap<String, Value>,
}

/// How a FLOAT is emitted, from its scopes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Color,
    Spacing,
    Radius,
    Number,
}

impl Kind {
    fn namespace(self) -> Option<&'static str> {
        match self {
            Kind::Color => Some("--color-"),
            Kind::Spacing => Some("--spacing-"),
            Kind::Radius => Some("--radius-"),
            Kind::Number => None,
        }
    }
}

/// A value in one mode: a literal CSS value, or an alias to a variable id.
#[derive(Clone, PartialEq, Eq)]
enum Val {
    Literal(String),
    Alias(String),
}

/// Render `vars` (`{snapshot, variables}`, as `figmog vars --snapshot`
/// returns it) to CSS. Errors: no current snapshot, an ambiguous or
/// unknown theme collection/mode, a name collision.
pub fn export_css(vars: &Value, opts: &ExportOptions) -> Result<Export, String> {
    let snapshot = &vars["snapshot"];
    let state = snapshot["state"].as_str().unwrap_or("none");
    if state != "current" {
        return Err(match state {
            "legacy" => "no variable snapshot provenance (state: legacy) — re-run `figmog import-variables <export.json>` (or an Enterprise pull) to record one before exporting".to_string(),
            "empty" => "the variable snapshot holds no variables (state: empty) — nothing to export".to_string(),
            _ => format!("no variable snapshot (state: {state}) — run `figmog import-variables <export.json>` first"),
        });
    }
    let mut report = BTreeSet::new();
    let rows = vars["variables"].as_array().cloned().unwrap_or_default();
    let mut all: BTreeMap<String, Var> = BTreeMap::new();
    for row in &rows {
        let id = row["variable_id"].as_str().unwrap_or_default().to_string();
        if row["source"] != "imported" {
            report.insert(entry(
                &id,
                "",
                "bound in the file but not in the variable snapshot: skipped",
            ));
            continue;
        }
        let Some(default_mode) = row["default_mode"].as_str() else {
            return Err("the variables read carries no `default_mode` — the figmog serving this mirror is older than this CLI; restart it (kill the pid in owner.json) and retry".into());
        };
        let text = |k: &str| row[k].as_str().unwrap_or_default().to_string();
        all.insert(
            id.clone(),
            Var {
                id,
                name: text("name"),
                resolved_type: text("resolved_type"),
                collection: text("collection"),
                default_mode: default_mode.to_string(),
                scopes: row["scopes"]
                    .as_array()
                    .map(|xs| {
                        xs.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                web: row["code_syntax"]["WEB"].as_str().map(str::to_string),
                values: row["values_by_mode"]
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default(),
            },
        );
    }

    let theme = choose_theme(&all, opts)?;
    // The modes each variable is emitted in: its root mode (the theme's
    // light mode for the theme collection, else its default) and, for the
    // theme collection, the dark mode.
    let modes_of = |v: &Var| -> Vec<String> {
        match &theme {
            Some(t) if t.collection == v.collection => {
                vec![t.light_mode.clone(), t.dark_mode.clone()]
            }
            _ => vec![v.default_mode.clone()],
        }
    };

    // First pass: type, name and per-mode values.
    struct Token {
        name: String,
        kind: Kind,
        vals: Vec<Val>,
    }
    let mut tokens: BTreeMap<String, Token> = BTreeMap::new();
    for v in all.values() {
        let kind = match v.resolved_type.as_str() {
            "COLOR" => Kind::Color,
            "FLOAT" => {
                let has = |s: &[&str]| v.scopes.iter().any(|x| s.contains(&x.as_str()));
                if has(&["CORNER_RADIUS"]) {
                    Kind::Radius
                } else if has(&["GAP", "WIDTH_HEIGHT"]) {
                    Kind::Spacing
                } else {
                    Kind::Number
                }
            }
            other => {
                let t = if other.is_empty() { "unknown" } else { other };
                report.insert(entry(
                    &v.id,
                    &v.name,
                    &format!("{t} variable: skipped (only COLOR and FLOAT are exported)"),
                ));
                continue;
            }
        };
        let name = match v.web.as_deref().map(web_name) {
            Some(Some(n)) => n,
            web => {
                if let Some(None) = web {
                    report.insert(entry(
                        &v.id,
                        &v.name,
                        &format!(
                            "codeSyntax.WEB {:?} is not a valid custom property name: used the name path",
                            v.web.as_deref().unwrap_or_default()
                        ),
                    ));
                }
                match path_name(&v.name) {
                    Some(n) => n,
                    None => {
                        report.insert(entry(
                            &v.id,
                            &v.name,
                            "name has no ASCII letters or digits to build a CSS name from: skipped",
                        ));
                        continue;
                    }
                }
            }
        };
        let mut vals = Vec::new();
        let mut failed = None;
        for mode in modes_of(v) {
            match v.values.get(&mode).map(|raw| value(raw, kind)) {
                Some(Ok(val)) => vals.push(val),
                Some(Err(why)) => {
                    failed = Some(format!("value in mode {mode:?} {why}: skipped"));
                    break;
                }
                None => {
                    failed = Some(format!("no value for mode {mode:?}: skipped"));
                    break;
                }
            }
        }
        if let Some(why) = failed {
            report.insert(entry(&v.id, &v.name, &why));
            continue;
        }
        if kind == Kind::Number {
            report.insert(entry(
                &v.id,
                &v.name,
                "FLOAT without a spacing, size or radius scope: emitted unitless, no Tailwind theme entry",
            ));
        }
        tokens.insert(v.id.clone(), Token { name, kind, vals });
    }
    // Drop aliases to variables that aren't exported, until none remain
    // (an alias chain to a skipped variable drops as a whole).
    loop {
        let dangling: Vec<(String, String)> = tokens
            .iter()
            .filter_map(|(id, t)| {
                t.vals.iter().find_map(|val| match val {
                    Val::Alias(target) if !tokens.contains_key(target) => {
                        Some((id.clone(), target.clone()))
                    }
                    _ => None,
                })
            })
            .collect();
        if dangling.is_empty() {
            break;
        }
        for (id, target) in dangling {
            tokens.remove(&id);
            let name = all.get(&id).map(|v| v.name.as_str()).unwrap_or_default();
            report.insert(entry(
                &id,
                name,
                &format!("aliases {target}, which is not exported: skipped"),
            ));
        }
    }

    // Collisions: custom property names first, then theme names against
    // every name already in use.
    let label = |id: &str| {
        let name = all.get(id).map(|v| v.name.as_str()).unwrap_or_default();
        format!("`{name}` ({id})")
    };
    let mut by_name: BTreeMap<&str, &str> = BTreeMap::new();
    for (id, t) in &tokens {
        if let Some(other) = by_name.insert(&t.name, id) {
            return Err(format!(
                "token name collision: {} and {} both export as `{}` — rename one or set its WEB code syntax",
                label(other),
                label(id),
                t.name
            ));
        }
    }
    let ref_of = |val: &Val| match val {
        Val::Literal(s) => s.clone(),
        Val::Alias(target) => format!("var({})", tokens[target].name),
    };
    let mut theme_plain: BTreeMap<String, String> = BTreeMap::new();
    let mut theme_inline: BTreeMap<String, String> = BTreeMap::new();
    let mut theme_names: BTreeMap<String, &str> = BTreeMap::new();
    for (id, t) in &tokens {
        let Some(prefix) = t.kind.namespace() else {
            continue;
        };
        let theme_name = if t.name.len() > prefix.len() && t.name.starts_with(prefix) {
            t.name.clone()
        } else {
            format!("{prefix}{}", &t.name[2..])
        };
        let clash = theme_names.get(&theme_name).copied().or_else(|| {
            (theme_name != t.name)
                .then(|| by_name.get(theme_name.as_str()).copied())
                .flatten()
        });
        if let Some(other) = clash {
            return Err(format!(
                "Tailwind theme name collision: {} and {} both claim `{theme_name}` — rename one or set its WEB code syntax",
                label(other),
                label(id),
            ));
        }
        theme_names.insert(theme_name.clone(), id);
        let same_alias = match t.vals.first() {
            Some(first @ Val::Alias(_)) => t.vals.iter().all(|v| v == first),
            _ => false,
        };
        if theme_name != t.name {
            // Reference the token itself, inline, so a utility reads it
            // where it's used and follows the theme there.
            theme_inline.insert(theme_name, format!("var({})", t.name));
        } else if same_alias {
            theme_inline.insert(theme_name, ref_of(&t.vals[0]));
        } else {
            theme_plain.insert(theme_name, ref_of(&t.vals[0]));
        }
    }

    // Emit.
    let mut root: BTreeMap<&str, String> = BTreeMap::new();
    let mut dark: BTreeMap<&str, String> = BTreeMap::new();
    for t in tokens.values() {
        root.insert(&t.name, ref_of(&t.vals[0]));
        if let Some(val) = t.vals.get(1) {
            dark.insert(&t.name, ref_of(val));
        }
    }
    let field = |k: &str| snapshot[k].as_str().map(comment_safe);
    let opt = |v: Option<String>| v.unwrap_or_else(|| "null".to_string());
    let mut css = String::new();
    css.push_str("/* Generated by `figmog tokens export` — do not edit by hand.\n");
    css.push_str(&format!(
        " * snapshot: source={} format={} format_version={} exported_at={}\n",
        opt(field("source")),
        opt(field("format")),
        opt(field("format_version")),
        opt(field("exported_at")),
    ));
    css.push_str(&format!(" * digest: {}\n", opt(field("digest"))));
    match &theme {
        Some(t) => css.push_str(&format!(
            " * theme: collection \"{}\": mode \"{}\" -> :root, mode \"{}\" -> [data-theme=\"dark\"]\n",
            comment_safe(&t.collection),
            comment_safe(&t.light_mode),
            comment_safe(&t.dark_mode),
        )),
        None => css.push_str(" * theme: none (no collection with a dark mode)\n"),
    }
    css.push_str(" */\n");
    block(&mut css, ":root", &root);
    if theme.is_some() {
        block(&mut css, "[data-theme=\"dark\"]", &dark);
    }
    block(&mut css, "@theme", &theme_plain);
    block(&mut css, "@theme inline", &theme_inline);
    Ok(Export {
        css,
        tokens: tokens.len(),
        theme,
        report: report.into_iter().collect(),
        names: tokens
            .iter()
            .map(|(id, t)| (id.clone(), t.name.clone()))
            .collect(),
    })
}

fn entry(id: &str, name: &str, reason: &str) -> ReportEntry {
    ReportEntry {
        variable_id: id.to_string(),
        name: name.to_string(),
        reason: reason.to_string(),
    }
}

/// Append `selector { name: value; ... }` (sorted), or nothing when empty.
fn block<K: AsRef<str>>(css: &mut String, selector: &str, decls: &BTreeMap<K, String>) {
    if decls.is_empty() {
        return;
    }
    css.push('\n');
    css.push_str(selector);
    css.push_str(" {\n");
    for (name, value) in decls {
        css.push_str(&format!("  {}: {value};\n", name.as_ref()));
    }
    css.push_str("}\n");
}

/// Text safe inside a `/* ... */` comment on one line.
fn comment_safe(s: &str) -> String {
    s.replace("*/", "* /").replace(['\n', '\r'], " ")
}

/// The theme collection and its light/dark modes: explicit flags first;
/// otherwise the one collection with a mode named like "dark" (none ⇒ no
/// theme blocks; several ⇒ an error asking for `--collection`).
fn choose_theme(
    all: &BTreeMap<String, Var>,
    opts: &ExportOptions,
) -> Result<Option<Theme>, String> {
    // Collection name → (mode names, default modes).
    let mut collections: BTreeMap<&str, (BTreeSet<&str>, BTreeSet<&str>)> = BTreeMap::new();
    for v in all.values() {
        let c = collections.entry(&v.collection).or_default();
        c.0.extend(v.values.keys().map(String::as_str));
        c.0.insert(&v.default_mode);
        c.1.insert(&v.default_mode);
    }
    let like = |mode: &str, word: &str| mode.to_ascii_lowercase().contains(word);
    let collection = match &opts.collection {
        Some(name) => {
            if !collections.contains_key(name.as_str()) {
                return Err(format!(
                    "no variable collection named {name:?} (collections: {})",
                    quoted(collections.keys())
                ));
            }
            name.as_str()
        }
        None => {
            let with_dark: Vec<&str> = collections
                .iter()
                .filter(|(_, (modes, _))| modes.iter().any(|m| like(m, "dark")))
                .map(|(name, _)| *name)
                .collect();
            match with_dark.as_slice() {
                [one] => *one,
                [] if opts.light_mode.is_none() && opts.dark_mode.is_none() => return Ok(None),
                [] => {
                    return Err(format!(
                        "no collection has a mode named like \"dark\" — pass --collection (collections: {})",
                        quoted(collections.keys())
                    ));
                }
                many => {
                    return Err(format!(
                        "several collections have a mode named like \"dark\" ({}) — pass --collection",
                        quoted(many.iter())
                    ));
                }
            }
        }
    };
    let (modes, defaults) = &collections[collection];
    let pick = |flag: &Option<String>, word: &str, what: &str| -> Result<Option<String>, String> {
        if let Some(m) = flag {
            if !modes.contains(m.as_str()) {
                return Err(format!(
                    "collection {collection:?} has no mode named {m:?} (modes: {})",
                    quoted(modes.iter())
                ));
            }
            return Ok(Some(m.clone()));
        }
        let hits: Vec<&&str> = modes.iter().filter(|m| like(m, word)).collect();
        match hits.as_slice() {
            [one] => Ok(Some(one.to_string())),
            [] => Ok(None),
            _ => Err(format!(
                "collection {collection:?} has several modes named like {word:?} ({}) — pass --{what}-mode",
                quoted(hits.iter())
            )),
        }
    };
    let dark = pick(&opts.dark_mode, "dark", "dark")?.ok_or_else(|| {
        format!(
            "collection {collection:?} has no mode named like \"dark\" — pass --dark-mode (modes: {})",
            quoted(modes.iter())
        )
    })?;
    let light = match pick(&opts.light_mode, "light", "light")? {
        Some(m) => m,
        None => match defaults.iter().collect::<Vec<_>>().as_slice() {
            [one] => one.to_string(),
            _ => {
                return Err(format!(
                    "collection {collection:?} has no single default or \"light\" mode — pass --light-mode"
                ));
            }
        },
    };
    if light == dark {
        return Err(format!(
            "the light and dark modes are both {light:?} — pass --light-mode/--dark-mode"
        ));
    }
    Ok(Some(Theme {
        collection: collection.to_string(),
        light_mode: light,
        dark_mode: dark,
    }))
}

fn quoted<'a, S: AsRef<str> + 'a>(names: impl Iterator<Item = S>) -> String {
    names
        .map(|n| format!("{:?}", n.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `codeSyntax.WEB` value as a custom property name: `--x` or
/// `var(--x)`, `x` nonempty ASCII letters/digits/`-`/`_`. `None` if invalid.
fn web_name(web: &str) -> Option<String> {
    let web = web.trim();
    let inner = web
        .strip_prefix("var(")
        .and_then(|s| s.strip_suffix(')'))
        .map(str::trim)
        .unwrap_or(web);
    let body = inner.strip_prefix("--")?;
    (!body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| inner.to_string())
}

/// The kebab-cased name path: `Color/Brand Primary` → `--color-brand-primary`.
fn path_name(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    (!out.is_empty()).then(|| format!("--{out}"))
}

/// One mode's raw value as CSS, or why it can't be.
fn value(raw: &Value, kind: Kind) -> Result<Val, String> {
    if raw["type"] == "VARIABLE_ALIAS" {
        return raw["id"]
            .as_str()
            .map(|id| Val::Alias(id.to_string()))
            .ok_or_else(|| "is an alias without a target id".to_string());
    }
    match kind {
        Kind::Color => color(raw)
            .map(Val::Literal)
            .ok_or_else(|| "is not an {r, g, b, a} color".to_string()),
        _ => {
            let n = raw
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| "is not a number".to_string())?;
            let n = number(n);
            Ok(Val::Literal(match kind {
                Kind::Number => n,
                _ => format!("{n}px"),
            }))
        }
    }
}

/// `{r, g, b, a}` (0–1 floats) as `#rrggbb`, or `#rrggbbaa` below full alpha.
pub(crate) fn color(raw: &Value) -> Option<String> {
    let channel = |k: &str, default: Option<f64>| -> Option<u8> {
        let x = raw.get(k).and_then(Value::as_f64).or(default)?;
        x.is_finite()
            .then(|| (x.clamp(0.0, 1.0) * 255.0).round() as u8)
    };
    let (r, g, b) = (
        channel("r", None)?,
        channel("g", None)?,
        channel("b", None)?,
    );
    let a = channel("a", Some(1.0))?;
    Some(if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    })
}

/// A finite number in its shortest round-tripping decimal form (`16`,
/// `0.5`); `-0` prints as `0`.
fn number(n: f64) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    format!("{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(
        id: &str,
        name: &str,
        ty: &str,
        values: Value,
        scopes: Value,
        web: Option<&str>,
    ) -> Value {
        json!({
            "variable_id": id, "source": "imported", "name": name,
            "resolved_type": ty, "collection": "c", "default_mode": "light",
            "scopes": scopes, "values_by_mode": values,
            "code_syntax": web.map(|w| json!({"WEB": w})),
        })
    }

    fn snap(rows: Vec<Value>) -> Value {
        json!({
            "snapshot": {"state": "current", "source": "file", "format": "bare",
                          "format_version": null, "exported_at": null, "digest": "fnv1a64:0"},
            "variables": rows,
        })
    }

    #[test]
    fn names() {
        assert_eq!(
            web_name("var(--color-brand)").as_deref(),
            Some("--color-brand")
        );
        assert_eq!(web_name(" --x_1 ").as_deref(), Some("--x_1"));
        assert_eq!(web_name("colorBrand"), None);
        assert_eq!(web_name("--"), None);
        assert_eq!(web_name("--a b"), None);
        assert_eq!(
            path_name("Color/Brand Primary").as_deref(),
            Some("--color-brand-primary")
        );
        assert_eq!(path_name("/ -- /"), None);
    }

    #[test]
    fn values() {
        assert_eq!(
            color(&json!({"r": 1, "g": 0.5, "b": 0, "a": 1})).as_deref(),
            Some("#ff8000")
        );
        assert_eq!(
            color(&json!({"r": 0, "g": 0, "b": 0, "a": 0.5})).as_deref(),
            Some("#00000080")
        );
        assert_eq!(number(16.0), "16");
        assert_eq!(number(-0.0), "0");
        assert_eq!(number(0.25), "0.25");
    }

    #[test]
    fn collision_names_both_variables() {
        let v = snap(vec![
            row(
                "V:1",
                "brand/primary",
                "COLOR",
                json!({"light": {"r":0,"g":0,"b":0,"a":1}}),
                json!([]),
                None,
            ),
            row(
                "V:2",
                "brand primary",
                "COLOR",
                json!({"light": {"r":0,"g":0,"b":0,"a":1}}),
                json!([]),
                None,
            ),
        ]);
        let err = export_css(&v, &ExportOptions::default()).unwrap_err();
        assert!(err.contains("`brand/primary` (V:1)"), "{err}");
        assert!(err.contains("`brand primary` (V:2)"), "{err}");
        assert!(err.contains("--brand-primary"), "{err}");
    }

    #[test]
    fn theme_name_collision_is_an_error() {
        let black = json!({"light": {"r":0,"g":0,"b":0,"a":1}});
        let v = snap(vec![
            row("V:1", "brand", "COLOR", black.clone(), json!([]), None),
            row("V:2", "x", "COLOR", black, json!([]), Some("--color-brand")),
        ]);
        let err = export_css(&v, &ExportOptions::default()).unwrap_err();
        assert!(err.contains("Tailwind theme name collision"), "{err}");
        assert!(err.contains("(V:1)") && err.contains("(V:2)"), "{err}");
    }

    #[test]
    fn ambiguous_dark_collections_need_a_flag() {
        let mut a = row(
            "V:1",
            "a",
            "FLOAT",
            json!({"light": 1, "dark": 2}),
            json!(["GAP"]),
            None,
        );
        let mut b = a.clone();
        a["collection"] = json!("one");
        b["variable_id"] = json!("V:2");
        b["name"] = json!("b");
        b["collection"] = json!("two");
        let v = snap(vec![a, b]);
        let err = export_css(&v, &ExportOptions::default()).unwrap_err();
        assert!(err.contains("pass --collection"), "{err}");
        let ok = export_css(
            &v,
            &ExportOptions {
                collection: Some("two".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ok.theme.unwrap().collection, "two");
        assert!(
            ok.css.contains("[data-theme=\"dark\"] {\n  --b: 2px;\n}"),
            "{}",
            ok.css
        );
    }

    #[test]
    fn requires_a_current_snapshot() {
        let mut v = snap(vec![]);
        v["snapshot"]["state"] = json!("legacy");
        assert!(
            export_css(&v, &ExportOptions::default())
                .unwrap_err()
                .contains("legacy")
        );
    }
}
