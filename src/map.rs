//! The Figma → code component map (NWS-2949): `figmog.map.json`, a small,
//! hand-confirmed, committed file at the project root that links a code
//! component (a file inside the project and its export) to a Figma
//! component or component set, and maps the Figma component properties
//! (`Tier=Pro`, a text, a boolean, an instance swap) to the code
//! component's props (`tier="pro"`).
//!
//! ```json
//! {
//!   "version": 1,
//!   "components": [
//!     {
//!       "figma": {"component_set": "<component set key>"},
//!       "code": {"path": "src/components/PricingCard.tsx", "export": "PricingCard"},
//!       "props": {
//!         "Tier": {"prop": "tier", "values": {"Free": "free", "Pro": "pro"}},
//!         "Title": {"prop": "title"}
//!       }
//!     }
//!   ]
//! }
//! ```
//!
//! **Identity.** A Figma target is named by its **key** (`component` or
//! `component_set`), never its node id: the key is what the REST API
//! publishes for a component, survives renames and moves, and is the same
//! in the library file and in every file that uses the component (an
//! instance's `componentId` is a file-local node id that differs from file
//! to file). A `component` entry (one variant, or a standalone component)
//! wins over its set's `component_set` entry.
//!
//! **Strict.** The file is user-edited, so every error names the file and
//! the JSON path (`figmog.map.json: components[0].props["Tier"].prop: …`)
//! and unknown keys are errors, never ignored.
//!
//! **Honest coverage.** [`code_component`] never claims an instance is
//! covered while it carries something the map can't express: an instance
//! property the map doesn't name (or whose value it doesn't list; a
//! `component` entry's identity expresses its variant properties), a
//! property bound to a variable, an override (text, visibility, style —
//! on the instance or on any nested instance) or a nested instance swap
//! (checked against the main component) that no mapped property
//! explains, a nested instance that can't be checked, or slot content.
//! Each is listed and the mapping is `"partial"`.
//!
//! Determinism: every object is a `serde_json::Map` or a `BTreeMap`
//! (sorted keys); every list is sorted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value, json};

/// The map file's name, at the project root.
pub(crate) const MAP_FILE: &str = "figmog.map.json";

/// The only map format version this figmog reads.
const VERSION: u64 = 1;

/// What a map entry names in Figma, by key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    /// One component: a variant, or a standalone component.
    Component(String),
    /// A component set: every variant in it.
    ComponentSet(String),
}

impl Target {
    fn kind(&self) -> &'static str {
        match self {
            Target::Component(_) => "component",
            Target::ComponentSet(_) => "component_set",
        }
    }

    fn key(&self) -> &str {
        match self {
            Target::Component(k) | Target::ComponentSet(k) => k,
        }
    }

    fn to_json(&self) -> Value {
        json!({self.kind(): self.key()})
    }
}

/// One Figma component property → one code prop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropMap {
    /// The code prop's name.
    pub(crate) prop: String,
    /// Figma value (as text; for an instance swap, the swapped-in
    /// component's key) → code value (string, number, boolean, or `null`:
    /// leave the prop out, the code default). Absent: the Figma value
    /// passes through as is (a string, or a boolean).
    pub(crate) values: Option<BTreeMap<String, Value>>,
}

/// One map entry: a Figma target and the code component implementing it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) figma: Target,
    /// The code file, relative to the project root (`/`-separated).
    pub(crate) path: String,
    /// The export name (a capitalized JS identifier: a JSX component).
    pub(crate) export: String,
    /// Figma property name → code prop. A name without `#` also matches
    /// Figma's suffixed name (`Label` matches `Label#1:0`).
    pub(crate) props: BTreeMap<String, PropMap>,
}

/// A validated `figmog.map.json`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CodeMap {
    pub(crate) entries: Vec<Entry>,
}

/// Reject `obj`'s keys outside `allowed`, naming the first one.
fn only_keys(obj: &Map<String, Value>, allowed: &[&str], at: &str) -> Result<(), String> {
    match obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!(
            "{at}: unknown key {k:?} (expected {})",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

fn object<'a>(v: &'a Value, at: &str, what: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object()
        .ok_or_else(|| format!("{at}: expected an object {what}"))
}

fn string<'a>(v: Option<&'a Value>, at: &str) -> Result<&'a str, String> {
    match v {
        None => Err(format!("{at}: missing")),
        Some(Value::String(s)) if s.is_empty() => Err(format!("{at}: empty")),
        Some(Value::String(s)) if s.chars().any(char::is_control) => {
            Err(format!("{at}: contains a control character"))
        }
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(format!("{at}: expected a string")),
    }
}

/// A JSX component name: a capitalized JS identifier.
fn check_export(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a JSX component name (a JS identifier starting with an uppercase letter)"
        ))
    }
}

/// A JSX attribute name: a JS identifier, `-` allowed after the first
/// character (`aria-label`, `data-tier`).
fn check_prop(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a prop name (a JS identifier; `-` allowed after the first character)"
        ))
    }
}

impl CodeMap {
    /// Read `<project root>/figmog.map.json`: `Ok(None)` when there is
    /// none; every error names the file.
    pub(crate) fn load(project_root: &Path) -> Result<Option<CodeMap>, String> {
        let path = project_root.join(MAP_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let value: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        CodeMap::from_json(&value, &path.display().to_string()).map(Some)
    }

    /// Validate a map. `origin` prefixes every error (the file's path, or
    /// `map` for `figmog_spec`'s argument).
    pub(crate) fn from_json(value: &Value, origin: &str) -> Result<CodeMap, String> {
        CodeMap::parse(value).map_err(|e| format!("{origin}: {e}"))
    }

    fn parse(value: &Value) -> Result<CodeMap, String> {
        let root = object(value, "$", "{version, components}")?;
        only_keys(root, &["version", "components"], "$")?;
        match root.get("version") {
            None => return Err(format!("version: missing (expected {VERSION})")),
            Some(v) if v.as_u64() == Some(VERSION) => {}
            Some(v) => {
                return Err(format!(
                    "version: unsupported {v} (this figmog reads version {VERSION})"
                ));
            }
        }
        let list = root
            .get("components")
            .ok_or("components: missing")?
            .as_array()
            .ok_or("components: expected an array")?;
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        for (i, v) in list.iter().enumerate() {
            let at = format!("components[{i}]");
            let entry = Entry::parse(v, &at)?;
            if !seen.insert(entry.figma.clone()) {
                return Err(format!(
                    "{at}.figma: {} {:?} is already mapped by an earlier entry",
                    entry.figma.kind(),
                    entry.figma.key()
                ));
            }
            entries.push(entry);
        }
        Ok(CodeMap { entries })
    }

    /// The normalized map, as `figmog_spec`'s `map` argument;
    /// [`CodeMap::from_json`] reads it back to `self`.
    pub(crate) fn to_json(&self) -> Value {
        let components: Vec<Value> = self
            .entries
            .iter()
            .map(|e| {
                let props: Map<String, Value> = e
                    .props
                    .iter()
                    .map(|(name, p)| {
                        let mut m = Map::new();
                        m.insert("prop".into(), json!(p.prop));
                        if let Some(values) = &p.values {
                            m.insert("values".into(), json!(values));
                        }
                        (name.clone(), Value::Object(m))
                    })
                    .collect();
                json!({
                    "figma": e.figma.to_json(),
                    "code": {"path": e.path, "export": e.export},
                    "props": props,
                })
            })
            .collect();
        json!({"version": VERSION, "components": components})
    }

    fn get(&self, target: &Target) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.figma == target)
    }

    /// The entry for a component with key `key` in the set with key
    /// `set_key`: the component's own entry, else its set's.
    fn lookup(&self, key: Option<&str>, set_key: Option<&str>) -> Option<&Entry> {
        key.filter(|k| !k.is_empty())
            .and_then(|k| self.get(&Target::Component(k.to_string())))
            .or_else(|| {
                set_key
                    .filter(|k| !k.is_empty())
                    .and_then(|k| self.get(&Target::ComponentSet(k.to_string())))
            })
    }
}

impl Entry {
    fn parse(v: &Value, at: &str) -> Result<Entry, String> {
        let obj = object(v, at, "{figma, code, props?}")?;
        only_keys(obj, &["figma", "code", "props"], at)?;

        let fat = format!("{at}.figma");
        let figma = object(
            obj.get("figma").ok_or_else(|| format!("{fat}: missing"))?,
            &fat,
            "{component: <key>} or {component_set: <key>}",
        )?;
        only_keys(figma, &["component", "component_set"], &fat)?;
        let figma = match (figma.get("component"), figma.get("component_set")) {
            (Some(k), None) => {
                Target::Component(string(Some(k), &format!("{fat}.component"))?.into())
            }
            (None, Some(k)) => {
                Target::ComponentSet(string(Some(k), &format!("{fat}.component_set"))?.into())
            }
            _ => {
                return Err(format!(
                    "{fat}: expected exactly one of `component` or `component_set` (a Figma key)"
                ));
            }
        };

        let cat = format!("{at}.code");
        let code = object(
            obj.get("code").ok_or_else(|| format!("{cat}: missing"))?,
            &cat,
            "{path, export}",
        )?;
        only_keys(code, &["path", "export"], &cat)?;
        let path = string(code.get("path"), &format!("{cat}.path"))?;
        crate::project::check_project_path(path).map_err(|e| format!("{cat}.path: {e}"))?;
        let export = string(code.get("export"), &format!("{cat}.export"))?;
        check_export(export).map_err(|e| format!("{cat}.export: {e}"))?;

        let mut props = BTreeMap::new();
        let mut used: BTreeMap<String, String> = BTreeMap::new();
        if let Some(p) = obj.get("props") {
            let pat = format!("{at}.props");
            for (name, v) in object(p, &pat, "{\"<Figma property>\": {prop, values?}}")? {
                let at = format!("{pat}[{name:?}]");
                if name.is_empty() || name.chars().any(char::is_control) {
                    return Err(format!("{at}: not a Figma property name"));
                }
                let pm = PropMap::parse(v, &at)?;
                if let Some(other) = used.insert(pm.prop.clone(), name.clone()) {
                    return Err(format!(
                        "{at}.prop: {:?} is already the prop of {other:?}",
                        pm.prop
                    ));
                }
                props.insert(name.clone(), pm);
            }
        }
        Ok(Entry {
            figma,
            path: path.to_string(),
            export: export.to_string(),
            props,
        })
    }
}

impl PropMap {
    fn parse(v: &Value, at: &str) -> Result<PropMap, String> {
        let obj = object(v, at, "{prop, values?}")?;
        only_keys(obj, &["prop", "values"], at)?;
        let prop = string(obj.get("prop"), &format!("{at}.prop"))?;
        check_prop(prop).map_err(|e| format!("{at}.prop: {e}"))?;
        let values = match obj.get("values") {
            None => None,
            Some(v) => {
                let vat = format!("{at}.values");
                let mut out = BTreeMap::new();
                for (k, x) in object(v, &vat, "{\"<Figma value>\": <code value>}")? {
                    if x.is_array() || x.is_object() {
                        return Err(format!(
                            "{vat}[{k:?}]: expected a string, number, boolean or null"
                        ));
                    }
                    out.insert(k.clone(), x.clone());
                }
                Some(out)
            }
        };
        Ok(PropMap {
            prop: prop.to_string(),
            values,
        })
    }
}

/// The map key naming Figma property `name` among `names` (every
/// property of the instance or the component): the exact name, else —
/// when exactly one of `names` strips to it — `name` without its `#…`
/// suffix. `Err` names the ambiguity.
fn map_key<'a>(
    props: &'a BTreeMap<String, PropMap>,
    name: &str,
    names: &BTreeSet<&str>,
) -> Result<Option<(&'a String, &'a PropMap)>, String> {
    if let Some(found) = props.get_key_value(name) {
        return Ok(Some(found));
    }
    let Some((base, _)) = name.rsplit_once('#') else {
        return Ok(None);
    };
    let Some(found) = props.get_key_value(base) else {
        return Ok(None);
    };
    let alike = names
        .iter()
        .filter(|n| n.rsplit_once('#').is_some_and(|(b, _)| b == base))
        .count();
    if alike > 1 {
        return Err(format!(
            "map key {base:?} matches several properties: use the full name {name:?}"
        ));
    }
    Ok(Some(found))
}

/// A string as a JSX attribute value: `"…"` when that is exact, else a
/// JS string expression.
fn jsx_string(s: &str) -> String {
    if s.chars()
        .any(|c| matches!(c, '"' | '\\' | '&' | '{' | '}' | '<' | '>') || c.is_control())
    {
        format!("{{{}}}", Value::String(s.to_string()))
    } else {
        format!("\"{s}\"")
    }
}

/// `<Export a="b" c={1} d />`, props in name order.
fn jsx(export: &str, props: &Map<String, Value>) -> String {
    let mut out = format!("<{export}");
    for (name, v) in props {
        out.push(' ');
        match v {
            Value::Bool(true) => out.push_str(name),
            Value::String(s) => out.push_str(&format!("{name}={}", jsx_string(s))),
            Value::Object(o) => out.push_str(&format!(
                "{name}={{{}}}",
                o.get("jsx").and_then(Value::as_str).unwrap_or_default()
            )),
            other => out.push_str(&format!("{name}={{{other}}}")),
        }
    }
    out.push_str(" />");
    out
}

/// Every node of `n`'s subtree (itself included) by id.
fn index<'a>(n: &'a Value, out: &mut BTreeMap<&'a str, &'a Value>) {
    if let Some(id) = n["id"].as_str() {
        out.insert(id, n);
    }
    for child in n["children"].as_array().into_iter().flatten() {
        index(child, out);
    }
}

/// The `componentPropertyReferences` key that would explain an override
/// of `field`.
fn reference_key(field: &str) -> &str {
    match field {
        "componentId" | "mainComponent" => "mainComponent",
        other => other,
    }
}

/// `true` when instance property `p` (or the instance's
/// `boundVariables.componentProperties`) binds it to a variable.
fn is_bound(instance: &Value, name: &str, p: &Value) -> bool {
    let non_empty = |v: Option<&Value>| v.is_some_and(|v| !v.is_null());
    non_empty(p.get("boundVariables").and_then(|b| b.get("value")))
        || non_empty(
            instance
                .get("boundVariables")
                .and_then(|b| b.get("componentProperties"))
                .and_then(|b| b.get(name)),
        )
}

/// `unmapped_overrides` reasons: an `overrides` entry no mapped property
/// explains.
const OVERRIDE: &str = "override";
/// A nested instance whose component differs from the main component's
/// at that place, and no mapped instance-swap property explains it.
const SWAP: &str = "swap";
/// A nested instance whose component can't be checked against the main
/// component (not in the mirror, or no node at that place in it).
const UNVERIFIABLE_SWAP: &str = "unverifiable_swap";
/// A `SLOT` node with children: slot content, detected on an assumed REST
/// shape not yet verified against real Figma files.
const UNVERIFIED_SLOT: &str = "unverified_slot_shape";

/// The id of the main component's node that the instance-child `id`
/// stands for: Figma names a child of instance `I` after the main
/// component's node it copies, `I<I>;<node>` (`I1:2;10:5` is node `10:5`;
/// `I1:2;10:7;30:1` is node `I10:7;30:1`, a child of nested instance
/// `10:7` in the main component).
fn main_id(instance_id: &str, id: &str) -> Option<String> {
    let rest = id
        .strip_prefix('I')?
        .strip_prefix(instance_id)?
        .strip_prefix(';')?;
    Some(if rest.contains(';') {
        format!("I{rest}")
    } else {
        rest.to_string()
    })
}

/// Check every nested instance under `n` (exclusive) against `main` (the
/// main component's nested instances: node id → component id, or `null`
/// when the main component isn't in the mirror): a different component is
/// a `swap` unless `explained` (a mapped instance-swap property drives
/// it); one that can't be checked is an `unverifiable_swap`. Only an
/// instance that matches is descended into: a swapped one's children no
/// longer follow the main component.
fn swaps(
    n: &Value,
    own_id: &str,
    main: &Value,
    explained: &dyn Fn(&Value) -> bool,
    out: &mut BTreeMap<(String, String), (Value, &'static str)>,
) {
    for child in n["children"].as_array().into_iter().flatten() {
        if child["type"] != "INSTANCE" {
            swaps(child, own_id, main, explained, out);
            continue;
        }
        let id = child["id"].as_str().unwrap_or_default();
        let expected = main_id(own_id, id).and_then(|m| main.get(&m)?.as_str());
        let reason = match expected {
            Some(c) if child["componentId"].as_str() == Some(c) => {
                swaps(child, own_id, main, explained, out);
                continue;
            }
            // Swapped by a mapped property: its content is the swapped-in
            // component's, not the main component's.
            _ if explained(child) => continue,
            Some(_) => SWAP,
            None => UNVERIFIABLE_SWAP,
        };
        out.entry((id.to_string(), "componentId".to_string()))
            .or_insert((child["name"].clone(), reason));
    }
}

/// The code component an INSTANCE maps to, or `null` when the map has no
/// entry for its component (or its set). `component` is its component
/// record (`{name, key, component_set: {key} | null, main}`, `main` the
/// main component's nested instances, node id → component id, or `null`
/// when the main component isn't in the mirror), `records` every
/// component record by node id (for instance swaps).
///
/// `{figma, path, export, props, jsx, coverage, unmapped_properties,
/// unmapped_overrides, unknown_props}`: `coverage` is `"full"` only when
/// both `unmapped_*` lists are empty. `unknown_props` (map keys naming no
/// property of this instance: a stale entry) is a warning only.
pub(crate) fn code_component(
    map: Option<&CodeMap>,
    instance: &Value,
    component: &Value,
    records: &Value,
) -> Value {
    let Some(map) = map else {
        return Value::Null;
    };
    let Some(entry) = map.lookup(
        component["key"].as_str(),
        component["component_set"]["key"].as_str(),
    ) else {
        return Value::Null;
    };
    let empty = Map::new();
    let properties = instance["componentProperties"]
        .as_object()
        .unwrap_or(&empty);
    let names: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
    let mut props = Map::new();
    let mut used = BTreeSet::new();
    let mut mapped = BTreeSet::new();
    let mut unmapped_properties = Vec::new();
    for (name, p) in properties {
        let unmapped = |reason: String| json!({"name": name, "type": p["type"], "value": p["value"], "reason": reason});
        let (key, pm) = match map_key(&entry.props, name, &names) {
            Ok(Some(found)) => found,
            // A `component` entry names one variant: its identity already
            // fixes every variant property.
            Ok(None) if p["type"] == "VARIANT" && matches!(entry.figma, Target::Component(_)) => {
                mapped.insert(name.as_str());
                continue;
            }
            Ok(None) => {
                unmapped_properties.push(unmapped("no prop for it in the map".into()));
                continue;
            }
            Err(e) => {
                unmapped_properties.push(unmapped(e));
                continue;
            }
        };
        used.insert(key.as_str());
        if is_bound(instance, name, p) {
            unmapped_properties.push(unmapped(
                "bound to a variable: the map would pass a literal".into(),
            ));
            continue;
        }
        let swap = p["type"] == "INSTANCE_SWAP";
        let swapped = swap.then(|| &records[p["value"].as_str().unwrap_or_default()]);
        // The value `values` is keyed by: the text, or the swapped-in
        // component's key.
        let text = match (swapped, &p["value"]) {
            (Some(rec), _) => rec["key"].as_str().map(str::to_string),
            (None, Value::String(s)) => Some(s.clone()),
            (None, Value::Bool(b)) => Some(b.to_string()),
            (None, _) => None,
        };
        let value = match (&pm.values, swapped) {
            (Some(values), _) => match text.as_deref().and_then(|t| values.get(t)) {
                Some(v) => v.clone(),
                None => {
                    unmapped_properties.push(unmapped(format!(
                        "value {} is not listed in the map's values",
                        text.map_or_else(|| p["value"].to_string(), |t| format!("{t:?}"))
                    )));
                    continue;
                }
            },
            (None, Some(rec)) => {
                let target = map.lookup(rec["key"].as_str(), rec["component_set"]["key"].as_str());
                match target {
                    Some(t) if matches!(t.figma, Target::ComponentSet(_)) => {
                        // Component records carry the variant's name and
                        // identity, but no authoritative variant properties.
                        // A set mapping alone cannot select its code variant.
                        unmapped_properties.push(unmapped(
                            "the swapped-in component maps to a component set, but its variant properties are unavailable".into(),
                        ));
                        continue;
                    }
                    Some(t) => json!({
                        "jsx": format!("<{} />", t.export),
                        "path": t.path,
                        "export": t.export,
                    }),
                    None => {
                        let what = rec["name"]
                            .as_str()
                            .map_or_else(|| format!("{}", p["value"]), |n| format!("{n:?}"));
                        unmapped_properties.push(unmapped(format!(
                            "the swapped-in component {what} is not mapped"
                        )));
                        continue;
                    }
                }
            }
            (None, None) => match &p["value"] {
                v @ (Value::String(_) | Value::Bool(_)) => v.clone(),
                _ => {
                    unmapped_properties.push(unmapped("value is not a string or boolean".into()));
                    continue;
                }
            },
        };
        mapped.insert(name.as_str());
        if !value.is_null() {
            props.insert(pm.prop.clone(), value);
        }
    }

    // Overrides on the instance and on every nested instance, swapped
    // nested instances and slot content: explained only by a mapped
    // property's reference.
    let mut nodes = BTreeMap::new();
    index(instance, &mut nodes);
    let own_id = instance["id"].as_str().unwrap_or_default();
    let explained_by = |node: &Value, field: &str| {
        node["componentPropertyReferences"][reference_key(field)]
            .as_str()
            .is_some_and(|p| mapped.contains(p))
    };
    let mut unmapped_overrides = BTreeMap::new();
    for (id, node) in &nodes {
        for o in node["overrides"].as_array().into_iter().flatten() {
            let target = o["id"].as_str().unwrap_or_default();
            for field in o["overriddenFields"].as_array().into_iter().flatten() {
                let field = field.as_str().unwrap_or_default();
                // The instance's own properties are checked above.
                if *id == own_id && target == own_id && field == "componentProperties" {
                    continue;
                }
                let explained =
                    *id == own_id && nodes.get(target).is_some_and(|t| explained_by(t, field));
                if !explained {
                    let name = nodes.get(target).map_or(Value::Null, |t| t["name"].clone());
                    unmapped_overrides
                        .entry((target.to_string(), field.to_string()))
                        .or_insert((name, OVERRIDE));
                }
            }
        }
    }
    swaps(
        instance,
        own_id,
        &component["main"],
        &|n: &Value| explained_by(n, "mainComponent"),
        &mut unmapped_overrides,
    );
    for (id, node) in &nodes {
        if *id != own_id
            && node["type"] == "SLOT"
            && node["children"].as_array().is_some_and(|c| !c.is_empty())
        {
            unmapped_overrides
                .entry((id.to_string(), "slot".to_string()))
                .or_insert((node["name"].clone(), UNVERIFIED_SLOT));
        }
    }
    let unmapped_overrides: Vec<Value> = unmapped_overrides
        .into_iter()
        .map(|((id, field), (name, reason))| {
            json!({"id": id, "name": name, "field": field, "reason": reason})
        })
        .collect();
    let unknown_props: Vec<&String> = entry
        .props
        .keys()
        .filter(|k| !used.contains(k.as_str()))
        .collect();
    let full = unmapped_properties.is_empty() && unmapped_overrides.is_empty();
    json!({
        "figma": entry.figma.to_json(),
        "path": entry.path,
        "export": entry.export,
        "jsx": jsx(&entry.export, &props),
        "props": props,
        "coverage": if full { "full" } else { "partial" },
        "unmapped_properties": unmapped_properties,
        "unmapped_overrides": unmapped_overrides,
        "unknown_props": unknown_props,
    })
}

/// Whether a mapped code file is there: `"present"`, `"missing"`, or
/// `"outside_project"` (a symlink leading out of the project root).
fn code_file(project_root: &Path, path: &str) -> &'static str {
    let full = project_root.join(path);
    if !full.is_file() {
        return "missing";
    }
    match (project_root.canonicalize(), full.canonicalize()) {
        (Ok(root), Ok(file)) if file.starts_with(&root) => "present",
        (Ok(_), Ok(_)) => "outside_project",
        _ => "missing",
    }
}

/// `figmog map coverage`: the map against the mirror's design-system
/// inventory (`inventory` is the `figmog_components` answer) and the
/// project's files.
///
/// - `mapped`: each entry whose target is in the mirror, with its code
///   file's state and `unknown_props` (map keys no property definition of
///   the target names; `null` when the mirror records no definitions to
///   check against — a standalone component, or a set whose node isn't
///   in the mirror);
/// - `unmapped`: each component set (unless it or every variant is
///   mapped) and standalone component with no entry;
/// - `not_in_mirror`: entries whose target this mirror doesn't have (a
///   removed component, or one from another file).
pub(crate) fn coverage(map: Option<&CodeMap>, inventory: &Value, project_root: &Path) -> Value {
    let present = map.is_some();
    let empty = CodeMap::default();
    let map = map.unwrap_or(&empty);
    let text = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();

    // Every target in the mirror: key → (name, node ids, definitions).
    struct Found {
        name: String,
        node_ids: BTreeSet<String>,
        definitions: Option<Value>,
    }
    let mut targets: BTreeMap<Target, Found> = BTreeMap::new();
    let mut add = |t: Target, name: String, id: String, defs: Option<Value>| {
        let f = targets.entry(t).or_insert(Found {
            name,
            node_ids: BTreeSet::new(),
            definitions: defs,
        });
        f.node_ids.insert(id);
    };
    let sets = inventory["sets"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let standalone = inventory["components"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for s in sets {
        let defs = s["property_definitions"]
            .as_object()
            .map(|_| s["property_definitions"].clone());
        add(
            Target::ComponentSet(text(s, "key")),
            text(s, "name"),
            text(s, "node_id"),
            defs.clone(),
        );
        for v in s["variants"].as_array().into_iter().flatten() {
            add(
                Target::Component(text(v, "key")),
                text(v, "name"),
                text(v, "node_id"),
                defs.clone(),
            );
        }
    }
    for c in standalone {
        add(
            Target::Component(text(c, "key")),
            text(c, "name"),
            text(c, "node_id"),
            None,
        );
    }

    let mut mapped = Vec::new();
    let mut not_in_mirror = Vec::new();
    let mut missing_code = 0;
    let mut entries: Vec<&Entry> = map.entries.iter().collect();
    entries.sort_by(|a, b| a.figma.cmp(&b.figma));
    for e in entries {
        let code = json!({"path": e.path, "export": e.export});
        let Some(found) = targets.get(&e.figma) else {
            not_in_mirror.push(json!({"figma": e.figma.to_json(), "code": code}));
            continue;
        };
        let file = code_file(project_root, &e.path);
        if file != "present" {
            missing_code += 1;
        }
        let unknown_props = found.definitions.as_ref().map(|defs| {
            let names: BTreeSet<&str> = defs
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k.as_str())
                .collect();
            let known: BTreeSet<&String> = names
                .iter()
                .filter_map(|n| map_key(&e.props, n, &names).ok().flatten().map(|(k, _)| k))
                .collect();
            e.props
                .keys()
                .filter(|k| !known.contains(k))
                .cloned()
                .collect::<Vec<_>>()
        });
        mapped.push(json!({
            "figma": {
                "kind": e.figma.kind(),
                "key": e.figma.key(),
                "name": found.name,
                "node_ids": found.node_ids,
            },
            "code": code,
            "code_file": file,
            "unknown_props": unknown_props,
        }));
    }

    let has = |t: Target| map.get(&t).is_some();
    let mut unmapped = Vec::new();
    for s in sets {
        let variants = s["variants"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let variants_mapped = variants
            .iter()
            .filter(|v| has(Target::Component(text(v, "key"))))
            .count();
        if has(Target::ComponentSet(text(s, "key")))
            || (!variants.is_empty() && variants_mapped == variants.len())
        {
            continue;
        }
        unmapped.push(json!({
            "kind": "component_set",
            "key": text(s, "key"),
            "name": text(s, "name"),
            "node_id": text(s, "node_id"),
            "variants": variants.len(),
            "variants_mapped": variants_mapped,
        }));
    }
    for c in standalone {
        if !has(Target::Component(text(c, "key"))) {
            unmapped.push(json!({
                "kind": "component",
                "key": text(c, "key"),
                "name": text(c, "name"),
                "node_id": text(c, "node_id"),
            }));
        }
    }
    unmapped.sort_by(|a, b| {
        let k = |v: &Value| (text(v, "name"), text(v, "key"), text(v, "node_id"));
        k(a).cmp(&k(b))
    });

    json!({
        "map": {"path": MAP_FILE, "present": present},
        "summary": {
            "mapped": mapped.len(),
            "unmapped": unmapped.len(),
            "not_in_mirror": not_in_mirror.len(),
            "code_files_not_present": missing_code,
        },
        "mapped": mapped,
        "unmapped": unmapped,
        "not_in_mirror": not_in_mirror,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Result<CodeMap, String> {
        CodeMap::from_json(&v, MAP_FILE)
    }

    fn pricing() -> Value {
        json!({
            "version": 1,
            "components": [{
                "figma": {"component_set": "set-key"},
                "code": {"path": "src/PricingCard.tsx", "export": "PricingCard"},
                "props": {
                    "Tier": {"prop": "tier", "values": {"Free": "free", "Pro": "pro"}},
                    "Title": {"prop": "title"},
                    "Badge": {"prop": "badge"}
                }
            }]
        })
    }

    #[test]
    fn round_trips_through_its_normalized_json() {
        let map = parse(pricing()).unwrap();
        assert_eq!(parse(map.to_json()).unwrap(), map);
    }

    #[test]
    fn errors_name_the_file_and_the_json_path() {
        let cases = [
            (json!([]), "figmog.map.json: $: expected an object"),
            (
                json!({"components": []}),
                "figmog.map.json: version: missing",
            ),
            (
                json!({"version": 2, "components": []}),
                "figmog.map.json: version: unsupported 2",
            ),
            (
                json!({"version": 1, "components": [], "extra": 1}),
                "figmog.map.json: $: unknown key \"extra\"",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k", "component_set": "s"}, "code": {"path": "a.tsx", "export": "A"}}]}),
                "figmog.map.json: components[0].figma: expected exactly one",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k"}, "code": {"path": "../a.tsx", "export": "A"}}]}),
                "figmog.map.json: components[0].code.path: \"../a.tsx\" must be a relative path inside the project",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k"}, "code": {"path": "/abs/a.tsx", "export": "A"}}]}),
                "components[0].code.path: \"/abs/a.tsx\" must be a relative path",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k"}, "code": {"path": "a.tsx", "export": "button"}}]}),
                "components[0].code.export: \"button\" is not a JSX component name",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k"}, "code": {"path": "a.tsx", "export": "A"}, "props": {"Tier": {"prop": "1tier"}}}]}),
                "components[0].props[\"Tier\"].prop: \"1tier\" is not a prop name",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k"}, "code": {"path": "a.tsx", "export": "A"}, "props": {"Tier": {"prop": "tier", "values": {"Pro": ["x"]}}}}]}),
                "components[0].props[\"Tier\"].values[\"Pro\"]: expected a string, number, boolean or null",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k"}, "code": {"path": "a.tsx", "export": "A"}, "props": {"A": {"prop": "x"}, "B": {"prop": "x"}}}]}),
                "components[0].props[\"B\"].prop: \"x\" is already the prop of \"A\"",
            ),
            (
                json!({"version": 1, "components": [{"figma": {"component": "k"}, "code": {"path": "a.tsx", "export": "A"}, "props": {"A": {"prop": "x", "default": 1}}}]}),
                "components[0].props[\"A\"]: unknown key \"default\"",
            ),
            (
                json!({"version": 1, "components": [
                    {"figma": {"component": "k"}, "code": {"path": "a.tsx", "export": "A"}},
                    {"figma": {"component": "k"}, "code": {"path": "b.tsx", "export": "B"}}
                ]}),
                "components[1].figma: component \"k\" is already mapped",
            ),
        ];
        for (input, want) in cases {
            let err = parse(input.clone()).unwrap_err();
            assert!(err.contains(want), "{input}: {err}");
        }
    }

    fn records() -> Value {
        json!({
            "10:1": {"name": "Tier=Pro", "key": "pro-key", "component_set": {"key": "set-key"},
                     "main": {"3:1": "30:1", "I3:1;6:1": "31:1", "7:1": "20:1"}},
            "20:1": {"name": "Star", "key": "star-key", "component_set": null},
            "20:2": {"name": "Heart", "key": "heart-key", "component_set": null},
        })
    }

    fn instance(props: Value, overrides: Value) -> Value {
        json!({
            "id": "1:5", "name": "Card", "type": "INSTANCE", "componentId": "10:1",
            "componentProperties": props,
            "overrides": overrides,
            "children": [
                {"id": "I1:5;2:1", "name": "Title", "type": "TEXT",
                 "componentPropertyReferences": {"characters": "Title#1:0"}},
                {"id": "I1:5;2:2", "name": "Price", "type": "TEXT"},
            ]
        })
    }

    #[test]
    fn a_fully_mapped_instance_renders_as_jsx() {
        let map = parse(pricing()).unwrap();
        let n = instance(
            json!({
                "Tier": {"type": "VARIANT", "value": "Pro"},
                "Title#1:0": {"type": "TEXT", "value": "Team \"plus\""},
                "Badge#1:1": {"type": "BOOLEAN", "value": true},
            }),
            json!([{"id": "I1:5;2:1", "overriddenFields": ["characters"]}]),
        );
        let recs = records();
        let out = code_component(Some(&map), &n, &recs["10:1"], &recs);
        assert_eq!(out["coverage"], "full", "{out}");
        assert_eq!(
            out["jsx"],
            r#"<PricingCard badge tier="pro" title={"Team \"plus\""} />"#
        );
        assert_eq!(out["path"], "src/PricingCard.tsx");
        assert_eq!(out["unknown_props"], json!([]));
    }

    #[test]
    fn what_the_map_cannot_express_makes_it_partial() {
        let mut m = pricing();
        m["components"][0]["props"]["Icon"] = json!({"prop": "icon"});
        m["components"]
            .as_array_mut()
            .unwrap()
            .push(json!({"figma": {"component": "star-key"}, "code": {"path": "src/Star.tsx", "export": "StarIcon"}}));
        let map = parse(m).unwrap();
        let n = instance(
            json!({
                "Tier": {"type": "VARIANT", "value": "Enterprise"},
                "Size": {"type": "VARIANT", "value": "Large"},
                "Icon#1:2": {"type": "INSTANCE_SWAP", "value": "20:2"},
            }),
            json!([{"id": "I1:5;2:2", "overriddenFields": ["characters", "fills"]}]),
        );
        let recs = records();
        let out = code_component(Some(&map), &n, &recs["10:1"], &recs);
        assert_eq!(out["coverage"], "partial");
        let reasons: Vec<(&str, &str)> = out["unmapped_properties"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| (u["name"].as_str().unwrap(), u["reason"].as_str().unwrap()))
            .collect();
        assert_eq!(
            reasons,
            [
                (
                    "Icon#1:2",
                    "the swapped-in component \"Heart\" is not mapped"
                ),
                ("Size", "no prop for it in the map"),
                (
                    "Tier",
                    "value \"Enterprise\" is not listed in the map's values"
                ),
            ]
        );
        assert_eq!(
            out["unmapped_overrides"],
            json!([
                {"id": "I1:5;2:2", "name": "Price", "field": "characters", "reason": "override"},
                {"id": "I1:5;2:2", "name": "Price", "field": "fills", "reason": "override"},
            ])
        );
        assert_eq!(out["unknown_props"], json!(["Badge", "Title"]));

        // A swap to a mapped component renders as its element.
        let n = instance(
            json!({"Tier": {"type": "VARIANT", "value": "Free"}, "Icon#1:2": {"type": "INSTANCE_SWAP", "value": "20:1"}}),
            json!([]),
        );
        let out = code_component(Some(&map), &n, &recs["10:1"], &recs);
        assert_eq!(out["coverage"], "full", "{out}");
        assert_eq!(
            out["jsx"],
            "<PricingCard icon={<StarIcon />} tier=\"free\" />"
        );
    }

    #[test]
    fn set_based_instance_swaps_are_partial_without_authoritative_variant_properties() {
        let mut m = pricing();
        m["components"][0]["props"]["Icon"] = json!({"prop": "icon"});
        m["components"].as_array_mut().unwrap().push(json!({
            "figma": {"component_set": "icons-key"},
            "code": {"path": "src/Icon.tsx", "export": "Icon"},
            "props": {"Size": {"prop": "size", "values": {"Large": "lg"}}}
        }));
        let mut recs = records();
        recs["20:1"] = json!({"name": "Size=Large", "key": "large-key", "component_set": {"key": "icons-key"}});
        let n = instance(
            json!({"Tier": {"type": "VARIANT", "value": "Pro"},
                   "Icon#1:2": {"type": "INSTANCE_SWAP", "value": "20:1"}}),
            json!([]),
        );
        let map = parse(m.clone()).unwrap();
        let out = code_component(Some(&map), &n, &recs["10:1"], &recs);
        assert_eq!(out["coverage"], "partial", "{out}");
        assert_eq!(out["unmapped_properties"][0]["name"], "Icon#1:2");
        assert!(
            out["unmapped_properties"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("variant properties are unavailable")
        );
        assert!(out["props"].get("icon").is_none());

        // Explicit values still select the variant without needing its properties.
        m["components"][0]["props"]["Icon"]["values"] = json!({"large-key": "large"});
        let map = parse(m).unwrap();
        let out = code_component(Some(&map), &n, &recs["10:1"], &recs);
        assert_eq!(out["coverage"], "full", "{out}");
        assert_eq!(out["props"]["icon"], "large");
    }

    #[test]
    fn nested_instance_overrides_and_slots_are_reported() {
        let map = parse(pricing()).unwrap();
        let mut n = instance(
            json!({"Tier": {"type": "VARIANT", "value": "Pro"}}),
            json!([]),
        );
        n["children"].as_array_mut().unwrap().extend([
            json!({"id": "I1:5;3:1", "name": "Button", "type": "INSTANCE", "componentId": "30:1",
            "overrides": [{"id": "I1:5;3:1;4:1", "overriddenFields": ["characters"]}],
            "children": [
                {"id": "I1:5;3:1;4:1", "name": "Label", "type": "TEXT"},
                // As in the main component (`I3:1;6:1`): not listed.
                {"id": "I1:5;3:1;6:1", "name": "Glyph", "type": "INSTANCE", "componentId": "31:1"},
            ]}),
            json!({"id": "I1:5;5:1", "name": "Body", "type": "SLOT",
                   "children": [{"id": "9:9", "name": "Extra", "type": "FRAME"}]}),
        ]);
        let recs = records();
        let out = code_component(Some(&map), &n, &recs["10:1"], &recs);
        assert_eq!(out["coverage"], "partial");
        assert_eq!(
            out["unmapped_overrides"],
            json!([
                {"id": "I1:5;3:1;4:1", "name": "Label", "field": "characters", "reason": "override"},
                {"id": "I1:5;5:1", "name": "Body", "field": "slot", "reason": "unverified_slot_shape"},
            ])
        );
    }

    /// Nested instances are checked against the main component: a hand
    /// swap is listed even when `overrides` doesn't name it, one driven by
    /// a mapped instance-swap property is not, and without the main
    /// component in the mirror nothing can be verified.
    #[test]
    fn nested_instances_are_checked_against_the_main_component() {
        let mut m = pricing();
        m["components"][0]["props"]["Icon"] = json!({"prop": "icon"});
        m["components"].as_array_mut().unwrap().push(json!({
            "figma": {"component": "star-key"},
            "code": {"path": "src/Star.tsx", "export": "StarIcon"}
        }));
        let map = parse(m).unwrap();
        let card = |icon: &str, refs: Value| {
            let mut n = instance(
                json!({"Tier": {"type": "VARIANT", "value": "Pro"},
                       "Icon#1:2": {"type": "INSTANCE_SWAP", "value": "20:1"}}),
                json!([]),
            );
            n["children"].as_array_mut().unwrap().push(json!({
                "id": "I1:5;7:1", "name": "Icon", "type": "INSTANCE",
                "componentId": icon, "componentPropertyReferences": refs,
            }));
            n
        };
        let recs = records();
        let run = |n: &Value, rec: &Value| code_component(Some(&map), n, rec, &recs);
        // As in the main component.
        let out = run(&card("20:1", json!({})), &recs["10:1"]);
        assert_eq!(out["coverage"], "full", "{out}");
        // Swapped by hand.
        let out = run(&card("20:2", json!({})), &recs["10:1"]);
        assert_eq!(
            out["unmapped_overrides"],
            json!([{"id": "I1:5;7:1", "name": "Icon", "field": "componentId", "reason": "swap"}])
        );
        // Swapped by the mapped `Icon` property.
        let out = run(
            &card("20:2", json!({"mainComponent": "Icon#1:2"})),
            &recs["10:1"],
        );
        assert_eq!(out["coverage"], "full", "{out}");
        // The main component isn't in the mirror (a library component).
        let mut library = recs["10:1"].clone();
        library["main"] = Value::Null;
        let out = run(&card("20:1", json!({})), &library);
        assert_eq!(
            out["unmapped_overrides"],
            json!([{"id": "I1:5;7:1", "name": "Icon", "field": "componentId", "reason": "unverifiable_swap"}])
        );
        assert_eq!(out["coverage"], "partial");
    }

    /// A `component` entry names one variant: its variant properties are
    /// expressed by identity, the other properties still need props.
    #[test]
    fn a_variant_entry_expresses_its_variant_properties() {
        let map = parse(json!({"version": 1, "components": [{
            "figma": {"component": "pro-key"},
            "code": {"path": "src/ProCard.tsx", "export": "ProCard"},
            "props": {"Title": {"prop": "title"}}
        }]}))
        .unwrap();
        let recs = records();
        let n = instance(
            json!({"Tier": {"type": "VARIANT", "value": "Pro"},
                   "Title#1:0": {"type": "TEXT", "value": "Pro"}}),
            json!([]),
        );
        let out = code_component(Some(&map), &n, &recs["10:1"], &recs);
        assert_eq!(out["coverage"], "full", "{out}");
        assert_eq!(out["jsx"], "<ProCard title=\"Pro\" />");
        // A set entry still has to map the variant.
        let set = parse(json!({"version": 1, "components": [{
            "figma": {"component_set": "set-key"},
            "code": {"path": "src/Card.tsx", "export": "Card"},
            "props": {"Title": {"prop": "title"}}
        }]}))
        .unwrap();
        let out = code_component(Some(&set), &n, &recs["10:1"], &recs);
        assert_eq!(out["unmapped_properties"][0]["name"], "Tier");
    }

    #[test]
    fn unmapped_components_and_no_map_answer_null() {
        let map = parse(pricing()).unwrap();
        let recs = records();
        let n = json!({"id": "1:9", "type": "INSTANCE", "componentId": "20:1"});
        assert_eq!(
            code_component(Some(&map), &n, &recs["20:1"], &recs),
            Value::Null
        );
        assert_eq!(code_component(None, &n, &recs["10:1"], &recs), Value::Null);
    }

    #[test]
    fn suffix_matching_refuses_an_ambiguous_name() {
        let props: BTreeMap<String, PropMap> = [(
            "Label".to_string(),
            PropMap {
                prop: "label".into(),
                values: None,
            },
        )]
        .into();
        let names: BTreeSet<&str> = ["Label#1:0", "Label#2:0"].into();
        assert!(map_key(&props, "Label#1:0", &names).is_err());
        let names: BTreeSet<&str> = ["Label#1:0"].into();
        assert!(map_key(&props, "Label#1:0", &names).unwrap().is_some());
    }
}
