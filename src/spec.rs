//! `figmog spec <node>` / `figmog_spec`: a code-ready description of one
//! subtree (NWS-2968's slice, grown by NWS-2947). Per node, two layers:
//!
//! - **design** — what Figma recorded: the layout properties (auto layout,
//!   grid, sizing, constraints, min/max, clipping, text auto-resize)
//!   copied verbatim;
//! - **figmog's interpretation** — `layout` and `values` (every value
//!   shown as the variable token it is bound to, or flagged untokenized,
//!   naming any token whose root value it equals), `content` (text, mixed
//!   runs, assets), `instance` (component, variant properties) and `css`
//!   (the declarations figmog derives), plus an optional Tailwind class
//!   hint.
//!
//! Whatever figmog can't translate exactly is listed under `unsupported`
//! with the node id and reason and left out of `css`, never approximated
//! (SPEC §17).
//!
//! Pure: the inputs are the `figmog_subtree` answer for the node (raw
//! JSON, children nested), the `figmog_vars {snapshot: true}` answer, and
//! the component records the subtree's instances name, so the output is a
//! function of the mirror alone. A bound token's `css` name comes from
//! [`crate::tokens::export_css`] run on the same snapshot, so it is the
//! name `figmog tokens export` emits, never a parallel naming rule.
//!
//! Determinism: every object is a `serde_json::Map` (sorted keys, no
//! `preserve_order`), `unsupported` is a sorted set, children and runs
//! keep their order, and `matches` lists are sorted by token name then
//! variable id.

mod css;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::map::CodeMap;
use crate::tokens::{ExportOptions, color, export_css};
use css::{Css, CssVal, num, px};

/// One authoritative variable row of `figmog vars --snapshot`.
struct Var {
    name: String,
    resolved_type: String,
    collection: String,
    default_mode: Option<String>,
    scopes: Vec<String>,
    /// Mode name → raw value or alias.
    values: BTreeMap<String, Value>,
}

/// A comparable value: a `#rrggbb[aa]` color, a number, or a string.
#[derive(Debug, Clone)]
enum Lit {
    Color(String),
    Number(f64),
    Text(String),
}

impl Lit {
    fn same(&self, other: &Lit) -> bool {
        match (self, other) {
            (Lit::Color(a), Lit::Color(b)) | (Lit::Text(a), Lit::Text(b)) => a == b,
            (Lit::Number(a), Lit::Number(b)) => (a - b).abs() < 1e-6,
            _ => false,
        }
    }
}

/// Everything the spec needs from the variable snapshot.
struct Tokens {
    vars: BTreeMap<String, Var>,
    /// Variable id → custom property name, from the token export.
    css: BTreeMap<String, String>,
    /// Variable id → Tailwind theme name, from the token export.
    theme: BTreeMap<String, String>,
    /// Variable id → the unit a FLOAT token is exported with (`None`:
    /// unitless), from the token export.
    units: BTreeMap<String, Option<&'static str>>,
    /// Variable id → its value in the root mode (the mode the token export
    /// writes to `:root`, else the collection's default mode), aliases
    /// followed.
    root: BTreeMap<String, Lit>,
}

/// What a property accepts: the variable type and the Figma scopes that
/// make a variable eligible for it (`ALL_SCOPES` always does).
#[derive(Clone, Copy)]
struct Prop {
    resolved_type: &'static str,
    scopes: &'static [&'static str],
}

const fn float(scopes: &'static [&'static str]) -> Prop {
    Prop {
        resolved_type: "FLOAT",
        scopes,
    }
}

const GAP: Prop = float(&["GAP"]);
const SIZE: Prop = float(&["WIDTH_HEIGHT"]);
const RADIUS: Prop = float(&["CORNER_RADIUS"]);
const OPACITY: Prop = float(&["OPACITY"]);
const STROKE_WEIGHT: Prop = float(&["STROKE_FLOAT"]);
const EFFECT_FLOAT: Prop = float(&["EFFECT_FLOAT"]);
const STROKE_COLOR: Prop = Prop {
    resolved_type: "COLOR",
    scopes: &["STROKE_COLOR"],
};
const EFFECT_COLOR: Prop = Prop {
    resolved_type: "COLOR",
    scopes: &["EFFECT_COLOR"],
};

/// Fill scope by node type: text, frame-like, or shape.
fn fill_prop(node_type: &str) -> Prop {
    let scopes: &'static [&'static str] = match node_type {
        "TEXT" => &["ALL_FILLS", "TEXT_FILL"],
        "FRAME" | "COMPONENT" | "COMPONENT_SET" | "INSTANCE" | "SECTION" => {
            &["ALL_FILLS", "FRAME_FILL"]
        }
        _ => &["ALL_FILLS", "SHAPE_FILL"],
    };
    Prop {
        resolved_type: "COLOR",
        scopes,
    }
}

/// Frame-like nodes: a `layoutMode: NONE` one places its children by
/// their constraints.
const FRAME_LIKE: [&str; 5] = ["FRAME", "COMPONENT", "COMPONENT_SET", "INSTANCE", "SECTION"];

/// Nodes CSS can't draw: each is one vector asset (its children are part
/// of it and not specified separately).
const VECTOR_LIKE: [&str; 5] = [
    "VECTOR",
    "BOOLEAN_OPERATION",
    "STAR",
    "LINE",
    "REGULAR_POLYGON",
];

/// The recorded layout properties the `design` layer copies verbatim.
const DESIGN_KEYS: [&str; 41] = [
    "absoluteBoundingBox",
    "clipsContent",
    "constraints",
    "counterAxisAlignContent",
    "counterAxisAlignItems",
    "counterAxisSizingMode",
    "counterAxisSpacing",
    "gridChildHorizontalAlign",
    "gridChildVerticalAlign",
    "gridColumnAnchorIndex",
    "gridColumnCount",
    "gridColumnGap",
    "gridColumnSpan",
    "gridColumnsSizing",
    "gridRowAnchorIndex",
    "gridRowCount",
    "gridRowGap",
    "gridRowSpan",
    "gridRowsSizing",
    "itemReverseZIndex",
    "itemSpacing",
    "layoutAlign",
    "layoutGrow",
    "layoutMode",
    "layoutPositioning",
    "layoutSizingHorizontal",
    "layoutSizingVertical",
    "layoutWrap",
    "maxHeight",
    "maxWidth",
    "minHeight",
    "minWidth",
    "paddingBottom",
    "paddingLeft",
    "paddingRight",
    "paddingTop",
    "primaryAxisAlignItems",
    "primaryAxisSizingMode",
    "rotation",
    "strokesIncludedInLayout",
    "textAutoResize",
];

/// Text-layout properties of a TEXT node's type style the `design` layer
/// copies (under `design.style`).
const DESIGN_STYLE_KEYS: [&str; 5] = [
    "maxLines",
    "textAlignHorizontal",
    "textAlignVertical",
    "textAutoResize",
    "textTruncation",
];

/// Build the spec. `tree` is the `figmog_subtree` answer for the node;
/// `vars` the `figmog_vars {snapshot: true}` answer; `components` maps a
/// component id to `{name, key, remote, component_set}` for the subtree's
/// instances ([`component_ids`]); `tokens` the token export options (the
/// project's `[tokens]` settings), so the CSS names and root modes match
/// the committed token file; `tailwind` adds the per-node class hint;
/// `map` is the project's Figma → code component map (NWS-2949), which
/// fills each mapped instance's `code_component`.
pub fn build(
    tree: &Value,
    vars: &Value,
    components: &Value,
    tokens: &ExportOptions,
    tailwind: bool,
    map: Option<&CodeMap>,
) -> Value {
    let (css, theme, theme_names, units, roots, css_unavailable) = match export_css(vars, tokens) {
        Ok(export) => (
            export.names,
            export.theme,
            export.theme_names,
            export.units,
            export.root_modes,
            Value::Null,
        ),
        Err(e) => (
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            Value::String(e),
        ),
    };
    let mut tokens = Tokens::new(vars, css, theme_names, &roots);
    tokens.units = units;
    let mut cx = Cx {
        tokens: &tokens,
        components,
        tailwind,
        map,
        unsupported: BTreeSet::new(),
        consumed: BTreeSet::new(),
        style_base: "/style".into(),
    };
    let node = cx.node(
        tree,
        &Up {
            kind: Parent::Root,
            bbox: None,
            margin: None,
            border: None,
        },
    );
    let unsupported: Vec<Value> = cx
        .unsupported
        .into_iter()
        .map(|(id, reason)| json!({"id": id, "reason": reason}))
        .collect();
    let mut snapshot = vars["snapshot"].as_object().cloned().unwrap_or_default();
    snapshot.insert("css_unavailable".into(), css_unavailable);
    snapshot.insert("theme".into(), json!(theme));
    json!({
        "node": node,
        "tokens_snapshot": snapshot,
        "unsupported": unsupported,
    })
}

/// The component ids of every INSTANCE in `tree`, and of every component
/// an instance-swap property swaps in, for the component records [`build`]
/// takes.
pub fn component_ids(tree: &Value) -> BTreeSet<String> {
    fn walk(n: &Value, out: &mut BTreeSet<String>) {
        if n["type"] == "INSTANCE" {
            if let Some(id) = n["componentId"].as_str() {
                out.insert(id.to_string());
            }
            for (_, p) in n["componentProperties"].as_object().into_iter().flatten() {
                if p["type"] == "INSTANCE_SWAP"
                    && let Some(id) = p["value"].as_str()
                {
                    out.insert(id.to_string());
                }
            }
        }
        for child in n["children"].as_array().into_iter().flatten() {
            walk(child, out);
        }
    }
    let mut out = BTreeSet::new();
    walk(tree, &mut out);
    out
}

impl Tokens {
    fn new(
        vars: &Value,
        css: BTreeMap<String, String>,
        theme: BTreeMap<String, String>,
        roots: &BTreeMap<String, String>,
    ) -> Tokens {
        let mut all = BTreeMap::new();
        for row in vars["variables"].as_array().into_iter().flatten() {
            if row["source"] != "imported" {
                continue;
            }
            let text = |k: &str| row[k].as_str().unwrap_or_default().to_string();
            all.insert(
                text("variable_id"),
                Var {
                    name: text("name"),
                    resolved_type: text("resolved_type"),
                    collection: text("collection"),
                    default_mode: row["default_mode"].as_str().map(str::to_string),
                    scopes: row["scopes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    values: row["values_by_mode"]
                        .as_object()
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default(),
                },
            );
        }
        let root = all
            .keys()
            .filter_map(|id| Some((id.clone(), root_value(&all, roots, id, 0)?)))
            .collect();
        Tokens {
            vars: all,
            css,
            theme,
            units: BTreeMap::new(),
            root,
        }
    }

    /// A bound value: the token, its CSS name, the raw value, and the
    /// JSON pointers (relative to the node) of the binding.
    fn bound(&self, bind: &Bind, raw: Value) -> Value {
        let mut at = bind.at.clone();
        at.sort();
        at.dedup();
        json!({
            "token": self.vars.get(&bind.var).map(|v| v.name.clone()),
            "variable_id": bind.var,
            "css": self.css.get(&bind.var),
            "raw": raw,
            "bound_at": at,
        })
    }

    /// An unbound value: flagged untokenized, plus every eligible token
    /// whose root value equals it.
    fn untokenized(&self, raw: Value, lit: Option<&Lit>, prop: Prop) -> Value {
        let mut out = json!({"untokenized": true, "raw": raw});
        let Some(lit) = lit else {
            return out;
        };
        let mut matches: Vec<(&str, &str)> = self
            .vars
            .iter()
            .filter(|(id, v)| {
                v.resolved_type == prop.resolved_type
                    && v.scopes
                        .iter()
                        .any(|s| s == "ALL_SCOPES" || prop.scopes.contains(&s.as_str()))
                    && self.root.get(*id).is_some_and(|r| r.same(lit))
            })
            .map(|(id, v)| (v.name.as_str(), id.as_str()))
            .collect();
        matches.sort();
        if !matches.is_empty() {
            out["matches"] = matches
                .into_iter()
                .map(
                    |(name, id)| json!({"token": name, "variable_id": id, "css": self.css.get(id)}),
                )
                .collect();
        }
        out
    }

    /// The value entry for `raw` with optional binding `variable_id`.
    fn entry(&self, binding: Option<Bind>, raw: Value, lit: Option<&Lit>, prop: Prop) -> Value {
        match binding {
            Some(b) => self.bound(&b, raw),
            None => self.untokenized(raw, lit, prop),
        }
    }
}

/// An untokenized raw `0` — a CSS default, so not declared.
fn is_unbound_zero(e: &Value) -> bool {
    e.get("variable_id").is_none() && e["raw"].as_f64() == Some(0.0)
}

/// A variable's value in its root mode, aliases followed (bounded, so a
/// cyclic snapshot can't loop).
fn root_value(
    all: &BTreeMap<String, Var>,
    roots: &BTreeMap<String, String>,
    id: &str,
    depth: usize,
) -> Option<Lit> {
    let v = all.get(id)?;
    let mode = roots.get(&v.collection).or(v.default_mode.as_ref())?;
    let raw = v.values.get(mode)?;
    if raw["type"] == "VARIABLE_ALIAS" {
        return (depth < 32)
            .then(|| root_value(all, roots, raw["id"].as_str()?, depth + 1))
            .flatten();
    }
    match v.resolved_type.as_str() {
        "COLOR" => color(raw).map(Lit::Color),
        "FLOAT" => raw.as_f64().map(Lit::Number),
        "STRING" => raw.as_str().map(|s| Lit::Text(s.to_string())),
        _ => None,
    }
}

/// The variable id an alias (or an array of aliases) names. `Err` when an
/// array names more than one variable — a mixed binding.
fn alias_id(v: Option<&Value>) -> Result<Option<String>, ()> {
    let one = |a: &Value| {
        (a["type"] == "VARIABLE_ALIAS")
            .then(|| a["id"].as_str().map(str::to_string))
            .flatten()
    };
    match v {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let ids: BTreeSet<String> = items.iter().filter_map(one).collect();
            match ids.len() {
                0 => Ok(None),
                1 => Ok(ids.into_iter().next()),
                _ => Err(()),
            }
        }
        Some(a) => Ok(one(a)),
    }
}

/// Every variable an alias or an alias array names.
fn alias_ids(v: Option<&Value>) -> BTreeSet<String> {
    let items = match v {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(a) => vec![a],
        None => vec![],
    };
    items
        .into_iter()
        .filter(|a| a["type"] == "VARIABLE_ALIAS")
        .filter_map(|a| a["id"].as_str().map(str::to_string))
        .collect()
}

/// A binding: the variable, and the JSON pointers (relative to the node)
/// that name it.
#[derive(Debug, Clone, PartialEq)]
struct Bind {
    var: String,
    at: Vec<String>,
}

/// `base` + the RFC 6901 pointer of `path`.
fn pointer(base: &str, path: &[&str]) -> String {
    let mut out = base.to_string();
    for p in path {
        out.push('/');
        out.push_str(&p.replace('~', "~0").replace('/', "~1"));
    }
    out
}

/// `v[p]`, an array index when `v` is an array.
fn step<'a>(v: &'a Value, p: &str) -> Option<&'a Value> {
    match v {
        Value::Array(items) => items.get(p.parse::<usize>().ok()?),
        _ => v.get(p),
    }
}

/// Every `VARIABLE_ALIAS` under `v` (children excluded), by pointer.
fn alias_pointers(v: &Value, at: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(m) if m.get("type").and_then(Value::as_str) == Some("VARIABLE_ALIAS") => {
            out.push(at.to_string());
        }
        Value::Object(m) => {
            for (k, x) in m {
                if !(at.is_empty() && k == "children") {
                    alias_pointers(x, &pointer(at, &[k]), out);
                }
            }
        }
        Value::Array(xs) => {
            for (i, x) in xs.iter().enumerate() {
                alias_pointers(x, &pointer(at, &[&i.to_string()]), out);
            }
        }
        _ => {}
    }
}

/// What CSS needs from a value.
#[derive(Clone, Copy, PartialEq)]
enum Want {
    /// A length: px literals; a unitless token is scaled by `1px`.
    Px,
    /// A length, left out when it is an unbound zero.
    PxNonZero,
    /// `opacity`: a 0–1 literal; a `%` token as is, a unitless (0–100)
    /// token divided by 100.
    Opacity,
    /// A plain number (`font-weight`).
    Number,
    /// A color.
    Color,
    /// A font family (a quoted string).
    Family,
}

/// A node's absolute bounding box.
#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn rect(n: &Value) -> Option<Rect> {
    let b = n.get("absoluteBoundingBox")?;
    Some(Rect {
        x: b["x"].as_f64()?,
        y: b["y"].as_f64()?,
        w: b["width"].as_f64()?,
        h: b["height"].as_f64()?,
    })
}

/// The node's layout parent.
#[derive(Clone, Copy, PartialEq)]
enum Parent {
    /// The spec's root: its parent is outside the spec.
    Root,
    /// An auto-layout (flex) parent with this direction.
    Flex(&'static str),
    /// A `layoutMode: GRID` parent.
    Grid,
    /// A frame without auto layout: children are placed by constraints.
    Free,
    /// A parent whose child placement isn't specified (a group, an
    /// unknown layout mode).
    Unplaced,
}

/// What a node inherits from its parent.
struct Up {
    kind: Parent,
    /// The parent's bounding box (for constraints).
    bbox: Option<Rect>,
    /// A negative auto-layout gap, as a margin on every in-flow child
    /// after the first.
    margin: Option<(&'static str, CssVal)>,
    /// The parent's CSS border width (a stroke included in layout): CSS
    /// offsets an absolute child from the padding box, inside it.
    border: Option<CssVal>,
}

/// What a node is as a container.
#[derive(Clone, Copy, PartialEq)]
enum Container {
    Flex(&'static str),
    Grid,
    Free,
    /// Children not placed (group, unknown layout mode).
    Unplaced,
    /// No children to place.
    Leaf,
}

struct Cx<'a> {
    tokens: &'a Tokens,
    components: &'a Value,
    tailwind: bool,
    map: Option<&'a CodeMap>,
    unsupported: BTreeSet<(String, String)>,
    /// The current node's binding pointers read so far; any other is
    /// listed as unsupported (never dropped silently).
    consumed: BTreeSet<String>,
    /// Where the type style read by [`Cx::binding`] lives in the node
    /// (`/style`, or a run's `/styleOverrideTable/<n>`).
    style_base: String,
}

/// `SOLID` paint color as `#rrggbb[aa]`, the paint's opacity folded into
/// alpha — the same rendering `figmog tokens export` uses for a COLOR.
fn paint_color(paint: &Value) -> Option<String> {
    let mut c = paint.get("color")?.clone();
    let opacity = paint.get("opacity").and_then(Value::as_f64).unwrap_or(1.0);
    let a = c.get("a").and_then(Value::as_f64).unwrap_or(1.0);
    c["a"] = json!(a * opacity);
    color(&c)
}

fn lower(v: &Value, default: &str) -> String {
    v.as_str().unwrap_or(default).to_ascii_lowercase()
}

/// The `design` layer: the recorded layout properties, verbatim.
fn design(n: &Value) -> Map<String, Value> {
    let mut out: Map<String, Value> = DESIGN_KEYS
        .iter()
        .filter_map(|k| Some((k.to_string(), n.get(*k)?.clone())))
        .collect();
    if n["type"] == "TEXT" {
        let style: Map<String, Value> = DESIGN_STYLE_KEYS
            .iter()
            .filter_map(|k| Some((k.to_string(), n["style"].get(*k)?.clone())))
            .collect();
        if !style.is_empty() {
            out.insert("style".into(), Value::Object(style));
        }
    }
    out
}

/// A CSS track list as Figma writes `gridColumnsSizing`: plain tokens and
/// functions only (`1fr 1fr`, `repeat(2, minmax(0, 1fr))`, `48px auto`).
fn track_list(s: &str) -> bool {
    !s.trim().is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " .,()%-".contains(c))
}

impl Cx<'_> {
    fn unsupported(&mut self, id: &str, reason: impl Into<String>) {
        self.unsupported.insert((id.to_string(), reason.into()));
    }

    /// The binding at `path` under `v` (whose pointer is `base`): marked
    /// read, and `Err(pointer)` for a mixed alias array.
    fn lookup(&mut self, v: &Value, base: &str, path: &[&str]) -> Result<Option<Bind>, String> {
        let mut at = Some(v);
        for p in path {
            at = at.and_then(|x| step(x, p));
        }
        let ptr = pointer(base, path);
        if at.is_some() {
            self.consumed.insert(ptr.clone());
        }
        match alias_id(at) {
            Ok(Some(var)) => Ok(Some(Bind { var, at: vec![ptr] })),
            Ok(None) => Ok(None),
            Err(()) => Err(ptr),
        }
    }

    /// Two places one property can be bound (a paint and the node's
    /// `fills[i]`, a node and its type style): the first wins; the second
    /// naming the same variable joins it, another variable is listed.
    fn either(
        &mut self,
        id: &str,
        first: Result<Option<Bind>, String>,
        second: Result<Option<Bind>, String>,
    ) -> Result<Option<Bind>, ()> {
        let mixed = |cx: &mut Self, p: &str| {
            cx.unsupported(
                id,
                format!("{p}: bound to several variables (mixed): not specified"),
            );
        };
        match (first, second) {
            (Err(p), _) => {
                mixed(self, &p);
                Err(())
            }
            (Ok(None), Err(p)) => {
                mixed(self, &p);
                Err(())
            }
            (Ok(None), Ok(b)) => Ok(b),
            (Ok(Some(mut a)), other) => {
                match other {
                    Ok(Some(b)) if b.var == a.var => a.at.extend(b.at),
                    Ok(Some(b)) => self.unsupported(
                        id,
                        format!(
                            "{}: bound to {}, but {} ({}) is used: not specified",
                            b.at[0], b.var, a.at[0], a.var
                        ),
                    ),
                    Err(p) => mixed(self, &p),
                    Ok(None) => {}
                }
                Ok(Some(a))
            }
        }
    }

    /// A node's binding for `key`: the node's `boundVariables`, then — for
    /// text properties — the type style's own, reporting a mixed binding.
    /// With `runs` (a mixed-run node, or a run), only the type style's: the
    /// node-level per-range arrays are read when the style and the runs'
    /// overrides name every variable in them, else they stay unread (and
    /// listed).
    fn binding(&mut self, n: &Value, id: &str, key: &str, runs: bool) -> Result<Option<Bind>, ()> {
        let style_base = self.style_base.clone();
        if !runs {
            let node = self.lookup(n, "", &["boundVariables", key]);
            let style = self.lookup(&n["style"], &style_base, &["boundVariables", key]);
            return self.either(id, node, style);
        }
        let style = self.lookup(&n["style"], &style_base, &["boundVariables", key]);
        if let Some(v) = n.get("boundVariables").and_then(|b| b.get(key)) {
            let mut named = BTreeSet::new();
            if let Ok(Some(b)) = &style {
                named.insert(b.var.clone());
            }
            for (_, o) in n["styleOverrideTable"].as_object().into_iter().flatten() {
                named.extend(alias_ids(o.get("boundVariables").and_then(|b| b.get(key))));
            }
            if alias_ids(Some(v)).is_subset(&named) {
                self.consumed.insert(pointer("", &["boundVariables", key]));
            }
        }
        self.either(id, style, Ok(None))
    }

    /// A numeric property: bound or untokenized, raw value as stored.
    /// `keys` are the binding keys tried in order (a property Figma binds
    /// under more than one name; `size.x` is a path).
    fn number(
        &mut self,
        n: &Value,
        id: &str,
        keys: &[&str],
        raw: Value,
        prop: Prop,
    ) -> Option<Value> {
        let binding = self.bind_any(n, id, keys).ok()?;
        let lit = raw.as_f64().map(Lit::Number);
        Some(self.tokens.entry(binding, raw, lit.as_ref(), prop))
    }

    /// The first of `keys` that is bound (see [`Cx::number`]).
    fn bind_any(&mut self, n: &Value, id: &str, keys: &[&str]) -> Result<Option<Bind>, ()> {
        for key in keys {
            let path: Vec<&str> = key.split('.').collect();
            let found = if path.len() == 1 {
                self.binding(n, id, key, false)?
            } else {
                let mut full = vec!["boundVariables"];
                full.extend(path);
                match self.lookup(n, "", &full) {
                    Ok(b) => b,
                    Err(p) => {
                        self.unsupported(
                            id,
                            format!("{p}: bound to several variables (mixed): not specified"),
                        );
                        return Err(());
                    }
                }
            };
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// An entry's CSS value for `prop`: the bound token's `var()` (with
    /// its Tailwind theme name) when the token export names it — scaled to
    /// what `prop` takes when the export writes it unitless, listed as
    /// unsupported when the export's unit can't be converted — else the
    /// raw value. `None` when neither exists.
    fn css_value(&mut self, id: &str, prop: &str, e: &Value, want: Want) -> Option<CssVal> {
        if let Some(var) = e["variable_id"].as_str()
            && let Some(name) = self.tokens.css.get(var)
        {
            let plain = CssVal {
                value: format!("var({name})"),
                theme: self.tokens.theme.get(var).cloned(),
            };
            let unit = self.tokens.units.get(var).copied();
            return match (want, unit) {
                (Want::Color | Want::Family, _) | (_, None) => Some(plain),
                (Want::Px | Want::PxNonZero, Some(Some("px")))
                | (Want::Opacity, Some(Some("%")))
                | (Want::Number, Some(None)) => Some(plain),
                (Want::Px | Want::PxNonZero, Some(None)) => {
                    Some(CssVal::lit(format!("calc(var({name}) * 1px)")))
                }
                (Want::Opacity, Some(None)) => {
                    Some(CssVal::lit(format!("calc(var({name}) / 100)")))
                }
                (_, Some(unit)) => {
                    let unit = unit.unwrap_or("unitless");
                    self.unsupported(
                        id,
                        format!("{prop}: token {name} is exported in {unit}, which {prop} can't take: not specified"),
                    );
                    None
                }
            };
        }
        let raw = &e["raw"];
        match want {
            Want::PxNonZero if is_unbound_zero(e) => None,
            Want::Px | Want::PxNonZero => raw.as_f64().map(px),
            Want::Opacity | Want::Number => raw.as_f64().map(num),
            Want::Color => raw.as_str().map(str::to_string),
            Want::Family => raw.as_str().map(|s| format!("{s:?}")),
        }
        .map(CssVal::lit)
    }

    /// Bindings of this node the spec didn't read: listed, never dropped.
    fn unread_bindings(&mut self, n: &Value, id: &str) {
        let mut all = Vec::new();
        alias_pointers(n, "", &mut all);
        for p in all {
            let read = self
                .consumed
                .iter()
                .any(|c| p == *c || p.starts_with(&format!("{c}/")));
            if !read {
                self.unsupported(id, format!("{p}: bound to a variable: not specified"));
            }
        }
    }

    fn node(&mut self, n: &Value, up: &Up) -> Value {
        let id = n["id"].as_str().unwrap_or_default().to_string();
        let node_type = n["type"].as_str().unwrap_or_default().to_string();
        let mut out = Map::new();
        out.insert("id".into(), json!(id));
        out.insert("name".into(), n["name"].clone());
        out.insert("type".into(), json!(node_type));
        let design = design(n);
        if !design.is_empty() {
            out.insert("design".into(), Value::Object(design));
        }
        if n["visible"] == false {
            self.unsupported(&id, "hidden layer (visible: false): not specified");
            return Value::Object(out);
        }
        self.consumed.clear();
        let mut css = Css::default();
        if let Some((prop, margin)) = &up.margin {
            css.set(prop, margin.clone());
        }
        let mut layout = Map::new();
        let mut values = Map::new();
        let mut content = Map::new();
        let vector = VECTOR_LIKE.contains(&node_type.as_str());

        // Own layout.
        let kids: &[Value] = if vector {
            if let Some(k) = n["children"].as_array().filter(|k| !k.is_empty()) {
                self.unsupported(
                    &id,
                    format!(
                        "{} children are part of the vector asset: not specified",
                        k.len()
                    ),
                );
            }
            &[]
        } else {
            n["children"].as_array().map(Vec::as_slice).unwrap_or(&[])
        };
        let mode = n["layoutMode"].as_str().unwrap_or("NONE");
        let container = match mode {
            "HORIZONTAL" => Container::Flex("HORIZONTAL"),
            "VERTICAL" => Container::Flex("VERTICAL"),
            "GRID" => Container::Grid,
            "NONE" if kids.is_empty() => Container::Leaf,
            "NONE" if FRAME_LIKE.contains(&node_type.as_str()) => Container::Free,
            "NONE" => {
                self.unsupported(
                    &id,
                    format!(
                        "{node_type}: children are placed by the enclosing frame's constraints: child positions not specified"
                    ),
                );
                Container::Unplaced
            }
            other => {
                self.unsupported(
                    &id,
                    format!("layoutMode {other}: own layout and child positions not specified"),
                );
                Container::Unplaced
            }
        };
        let mut margin = None;
        match container {
            Container::Flex(dir) => margin = self.flex(n, &id, dir, &mut layout, &mut css),
            Container::Grid => self.grid(n, &id, &mut layout, &mut css),
            _ => {}
        }
        if n["clipsContent"] == true && !kids.is_empty() {
            css.lit("overflow", "hidden");
        }

        // Placement in the parent, own size, limits.
        let own = match container {
            Container::Flex(d) => Some(d),
            _ => None,
        };
        let absolute = self.place(
            n,
            &id,
            up,
            own,
            container == Container::Grid,
            &mut layout,
            &mut css,
        );
        self.limits(n, &id, &mut layout, &mut css);
        if !layout.is_empty()
            && let Some(b) = n.get("absoluteBoundingBox")
        {
            layout.insert(
                "size".into(),
                json!({"width": b["width"], "height": b["height"]}),
            );
        }
        if !layout.is_empty() {
            out.insert("layout".into(), Value::Object(layout));
        }

        // Values.
        let mut assets = Vec::new();
        self.opacity(n, &id, &mut values, &mut css);
        let fill_css = (!vector).then_some(if node_type == "TEXT" {
            "color"
        } else {
            "background-color"
        });
        let fills = self.paints(
            n,
            "",
            &id,
            "fills",
            fill_prop(&node_type),
            &mut assets,
            fill_css.map(|p| (p, &mut css)),
        );
        if !fills.is_empty() {
            values.insert("fill".into(), Value::Array(fills));
        }
        self.strokes(n, &id, &node_type, vector, &mut values, &mut css);
        if let Some(radius) = self.radius(n, &id, (!vector).then_some(&mut css)) {
            values.insert("radius".into(), radius);
        }
        let effects = self.effects(n, &id, node_type == "TEXT", vector, &mut css);
        if !effects.is_empty() {
            values.insert("effects".into(), Value::Array(effects));
        }
        if node_type == "TEXT" {
            self.text(n, &id, &mut values, &mut content, &mut css);
        }
        out.insert("values".into(), Value::Object(values));

        // Content: assets.
        if vector {
            assets.push(json!({"kind": "vector", "node_id": id, "format": "svg"}));
        }
        let formats: BTreeSet<String> = n["exportSettings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let format = s["format"].as_str()?;
                let c = &s["constraint"];
                Some(match (c["type"].as_str(), c["value"].as_f64()) {
                    (Some("SCALE"), Some(v)) if v != 1.0 => format!("{format}@{}x", num(v)),
                    _ => format.to_string(),
                })
            })
            .collect();
        if !formats.is_empty() {
            assets.push(json!({"kind": "export", "node_id": id, "formats": formats}));
        }
        if !assets.is_empty() {
            content.insert("assets".into(), Value::Array(assets));
        }
        if !content.is_empty() {
            out.insert("content".into(), Value::Object(content));
        }
        if node_type == "INSTANCE" {
            out.insert("instance".into(), self.instance(n, &id));
        }

        // Children.
        let positioned = |k: &Value| {
            k["visible"] != false
                && (container == Container::Free || k["layoutPositioning"] == "ABSOLUTE")
        };
        if !absolute
            && matches!(
                container,
                Container::Flex(_) | Container::Grid | Container::Free
            )
            && kids.iter().any(positioned)
        {
            css.lit("position", "relative");
        }
        if css.has("padding")
            || css.has("padding-top")
            || css.has("padding-right")
            || css.has("padding-bottom")
            || css.has("padding-left")
            || css.has("border-width")
        {
            css.lit("box-sizing", "border-box");
        }
        let kind = match container {
            Container::Flex(d) => Parent::Flex(d),
            Container::Grid => Parent::Grid,
            Container::Free => Parent::Free,
            Container::Unplaced | Container::Leaf => Parent::Unplaced,
        };
        self.unread_bindings(n, &id);
        let bbox = rect(n);
        let border = css.get("border-width").cloned();
        let mut seen_in_flow = false;
        let mut children = Vec::with_capacity(kids.len());
        for (i, k) in kids.iter().enumerate() {
            let in_flow = k["visible"] != false && k["layoutPositioning"] != "ABSOLUTE";
            if matches!(container, Container::Flex(_) | Container::Grid)
                && k["visible"] != false
                && k["layoutPositioning"] == "ABSOLUTE"
                && kids[i + 1..].iter().any(|later| {
                    later["visible"] != false && later["layoutPositioning"] != "ABSOLUTE"
                })
            {
                self.unsupported(
                    k["id"].as_str().unwrap_or_default(),
                    "absolute child before in-flow siblings: CSS paints it above them, Figma below: stacking not specified",
                );
            }
            let child_up = Up {
                kind,
                bbox,
                margin: margin.clone().filter(|_| in_flow && seen_in_flow),
                border: border.clone(),
            };
            seen_in_flow |= in_flow;
            children.push(self.node(k, &child_up));
        }
        if !css.is_empty() {
            if self.tailwind {
                out.insert("tailwind".into(), json!(css.tailwind()));
            }
            out.insert("css".into(), css.to_json());
        }
        out.insert("children".into(), Value::Array(children));
        Value::Object(out)
    }

    /// An auto-layout frame: direction, wrap, gap, padding, alignment.
    /// Returns the margin a negative gap puts on in-flow children after
    /// the first.
    fn flex(
        &mut self,
        n: &Value,
        id: &str,
        dir: &'static str,
        layout: &mut Map<String, Value>,
        css: &mut Css,
    ) -> Option<(&'static str, CssVal)> {
        layout.insert("direction".into(), json!(dir.to_ascii_lowercase()));
        css.lit("display", "flex");
        css.lit(
            "flex-direction",
            if dir == "HORIZONTAL" { "row" } else { "column" },
        );
        let mut wrap = n["layoutWrap"] == "WRAP";
        if wrap && dir == "VERTICAL" {
            self.unsupported(
                id,
                "layoutWrap WRAP on a vertical frame: wrapping not specified",
            );
            wrap = false;
        }
        if wrap {
            layout.insert("wrap".into(), json!(true));
            css.lit("flex-wrap", "wrap");
        }
        if n["itemReverseZIndex"] == true {
            self.unsupported(
                id,
                "itemReverseZIndex: first child drawn on top: stacking not specified",
            );
        }
        let primary = n["primaryAxisAlignItems"].as_str().unwrap_or("MIN");
        let mut margin = None;
        if primary == "SPACE_BETWEEN" {
            layout.insert("gap".into(), json!("auto"));
            // Figma ignores the item spacing here; a binding on it is
            // still shown (and that CSS doesn't use it, listed).
            if let Ok(Some(bind)) = self.binding(n, id, "itemSpacing", false) {
                let raw = n.get("itemSpacing").cloned().unwrap_or(json!(0));
                let e = self.tokens.bound(&bind, raw);
                self.unsupported(
                    id,
                    format!(
                        "{}: itemSpacing is bound, but primaryAxisAlignItems SPACE_BETWEEN spaces the children: the token is not used in CSS",
                        bind.at[0]
                    ),
                );
                layout.insert("item_spacing".into(), e);
            }
        } else {
            let raw = n.get("itemSpacing").cloned().unwrap_or(json!(0));
            let negative = raw.as_f64().is_some_and(|g| g < 0.0);
            if let Some(gap) = self.number(n, id, &["itemSpacing"], raw, GAP) {
                let value = self.css_value(id, "gap", &gap, Want::PxNonZero);
                layout.insert("gap".into(), gap);
                match (value, negative, wrap) {
                    (None, _, _) => {}
                    (Some(_), true, true) => self.unsupported(
                        id,
                        "itemSpacing: negative gap in a wrapping frame: gap not specified",
                    ),
                    (Some(v), true, false) => {
                        // CSS gaps can't be negative; a negative margin on
                        // every later child overlaps them the same way.
                        let side = if dir == "HORIZONTAL" {
                            "margin-left"
                        } else {
                            "margin-top"
                        };
                        margin = Some((side, v));
                    }
                    (Some(v), false, true) => css.set("column-gap", v),
                    (Some(v), false, false) => css.set("gap", v),
                }
            }
        }
        if wrap {
            // Figma's counter-axis spacing defaults to the item spacing
            // (value and binding).
            let own = n.get("counterAxisSpacing").filter(|v| !v.is_null());
            let raw = own
                .or_else(|| n.get("itemSpacing"))
                .cloned()
                .unwrap_or(json!(0));
            let keys: &[&str] = if own.is_some() {
                &["counterAxisSpacing"]
            } else {
                &["counterAxisSpacing", "itemSpacing"]
            };
            let negative = raw.as_f64().is_some_and(|g| g < 0.0);
            if let Some(gap) = self.number(n, id, keys, raw, GAP) {
                if negative {
                    self.unsupported(
                        id,
                        "counterAxisSpacing: negative gap between wrapped rows: row-gap not specified",
                    );
                } else if let Some(v) = self.css_value(id, "row-gap", &gap, Want::PxNonZero) {
                    css.set("row-gap", v);
                }
                layout.insert("wrap_gap".into(), gap);
            }
        }
        self.padding(n, id, layout, css);
        let align = |v: &Value| match v.as_str().unwrap_or("MIN") {
            "MIN" => "start".to_string(),
            "MAX" => "end".to_string(),
            "SPACE_BETWEEN" => "space-between".to_string(),
            other => other.to_ascii_lowercase(),
        };
        layout.insert(
            "align".into(),
            json!({
                "primary": align(&n["primaryAxisAlignItems"]),
                "counter": align(&n["counterAxisAlignItems"]),
            }),
        );
        match primary {
            "CENTER" => css.lit("justify-content", "center"),
            "MAX" => css.lit("justify-content", "flex-end"),
            "SPACE_BETWEEN" => css.lit("justify-content", "space-between"),
            _ => {}
        }
        let counter = match n["counterAxisAlignItems"].as_str().unwrap_or("MIN") {
            "CENTER" => "center",
            "MAX" => "flex-end",
            "BASELINE" => "baseline",
            _ => "flex-start",
        };
        css.lit("align-items", counter);
        if wrap {
            let content = if n["counterAxisAlignContent"] == "SPACE_BETWEEN" {
                "space-between"
            } else if counter == "baseline" {
                "flex-start"
            } else {
                counter
            };
            css.lit("align-content", content);
        }
        margin
    }

    /// A `layoutMode: GRID` frame: track counts, sizing, gaps, padding.
    fn grid(&mut self, n: &Value, id: &str, layout: &mut Map<String, Value>, css: &mut Css) {
        css.lit("display", "grid");
        let mut grid = Map::new();
        grid.insert("columns".into(), n["gridColumnCount"].clone());
        grid.insert("rows".into(), n["gridRowCount"].clone());
        for (key, prop) in [
            ("gridColumnsSizing", "grid-template-columns"),
            ("gridRowsSizing", "grid-template-rows"),
        ] {
            match n[key].as_str() {
                Some(s) if track_list(s) => css.lit(prop, s.trim()),
                Some(s) => self.unsupported(
                    id,
                    format!("{key} {s:?}: not a CSS track list: {prop} not specified"),
                ),
                None => self.unsupported(id, format!("{key} not recorded: {prop} not specified")),
            }
        }
        for (key, name, prop) in [
            ("gridColumnGap", "column_gap", "column-gap"),
            ("gridRowGap", "row_gap", "row-gap"),
        ] {
            let raw = n.get(key).cloned().unwrap_or(json!(0));
            let negative = raw.as_f64().is_some_and(|g| g < 0.0);
            if let Some(gap) = self.number(n, id, &[key], raw, GAP) {
                if negative {
                    self.unsupported(
                        id,
                        format!("{key}: negative grid gap: {prop} not specified"),
                    );
                } else if let Some(v) = self.css_value(id, prop, &gap, Want::PxNonZero) {
                    css.set(prop, v);
                }
                grid.insert(name.into(), gap);
            }
        }
        layout.insert("grid".into(), Value::Object(grid));
        self.padding(n, id, layout, css);
    }

    /// Auto-layout padding: `layout.padding` entries and CSS.
    fn padding(&mut self, n: &Value, id: &str, layout: &mut Map<String, Value>, css: &mut Css) {
        let mut padding = Map::new();
        let mut sides: [Option<CssVal>; 4] = Default::default();
        for (i, (side, key, prop)) in [
            ("top", "paddingTop", "padding-top"),
            ("right", "paddingRight", "padding-right"),
            ("bottom", "paddingBottom", "padding-bottom"),
            ("left", "paddingLeft", "padding-left"),
        ]
        .into_iter()
        .enumerate()
        {
            let raw = n.get(key).cloned().unwrap_or(json!(0));
            if let Some(e) = self.number(n, id, &[key], raw, GAP) {
                sides[i] = self.css_value(id, prop, &e, Want::PxNonZero);
                padding.insert(side.into(), e);
            }
        }
        layout.insert("padding".into(), Value::Object(padding));
        css.four(
            "padding",
            [
                "padding-top",
                "padding-right",
                "padding-bottom",
                "padding-left",
            ],
            sides,
        );
    }

    /// `width`/`height` from the bounding box: a `layout.width`/`height`
    /// entry (bound through `boundVariables.size.x`/`width`…) and CSS.
    fn dim(
        &mut self,
        n: &Value,
        id: &str,
        axis: &str,
        layout: &mut Map<String, Value>,
        css: &mut Css,
    ) {
        let (key, prop, keys) = if axis == "HORIZONTAL" {
            ("width", "width", ["size.x", "width"])
        } else {
            ("height", "height", ["size.y", "height"])
        };
        let raw = n["absoluteBoundingBox"][key].clone();
        if raw.is_null() {
            self.unsupported(id, format!("no absoluteBoundingBox: {prop} not specified"));
            return;
        }
        if let Some(e) = self.number(n, id, &keys, raw, SIZE) {
            if let Some(v) = self.css_value(id, prop, &e, Want::Px) {
                css.set(prop, v);
            }
            layout.insert(key.into(), e);
        }
    }

    /// Place the node in its parent: `layout.sizing` and the CSS for its
    /// size and position. Returns whether it is absolutely positioned.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        n: &Value,
        id: &str,
        up: &Up,
        own: Option<&'static str>,
        grid: bool,
        layout: &mut Map<String, Value>,
        css: &mut Css,
    ) -> bool {
        let absolute = match up.kind {
            Parent::Free => true,
            Parent::Flex(_) | Parent::Grid => n["layoutPositioning"] == "ABSOLUTE",
            Parent::Root => {
                if n["layoutPositioning"] == "ABSOLUTE" {
                    self.unsupported(
                        id,
                        "layoutPositioning ABSOLUTE on the spec root: its position in its parent is not specified",
                    );
                }
                false
            }
            Parent::Unplaced => false,
        };
        let parent_dir = match up.kind {
            Parent::Flex(d) if !absolute => Some(d),
            _ => None,
        };
        let has_sizing_fields =
            n.get("layoutSizingHorizontal").is_some() || n.get("layoutSizingVertical").is_some();
        let emit_sizing = match up.kind {
            Parent::Flex(_) | Parent::Grid | Parent::Free => true,
            Parent::Root => own.is_some() || grid || has_sizing_fields,
            Parent::Unplaced => own.is_some() || grid,
        };
        let h = sizing(n, "HORIZONTAL", own, parent_dir);
        let v = sizing(n, "VERTICAL", own, parent_dir);
        if emit_sizing {
            layout.insert(
                "sizing".into(),
                json!({"horizontal": h.clone(), "vertical": v.clone()}),
            );
        }
        if !absolute && n["rotation"].as_f64().is_some_and(|r| r.abs() > 1e-6) {
            self.unsupported(
                id,
                format!(
                    "rotation {}: the rotation is not specified (sizes are the rotated bounds)",
                    num(n["rotation"].as_f64().unwrap_or_default())
                ),
            );
        }
        if absolute {
            layout.insert("position".into(), json!("absolute"));
            self.position(n, id, up, [&h, &v], layout, css);
        } else {
            self.flow(n, id, up, [&h, &v], layout, css);
        }
        // A bound width/height the CSS doesn't declare (it follows the
        // sizing or the constraints): shown as its token, and listed.
        for (key, keys, s) in [
            ("width", ["size.x", "width"], &h),
            ("height", ["size.y", "height"], &v),
        ] {
            if layout.contains_key(key) {
                continue;
            }
            if let Ok(Some(bind)) = self.bind_any(n, id, &keys) {
                let why = if absolute {
                    "its constraints".to_string()
                } else {
                    format!("its {s} sizing")
                };
                self.unsupported(
                    id,
                    format!(
                        "{}: {key} is bound, but the CSS {key} follows {why}: the token is not used in CSS",
                        bind.at[0]
                    ),
                );
                let raw = n["absoluteBoundingBox"][key].clone();
                layout.insert(key.into(), self.tokens.bound(&bind, raw));
            }
        }
        absolute
    }

    /// An in-flow node's size (and grid placement) in its parent.
    fn flow(
        &mut self,
        n: &Value,
        id: &str,
        up: &Up,
        sizing: [&str; 2],
        layout: &mut Map<String, Value>,
        css: &mut Css,
    ) {
        for (axis, s) in [("HORIZONTAL", sizing[0]), ("VERTICAL", sizing[1])] {
            let horizontal = axis == "HORIZONTAL";
            let full = if horizontal { "width" } else { "height" };
            match (up.kind, s) {
                (_, "fixed") => self.dim(n, id, axis, layout, css),
                (Parent::Root, "hug") if horizontal => css.lit("width", "fit-content"),
                (Parent::Root, "fill") => css.lit(full, "100%"),
                (Parent::Flex(d), "fill") if d == axis => {
                    // Fill shares the space, down to nothing like Figma's.
                    css.lit("flex", "1 1 0%");
                    css.lit(
                        if horizontal {
                            "min-width"
                        } else {
                            "min-height"
                        },
                        "0",
                    );
                }
                (Parent::Flex(_), "fill") => css.lit("align-self", "stretch"),
                (Parent::Grid, "fill") => css.lit(
                    if horizontal {
                        "justify-self"
                    } else {
                        "align-self"
                    },
                    "stretch",
                ),
                (Parent::Grid, "hug") => css.lit(full, "fit-content"),
                _ => {}
            }
            if let Parent::Flex(d) = up.kind
                && d == axis
                && s != "fill"
            {
                css.lit("flex-shrink", "0");
            }
            if up.kind == Parent::Grid && s != "fill" {
                let (key, prop) = if horizontal {
                    ("gridChildHorizontalAlign", "justify-self")
                } else {
                    ("gridChildVerticalAlign", "align-self")
                };
                match n[key].as_str() {
                    Some("MIN") => css.lit(prop, "start"),
                    Some("CENTER") => css.lit(prop, "center"),
                    Some("MAX") => css.lit(prop, "end"),
                    _ => {}
                }
            }
        }
        if up.kind == Parent::Grid {
            for (anchor, span, prop) in [
                ("gridColumnAnchorIndex", "gridColumnSpan", "grid-column"),
                ("gridRowAnchorIndex", "gridRowSpan", "grid-row"),
            ] {
                let span = n[span].as_u64().unwrap_or(1);
                match n[anchor].as_u64() {
                    Some(a) => css.lit(prop, format!("{} / span {span}", a + 1)),
                    None if span > 1 => css.lit(prop, format!("span {span}")),
                    None => {}
                }
            }
        }
    }

    /// An absolutely positioned node: offsets from its constraints,
    /// relative to the parent's box (Figma's default constraints are
    /// LEFT/TOP).
    fn position(
        &mut self,
        n: &Value,
        id: &str,
        up: &Up,
        sizing: [&str; 2],
        layout: &mut Map<String, Value>,
        css: &mut Css,
    ) {
        css.lit("position", "absolute");
        if let Some(r) = n["rotation"].as_f64().filter(|r| r.abs() > 1e-6) {
            self.unsupported(
                id,
                format!("rotation {}: position and size not specified", num(r)),
            );
            return;
        }
        let (Some(p), Some(b)) = (up.bbox, rect(n)) else {
            self.unsupported(id, "no absoluteBoundingBox: position not specified");
            return;
        };
        let mut translate = [false, false];
        for (i, axis) in ["HORIZONTAL", "VERTICAL"].into_iter().enumerate() {
            let (start, end, d, s, ps, key, default) = if i == 0 {
                ("left", "right", b.x - p.x, b.w, p.w, "horizontal", "LEFT")
            } else {
                ("top", "bottom", b.y - p.y, b.h, p.h, "vertical", "TOP")
            };
            let constraint = n["constraints"][key].as_str().unwrap_or(default);
            let size = sizing[i] != "hug";
            let percent = |v: f64| format!("{}%", num(v / ps * 100.0));
            // CSS offsets from the padding box: inside the parent's border.
            let inset = |v: f64| match &up.border {
                None => px(v),
                Some(b) if b.is_var() => format!("calc({} - {})", px(v), b.value),
                Some(b) => px(v - b.value.trim_end_matches("px").parse::<f64>().unwrap_or(0.0)),
            };
            match constraint {
                "LEFT" | "TOP" | "MIN" => {
                    css.lit(start, inset(d));
                    if size {
                        self.dim(n, id, axis, layout, css);
                    }
                }
                "RIGHT" | "BOTTOM" | "MAX" => {
                    css.lit(end, inset(ps - d - s));
                    if size {
                        self.dim(n, id, axis, layout, css);
                    }
                }
                "LEFT_RIGHT" | "TOP_BOTTOM" | "STRETCH" => {
                    css.lit(start, inset(d));
                    css.lit(end, inset(ps - d - s));
                }
                "CENTER" => {
                    // The node's center keeps its offset from the parent's.
                    let off = d + s / 2.0 - ps / 2.0;
                    let at = match off {
                        o if o.abs() < 5e-4 => "50%".to_string(),
                        o if o > 0.0 => format!("calc(50% + {})", px(o)),
                        o => format!("calc(50% - {})", px(-o)),
                    };
                    css.lit(start, at);
                    translate[i] = true;
                    if size {
                        self.dim(n, id, axis, layout, css);
                    }
                }
                "SCALE" if up.border.is_some() => self.unsupported(
                    id,
                    format!(
                        "constraints.{key} SCALE inside a border: CSS percentages are of the padding box: {start} not specified"
                    ),
                ),
                "SCALE" if ps > 0.0 => {
                    css.lit(start, percent(d));
                    css.lit(if i == 0 { "width" } else { "height" }, percent(s));
                }
                other => self.unsupported(
                    id,
                    format!("constraints.{key} {other}: {start} not specified"),
                ),
            }
        }
        match translate {
            [true, true] => css.lit("transform", "translate(-50%, -50%)"),
            [true, false] => css.lit("transform", "translateX(-50%)"),
            [false, true] => css.lit("transform", "translateY(-50%)"),
            [false, false] => {}
        }
    }

    /// `minWidth`/`maxWidth`/`minHeight`/`maxHeight`.
    fn limits(&mut self, n: &Value, id: &str, layout: &mut Map<String, Value>, css: &mut Css) {
        for (key, name, prop) in [
            ("minWidth", "min_width", "min-width"),
            ("maxWidth", "max_width", "max-width"),
            ("minHeight", "min_height", "min-height"),
            ("maxHeight", "max_height", "max-height"),
        ] {
            let Some(raw) = n.get(key).filter(|v| !v.is_null()).cloned() else {
                continue;
            };
            if let Some(e) = self.number(n, id, &[key], raw, SIZE) {
                if let Some(v) = self.css_value(id, prop, &e, Want::Px) {
                    css.set(prop, v);
                }
                layout.insert(name.into(), e);
            }
        }
    }

    /// Layer opacity below 1 (or bound) and the node's blend mode.
    fn opacity(&mut self, n: &Value, id: &str, values: &mut Map<String, Value>, css: &mut Css) {
        let bound = n["boundVariables"].get("opacity").is_some();
        if let Some(o) = n["opacity"].as_f64().filter(|o| *o < 1.0 || bound) {
            // Figma opacity variables are 0–100; the layer stores 0–1.
            let binding = self.binding(n, id, "opacity", false).ok().flatten();
            let lit = Lit::Number(o * 100.0);
            let e = self
                .tokens
                .entry(binding, n["opacity"].clone(), Some(&lit), OPACITY);
            if let Some(v) = self.css_value(id, "opacity", &e, Want::Opacity) {
                css.set("opacity", v);
            }
            values.insert("opacity".into(), e);
        }
        if let Some(mode) = n["blendMode"]
            .as_str()
            .filter(|m| !["PASS_THROUGH", "NORMAL"].contains(m))
        {
            self.unsupported(id, format!("blendMode {mode}: not specified"));
        }
    }

    /// Visible `SOLID` paints of `fills`/`strokes` as value entries; an
    /// `IMAGE` fill becomes an asset reference; any other paint type is
    /// unsupported. With `css`, a single visible solid paint is declared
    /// as that property.
    #[allow(clippy::too_many_arguments)]
    fn paints(
        &mut self,
        n: &Value,
        base: &str,
        id: &str,
        key: &str,
        prop: Prop,
        assets: &mut Vec<Value>,
        css: Option<(&'static str, &mut Css)>,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        let mut visible = 0;
        for (i, paint) in n[key].as_array().into_iter().flatten().enumerate() {
            if paint["visible"] == false {
                continue;
            }
            visible += 1;
            let ty = paint["type"].as_str().unwrap_or("unknown");
            if let Some(mode) = paint["blendMode"]
                .as_str()
                .filter(|m| !["PASS_THROUGH", "NORMAL"].contains(m))
            {
                self.unsupported(id, format!("{key}[{i}]: blendMode {mode}: not specified"));
            }
            if ty == "IMAGE" && key == "fills" {
                assets.push(json!({
                    "kind": "image",
                    "node_id": id,
                    "paint": format!("fills[{i}]"),
                    "image_ref": paint["imageRef"],
                    "scale_mode": paint["scaleMode"],
                }));
                self.unsupported(
                    id,
                    format!(
                        "fills[{i}]: IMAGE paint: referenced under content.assets, no CSS background until the asset manifest (NWS-2952)"
                    ),
                );
                continue;
            }
            let hex = (ty == "SOLID").then(|| paint_color(paint)).flatten();
            let Some(hex) = hex else {
                self.unsupported(id, format!("{key}[{i}]: {ty} paint not specified"));
                continue;
            };
            // The node-level binding is a fallback only when the paint has
            // none of its own: a mixed paint binding stays an error.
            let index = i.to_string();
            let own = self.lookup(n, base, &[key, &index, "boundVariables", "color"]);
            let node = self.lookup(n, base, &["boundVariables", key, &index]);
            let Ok(binding) = self.either(id, own, node) else {
                continue;
            };
            let lit = Lit::Color(hex.clone());
            out.push(self.tokens.entry(binding, json!(hex), Some(&lit), prop));
        }
        if let Some((property, css)) = css {
            match (visible, out.as_slice()) {
                (1, [e]) => {
                    if let Some(v) = self.css_value(id, property, e, Want::Color) {
                        css.set(property, v);
                    }
                }
                (0 | 1, _) => {}
                (many, _) => self.unsupported(
                    id,
                    format!("{key}: {many} visible paints: CSS {property} not specified"),
                ),
            }
        }
        out
    }

    /// Strokes: color entries, weight, and CSS — an `outline` (Figma
    /// strokes don't take layout space), or a `border` inside an
    /// auto-layout frame with `strokesIncludedInLayout`.
    fn strokes(
        &mut self,
        n: &Value,
        id: &str,
        node_type: &str,
        vector: bool,
        values: &mut Map<String, Value>,
        css: &mut Css,
    ) {
        let mut ignored = Vec::new();
        let strokes = self.paints(n, "", id, "strokes", STROKE_COLOR, &mut ignored, None);
        if strokes.is_empty() {
            return;
        }
        let visible = n["strokes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["visible"] != false)
            .count();
        let color = if visible == 1 {
            self.css_value(id, "outline-color", &strokes[0], Want::Color)
        } else {
            None
        };
        values.insert("stroke".into(), Value::Array(strokes));
        let mut weight = None;
        if n.get("individualStrokeWeights").is_some() {
            self.unsupported(id, "individualStrokeWeights: stroke weight not specified");
        } else if let Some(raw) = n.get("strokeWeight").cloned()
            && let Some(e) = self.number(n, id, &["strokeWeight"], raw, STROKE_WEIGHT)
        {
            weight = self.css_value(id, "outline-width", &e, Want::Px);
            values.insert("stroke_weight".into(), e);
        }
        if vector {
            return;
        }
        let why = if node_type == "TEXT" {
            Some("text stroke".to_string())
        } else if visible > 1 {
            Some(format!("{visible} visible stroke paints"))
        } else if n["strokeDashes"].as_array().is_some_and(|d| !d.is_empty()) {
            Some("strokeDashes (dash pattern)".to_string())
        } else if n.get("strokeAlign").is_none() {
            Some("strokeAlign not recorded".to_string())
        } else {
            None
        };
        if let Some(why) = why {
            self.unsupported(id, format!("{why}: CSS stroke not specified"));
            return;
        }
        let (Some(color), Some(weight)) = (color, weight) else {
            return;
        };
        let align = n["strokeAlign"].as_str().unwrap_or_default();
        if align == "INSIDE" && n["strokesIncludedInLayout"] == true {
            css.set("border-width", weight);
            css.lit("border-style", "solid");
            css.set("border-color", color);
            return;
        }
        let offset = match align {
            "INSIDE" => Some(weight.scaled(-1.0, "px")),
            "CENTER" => Some(weight.scaled(-0.5, "px")),
            _ => None,
        };
        css.set("outline-width", weight);
        css.lit("outline-style", "solid");
        css.set("outline-color", color);
        if let Some(offset) = offset {
            css.set("outline-offset", offset);
        }
    }

    /// The four corner radii, each bound or untokenized; `None` when the
    /// node has no radius. With `css`, `border-radius`.
    fn radius(&mut self, n: &Value, id: &str, css: Option<&mut Css>) -> Option<Value> {
        const CORNERS: [(&str, &str, &str, &str); 4] = [
            (
                "top_left",
                "topLeftRadius",
                "rectangleCornerRadii.RECTANGLE_TOP_LEFT_CORNER_RADIUS",
                "border-top-left-radius",
            ),
            (
                "top_right",
                "topRightRadius",
                "rectangleCornerRadii.RECTANGLE_TOP_RIGHT_CORNER_RADIUS",
                "border-top-right-radius",
            ),
            (
                "bottom_right",
                "bottomRightRadius",
                "rectangleCornerRadii.RECTANGLE_BOTTOM_RIGHT_CORNER_RADIUS",
                "border-bottom-right-radius",
            ),
            (
                "bottom_left",
                "bottomLeftRadius",
                "rectangleCornerRadii.RECTANGLE_BOTTOM_LEFT_CORNER_RADIUS",
                "border-bottom-left-radius",
            ),
        ];
        let bv = n.get("boundVariables");
        let any_bound = CORNERS
            .iter()
            .any(|(_, k, _, _)| bv.and_then(|b| b.get(*k)).is_some())
            || bv.and_then(|b| b.get("rectangleCornerRadii")).is_some();
        let per_corner = n["rectangleCornerRadii"].as_array();
        if n.get("cornerRadius").is_none() && per_corner.is_none() && !any_bound {
            return None;
        }
        let mut out = Map::new();
        let mut sides: [Option<CssVal>; 4] = Default::default();
        for (i, (side, key, alt, prop)) in CORNERS.iter().enumerate() {
            let raw = per_corner
                .and_then(|r| r.get(i))
                .or_else(|| n.get("cornerRadius"))
                .cloned()
                .unwrap_or(json!(0));
            if let Some(e) = self.number(n, id, &[key, alt], raw, RADIUS) {
                sides[i] = self.css_value(id, prop, &e, Want::PxNonZero);
                out.insert(side.to_string(), e);
            }
        }
        if let Some(css) = css {
            if n["cornerSmoothing"].as_f64().is_some_and(|s| s > 0.0) {
                self.unsupported(id, "cornerSmoothing: smoothed corners not specified");
            }
            css.four(
                "border-radius",
                [CORNERS[0].3, CORNERS[1].3, CORNERS[2].3, CORNERS[3].3],
                sides,
            );
        }
        Some(Value::Object(out))
    }

    /// Visible effects as value entries; one drop or inner shadow becomes
    /// `box-shadow` (`text-shadow` for a TEXT drop shadow). Several
    /// shadows are unsupported: CSS lists them top-most first and Figma's
    /// effect order isn't verified. Blurs are unsupported (Figma's blur
    /// radius has no exact CSS `blur()`).
    fn effects(
        &mut self,
        n: &Value,
        id: &str,
        text: bool,
        vector: bool,
        css: &mut Css,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        let mut shadows = Vec::new();
        for (i, effect) in n["effects"].as_array().into_iter().flatten().enumerate() {
            if effect["visible"] == false {
                continue;
            }
            let ty = effect["type"].as_str().unwrap_or("unknown").to_string();
            let mut entry = Map::new();
            entry.insert("type".into(), json!(ty));
            let index = i.to_string();
            let mut field = |cx: &mut Self, name: &str, key: &str, raw: Value| -> Option<Value> {
                let binding = match cx.lookup(n, "", &["effects", &index, "boundVariables", key]) {
                    Ok(b) => b,
                    Err(p) => {
                        cx.unsupported(
                            id,
                            format!("{p}: bound to several variables (mixed): not specified"),
                        );
                        return None;
                    }
                };
                let e = if name == "color" {
                    let hex = color(&raw);
                    let lit = hex.clone().map(Lit::Color);
                    cx.tokens
                        .entry(binding, json!(hex), lit.as_ref(), EFFECT_COLOR)
                } else {
                    let lit = raw.as_f64().map(Lit::Number);
                    cx.tokens.entry(binding, raw, lit.as_ref(), EFFECT_FLOAT)
                };
                entry.insert(name.into(), e.clone());
                Some(e)
            };
            match ty.as_str() {
                "DROP_SHADOW" | "INNER_SHADOW" => {
                    let x = field(self, "offset_x", "offsetX", effect["offset"]["x"].clone());
                    let y = field(self, "offset_y", "offsetY", effect["offset"]["y"].clone());
                    let r = field(self, "radius", "radius", effect["radius"].clone());
                    let sp = field(
                        self,
                        "spread",
                        "spread",
                        effect.get("spread").cloned().unwrap_or(json!(0)),
                    );
                    let c = field(self, "color", "color", effect["color"].clone());
                    if vector {
                        out.push(Value::Object(entry));
                        continue;
                    }
                    let prop = if text { "text-shadow" } else { "box-shadow" };
                    let len = |cx: &mut Self, e: &Option<Value>| {
                        e.as_ref().and_then(|e| cx.css_value(id, prop, e, Want::Px))
                    };
                    let (x, y, r, sp) =
                        (len(self, &x), len(self, &y), len(self, &r), len(self, &sp));
                    let c = c.and_then(|e| self.css_value(id, prop, &e, Want::Color));
                    let why = if effect["showShadowBehindNode"] == true {
                        Some("showShadowBehindNode")
                    } else if effect["blendMode"]
                        .as_str()
                        .is_some_and(|m| !["NORMAL", "PASS_THROUGH"].contains(&m))
                    {
                        Some("blendMode")
                    } else if text && ty == "INNER_SHADOW" {
                        Some("inner shadow on text")
                    } else if text && sp.as_ref().is_some_and(|s| s.value != "0px") {
                        Some("spread on a text shadow")
                    } else {
                        None
                    };
                    if let Some(why) = why {
                        self.unsupported(id, format!("effects[{i}]: {ty} {why}: not specified"));
                    } else if let (Some(x), Some(y), Some(r), Some(sp), Some(c)) = (x, y, r, sp, c)
                    {
                        let inset = if ty == "INNER_SHADOW" { "inset " } else { "" };
                        shadows.push(if text {
                            format!("{} {} {} {}", x.value, y.value, r.value, c.value)
                        } else {
                            format!(
                                "{inset}{} {} {} {} {}",
                                x.value, y.value, r.value, sp.value, c.value
                            )
                        });
                    }
                }
                "LAYER_BLUR" | "BACKGROUND_BLUR" => {
                    field(self, "radius", "radius", effect["radius"].clone());
                    self.unsupported(
                        id,
                        format!("effects[{i}]: {ty}: Figma's blur radius has no exact CSS blur(): not specified"),
                    );
                }
                other => self.unsupported(id, format!("effects[{i}]: {other}: not specified")),
            }
            out.push(Value::Object(entry));
        }
        let prop = if text { "text-shadow" } else { "box-shadow" };
        match shadows.as_slice() {
            [] => {}
            [one] => css.lit(prop, one.clone()),
            many => self.unsupported(
                id,
                format!(
                    "effects: {} shadows: their stacking order is unverified: {prop} not specified",
                    many.len()
                ),
            ),
        }
        out
    }

    /// A TEXT node: typography, text CSS, `content.text` and mixed runs.
    fn text(
        &mut self,
        n: &Value,
        id: &str,
        values: &mut Map<String, Value>,
        content: &mut Map<String, Value>,
        css: &mut Css,
    ) {
        let runs = self.runs(n, id);
        let mixed = runs.is_some();
        let typography = self.typography(n, id, mixed);
        self.type_css(id, &n["style"], &typography, css);
        if !typography.is_empty() {
            values.insert("typography".into(), Value::Object(typography));
        }
        let style = &n["style"];
        let resize = style["textAutoResize"]
            .as_str()
            .or(n["textAutoResize"].as_str());
        // Figma keeps every newline and space: `pre-wrap`, or `pre` for
        // text that grows in width instead of wrapping.
        css.lit(
            "white-space",
            if resize == Some("WIDTH_AND_HEIGHT") {
                "pre"
            } else {
                "pre-wrap"
            },
        );
        if style["textTruncation"] == "ENDING" || resize == Some("TRUNCATE") {
            match style["maxLines"].as_u64().or(n["maxLines"].as_u64()) {
                Some(lines) if lines > 1 => {
                    css.lit("display", "-webkit-box");
                    css.lit("-webkit-box-orient", "vertical");
                    css.lit("-webkit-line-clamp", lines.to_string());
                    css.lit("overflow", "hidden");
                }
                _ => {
                    css.lit("overflow", "hidden");
                    css.lit("text-overflow", "ellipsis");
                    css.lit("white-space", "pre");
                }
            }
        }
        let hug_height = matches!(resize, Some("WIDTH_AND_HEIGHT" | "HEIGHT"))
            || n["layoutSizingVertical"] == "HUG";
        if let Some(v) = style["textAlignVertical"]
            .as_str()
            .filter(|v| *v != "TOP" && !hug_height)
        {
            self.unsupported(
                id,
                format!("textAlignVertical {v} in a fixed-height text box: not specified"),
            );
        }
        for key in ["paragraphSpacing", "paragraphIndent"] {
            if style[key].as_f64().is_some_and(|v| v != 0.0) {
                self.unsupported(id, format!("{key}: not specified"));
            }
        }
        if n["lineTypes"]
            .as_array()
            .is_some_and(|l| l.iter().any(|t| t != "NONE"))
        {
            self.unsupported(id, "list text (lineTypes): not specified");
        }
        content.insert("text".into(), n["characters"].clone());
        if let Ok(Some(bind)) = self.binding(n, id, "characters", false) {
            content.insert(
                "text_token".into(),
                self.tokens.bound(&bind, n["characters"].clone()),
            );
        }
        if let Some(runs) = runs {
            content.insert("runs".into(), Value::Array(runs));
        }
    }

    /// Mixed text runs: `None` unless `characterStyleOverrides` names a
    /// style other than the base. Offsets are UTF-16 code units, as
    /// Figma's own; a run splitting a surrogate pair is unsupported.
    fn runs(&mut self, n: &Value, id: &str) -> Option<Vec<Value>> {
        let overrides: Vec<u64> = n["characterStyleOverrides"]
            .as_array()?
            .iter()
            .map(|x| x.as_u64().unwrap_or(0))
            .collect();
        if overrides.iter().all(|o| *o == 0) {
            return None;
        }
        let units: Vec<u16> = n["characters"]
            .as_str()
            .unwrap_or_default()
            .encode_utf16()
            .collect();
        let style_at = |i: usize| overrides.get(i).copied().unwrap_or(0);
        let mut spans = Vec::new();
        let mut start = 0;
        for i in 1..=units.len() {
            if i == units.len() || style_at(i) != style_at(start) {
                spans.push((start, i, style_at(start)));
                start = i;
            }
        }
        let mut out = Vec::new();
        for (start, end, style) in spans {
            let Ok(text) = String::from_utf16(&units[start..end]) else {
                self.unsupported(
                    id,
                    "mixed text runs split a surrogate pair: runs not specified",
                );
                return None;
            };
            let mut run = Map::new();
            run.insert("start".into(), json!(start));
            run.insert("end".into(), json!(end));
            run.insert("text".into(), json!(text));
            run.insert("style".into(), json!(style));
            if style != 0 {
                let Some(o) = n["styleOverrideTable"].get(style.to_string()) else {
                    self.unsupported(
                        id,
                        format!("styleOverrideTable has no style {style}: run not specified"),
                    );
                    out.push(Value::Object(run));
                    continue;
                };
                let pseudo = json!({"style": o, "type": "TEXT"});
                let base = pointer("", &["styleOverrideTable", &style.to_string()]);
                self.style_base = base.clone();
                let typography = self.typography(&pseudo, id, true);
                self.style_base = "/style".into();
                let mut run_css = Css::default();
                self.type_css(id, o, &typography, &mut run_css);
                if !typography.is_empty() {
                    run.insert("typography".into(), Value::Object(typography));
                }
                let paint_node = json!({"fills": o["fills"].clone()});
                let mut assets = Vec::new();
                let fills = self.paints(
                    &paint_node,
                    &base,
                    id,
                    "fills",
                    fill_prop("TEXT"),
                    &mut assets,
                    Some(("color", &mut run_css)),
                );
                if !fills.is_empty() {
                    run.insert("fill".into(), Value::Array(fills));
                }
                if !run_css.is_empty() {
                    if self.tailwind {
                        run.insert("tailwind".into(), json!(run_css.tailwind()));
                    }
                    run.insert("css".into(), run_css.to_json());
                }
            }
            out.push(Value::Object(run));
        }
        Some(out)
    }

    /// A type style's family, size, weight, line height and letter
    /// spacing (only those it has, unless bound).
    fn typography(&mut self, n: &Value, id: &str, style_first: bool) -> Map<String, Value> {
        const FIELDS: [(&str, &str, &str, Prop); 5] = [
            (
                "font_family",
                "fontFamily",
                "fontFamily",
                Prop {
                    resolved_type: "STRING",
                    scopes: &["FONT_FAMILY"],
                },
            ),
            ("font_size", "fontSize", "fontSize", float(&["FONT_SIZE"])),
            (
                "font_weight",
                "fontWeight",
                "fontWeight",
                float(&["FONT_WEIGHT"]),
            ),
            (
                "line_height",
                "lineHeightPx",
                "lineHeight",
                float(&["LINE_HEIGHT"]),
            ),
            (
                "letter_spacing",
                "letterSpacing",
                "letterSpacing",
                float(&["LETTER_SPACING"]),
            ),
        ];
        let style = &n["style"];
        let mut out = Map::new();
        for (name, field, binding_key, prop) in FIELDS {
            let Ok(binding) = self.binding(n, id, binding_key, style_first) else {
                continue;
            };
            let auto_line = name == "line_height" && style["lineHeightUnit"] == "INTRINSIC_%";
            let raw = if auto_line {
                json!("auto")
            } else {
                match style.get(field) {
                    Some(raw) => raw.clone(),
                    None if binding.is_some() => Value::Null,
                    None => continue,
                }
            };
            let lit = match (&raw, auto_line) {
                (_, true) => None,
                (Value::String(s), _) => Some(Lit::Text(s.clone())),
                (raw, _) => raw.as_f64().map(Lit::Number),
            };
            out.insert(
                name.to_string(),
                self.tokens.entry(binding, raw, lit.as_ref(), prop),
            );
        }
        out
    }

    /// Text CSS from a type style and its typography entries.
    fn type_css(
        &mut self,
        id: &str,
        style: &Value,
        typography: &Map<String, Value>,
        css: &mut Css,
    ) {
        for (name, prop) in [
            ("font_family", "font-family"),
            ("font_size", "font-size"),
            ("font_weight", "font-weight"),
            ("line_height", "line-height"),
            ("letter_spacing", "letter-spacing"),
        ] {
            let Some(e) = typography.get(name) else {
                continue;
            };
            let v = match name {
                "font_family" => self.css_value(id, prop, e, Want::Family),
                "font_weight" => self.css_value(id, prop, e, Want::Number),
                "line_height" if e["raw"] == "auto" && e.get("variable_id").is_none() => {
                    Some(CssVal::lit("normal"))
                }
                "letter_spacing" => self.css_value(id, prop, e, Want::PxNonZero),
                _ => self.css_value(id, prop, e, Want::Px),
            };
            if let Some(v) = v {
                css.set(prop, v);
            }
        }
        if style["italic"] == true {
            css.lit("font-style", "italic");
        }
        match style["textAlignHorizontal"].as_str() {
            Some("CENTER") => css.lit("text-align", "center"),
            Some("RIGHT") => css.lit("text-align", "right"),
            Some("JUSTIFIED") => css.lit("text-align", "justify"),
            _ => {}
        }
        match style["textCase"].as_str() {
            Some("UPPER") => css.lit("text-transform", "uppercase"),
            Some("LOWER") => css.lit("text-transform", "lowercase"),
            Some("TITLE") => css.lit("text-transform", "capitalize"),
            Some("SMALL_CAPS") => css.lit("font-variant-caps", "small-caps"),
            Some("SMALL_CAPS_FORCED") => css.lit("font-variant-caps", "all-small-caps"),
            _ => {}
        }
        match style["textDecoration"].as_str() {
            Some("UNDERLINE") => css.lit("text-decoration-line", "underline"),
            Some("STRIKETHROUGH") => css.lit("text-decoration-line", "line-through"),
            _ => {}
        }
    }

    /// An INSTANCE: its component, variant and other properties, and the
    /// code component the project's map names (NWS-2949), else `null`.
    fn instance(&mut self, n: &Value, id: &str) -> Value {
        let component_id = n["componentId"].as_str().unwrap_or_default();
        let component = &self.components[component_id];
        let mut variant = Map::new();
        let mut properties = Map::new();
        for (name, p) in n["componentProperties"].as_object().into_iter().flatten() {
            if p["type"] == "VARIANT" {
                variant.insert(name.clone(), p["value"].clone());
                continue;
            }
            let mut prop = Map::new();
            prop.insert("type".into(), p["type"].clone());
            prop.insert("value".into(), p["value"].clone());
            let own = self.lookup(
                n,
                "",
                &["componentProperties", name, "boundVariables", "value"],
            );
            let node = self.lookup(n, "", &["boundVariables", "componentProperties", name]);
            if let Ok(Some(bind)) = self.either(id, own, node) {
                prop.insert("token".into(), self.tokens.bound(&bind, p["value"].clone()));
            }
            properties.insert(name.clone(), Value::Object(prop));
        }
        if component.is_null() {
            self.unsupported(
                id,
                format!("component {component_id} not in the mirror: component name not specified"),
            );
        }
        json!({
            "component_id": component_id,
            "component": if component.is_null() { Value::Null } else {
                json!({
                    "name": component["name"],
                    "key": component["key"],
                    "remote": component["remote"],
                })
            },
            "component_set": component["component_set"],
            "variant": variant,
            "properties": properties,
            "code_component": crate::map::code_component(self.map, n, component, self.components),
        })
    }
}

/// `"hug"`, `"fill"` or `"fixed"` along `axis`. `layoutSizing*` when Figma
/// sent it; otherwise the older fields: `layoutGrow`/`layoutAlign` against
/// the parent's direction (fill), the node's own auto-layout sizing modes
/// (hug), a TEXT node's `textAutoResize` (hug), else fixed.
fn sizing(n: &Value, axis: &str, own: Option<&str>, parent: Option<&str>) -> String {
    let field = if axis == "HORIZONTAL" {
        "layoutSizingHorizontal"
    } else {
        "layoutSizingVertical"
    };
    if let Some(s) = n[field].as_str() {
        return s.to_ascii_lowercase();
    }
    match parent {
        Some(p) if p == axis && n["layoutGrow"].as_f64().is_some_and(|g| g > 0.0) => {
            return "fill".into();
        }
        Some(p) if p != axis && n["layoutAlign"] == "STRETCH" => return "fill".into(),
        _ => {}
    }
    if let Some(d) = own {
        let mode = if d == axis {
            &n["primaryAxisSizingMode"]
        } else {
            &n["counterAxisSizingMode"]
        };
        return if lower(mode, "AUTO") == "auto" {
            "hug".into()
        } else {
            "fixed".into()
        };
    }
    if n["type"] == "TEXT" {
        let hug = match n["style"]["textAutoResize"]
            .as_str()
            .or(n["textAutoResize"].as_str())
        {
            Some("WIDTH_AND_HEIGHT") => true,
            Some("HEIGHT") => axis == "VERTICAL",
            _ => false,
        };
        if hug {
            return "hug".into();
        }
    }
    "fixed".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(tree: &Value) -> Value {
        let vars = json!({"snapshot": {"state": "none"}, "variables": []});
        build(
            tree,
            &vars,
            &json!({}),
            &ExportOptions::default(),
            false,
            None,
        )
    }

    #[test]
    fn legacy_sizing_fields() {
        let row = json!({"type": "FRAME", "layoutMode": "HORIZONTAL",
            "primaryAxisSizingMode": "FIXED", "layoutAlign": "STRETCH"});
        // Child of a vertical parent: stretch fills horizontally.
        assert_eq!(
            sizing(&row, "HORIZONTAL", Some("HORIZONTAL"), Some("VERTICAL")),
            "fill"
        );
        // Counter axis defaults to AUTO: hug.
        assert_eq!(
            sizing(&row, "VERTICAL", Some("HORIZONTAL"), Some("VERTICAL")),
            "hug"
        );
        // Without the parent, the fixed primary axis stays fixed.
        assert_eq!(
            sizing(&row, "HORIZONTAL", Some("HORIZONTAL"), None),
            "fixed"
        );
        let text = json!({"type": "TEXT", "layoutGrow": 1, "style": {"textAutoResize": "HEIGHT"}});
        assert_eq!(
            sizing(&text, "HORIZONTAL", None, Some("HORIZONTAL")),
            "fill"
        );
        assert_eq!(sizing(&text, "VERTICAL", None, Some("HORIZONTAL")), "hug");
        // Modern fields win.
        let modern = json!({"layoutSizingHorizontal": "HUG", "layoutGrow": 1});
        assert_eq!(
            sizing(&modern, "HORIZONTAL", None, Some("HORIZONTAL")),
            "hug"
        );
    }

    #[test]
    fn alias_arrays_name_one_variable_or_are_mixed() {
        let a = json!({"type": "VARIABLE_ALIAS", "id": "V:1"});
        let b = json!({"type": "VARIABLE_ALIAS", "id": "V:2"});
        assert_eq!(alias_id(Some(&a)), Ok(Some("V:1".into())));
        assert_eq!(alias_id(Some(&json!([a, a]))), Ok(Some("V:1".into())));
        assert_eq!(alias_id(Some(&json!([a, b]))), Err(()));
        assert_eq!(alias_id(None), Ok(None));
    }

    #[test]
    fn a_mixed_paint_binding_is_unsupported_not_untokenized() {
        // The paint's own binding is mixed; the node-level fallback must not
        // paper over it (it would read as `Ok(None)` → untokenized).
        let tree = json!({"id": "1:1", "name": "Box", "type": "RECTANGLE",
            "absoluteBoundingBox": {"x": 0, "y": 0, "width": 10, "height": 10},
            "fills": [{"type": "SOLID", "color": {"r": 0, "g": 0, "b": 0, "a": 1},
                "boundVariables": {"color": [
                    {"type": "VARIABLE_ALIAS", "id": "V:1"},
                    {"type": "VARIABLE_ALIAS", "id": "V:2"}]}}],
            "children": []});
        let out = spec(&tree);
        assert!(out["node"]["values"].get("fill").is_none(), "{out}");
        assert_eq!(
            out["unsupported"],
            json!([{"id": "1:1", "reason": "/fills/0/boundVariables/color: bound to several variables (mixed): not specified"}])
        );
        // A paint without its own binding still falls back to the node's.
        let mut tree = tree;
        tree["fills"][0]
            .as_object_mut()
            .unwrap()
            .remove("boundVariables");
        tree["boundVariables"] = json!({"fills": [{"type": "VARIABLE_ALIAS", "id": "V:3"}]});
        let out = spec(&tree);
        assert_eq!(out["node"]["values"]["fill"][0]["variable_id"], "V:3");
        assert_eq!(out["unsupported"], json!([]));
    }

    #[test]
    fn paint_opacity_folds_into_alpha() {
        let paint =
            json!({"type": "SOLID", "opacity": 0.5, "color": {"r": 0, "g": 0, "b": 0, "a": 1}});
        assert_eq!(paint_color(&paint).as_deref(), Some("#00000080"));
    }

    #[test]
    fn without_a_snapshot_bindings_still_show_and_nothing_matches() {
        let tree = json!({"id": "1:1", "name": "Card", "type": "FRAME",
            "layoutMode": "VERTICAL", "itemSpacing": 8,
            "boundVariables": {"itemSpacing": {"type": "VARIABLE_ALIAS", "id": "V:9"}},
            "children": []});
        let out = spec(&tree);
        assert_eq!(
            out["node"]["layout"]["gap"],
            json!({"token": null, "variable_id": "V:9", "css": null, "raw": 8,
                "bound_at": ["/boundVariables/itemSpacing"]})
        );
        assert_eq!(
            out["node"]["layout"]["padding"]["top"],
            json!({"untokenized": true, "raw": 0})
        );
        // No CSS name: the bound gap is written raw.
        assert_eq!(out["node"]["css"]["gap"], "8px");
        assert!(
            out["tokens_snapshot"]["css_unavailable"]
                .as_str()
                .unwrap()
                .contains("state: none")
        );
    }

    #[test]
    fn negative_spacing_becomes_child_margins() {
        let kid = |id: &str| {
            json!({"id": id, "type": "RECTANGLE", "layoutSizingHorizontal": "FIXED",
            "layoutSizingVertical": "FIXED",
            "absoluteBoundingBox": {"x": 0, "y": 0, "width": 10, "height": 10}})
        };
        let tree = json!({"id": "1:1", "type": "FRAME", "layoutMode": "HORIZONTAL",
            "itemSpacing": -4, "children": [kid("1:2"), kid("1:3")]});
        let out = spec(&tree);
        let node = &out["node"];
        assert!(node["css"].get("gap").is_none(), "{node}");
        assert!(node["children"][0]["css"].get("margin-left").is_none());
        assert_eq!(node["children"][1]["css"]["margin-left"], "-4px");
        assert_eq!(out["unsupported"], json!([]));
        // Wrapping: no exact CSS, listed.
        let mut wrapped = tree.clone();
        wrapped["layoutWrap"] = json!("WRAP");
        let out = spec(&wrapped);
        assert!(
            out["unsupported"]
                .as_array()
                .unwrap()
                .contains(&json!({"id": "1:1",
                "reason": "itemSpacing: negative gap in a wrapping frame: gap not specified"})),
            "{out}"
        );
    }

    #[test]
    fn constraints_place_children_of_a_frame_without_auto_layout() {
        let child = |id: &str, h: &str, v: &str, x: i64, y: i64| {
            json!({"id": id, "type": "RECTANGLE",
                "constraints": {"horizontal": h, "vertical": v},
                "absoluteBoundingBox": {"x": x, "y": y, "width": 20, "height": 10}})
        };
        let tree = json!({"id": "1:1", "type": "FRAME", "clipsContent": true,
        "absoluteBoundingBox": {"x": 100, "y": 100, "width": 200, "height": 100},
        "children": [
            child("1:2", "RIGHT", "BOTTOM", 270, 180),
            child("1:3", "CENTER", "TOP_BOTTOM", 190, 110),
            child("1:4", "SCALE", "CENTER", 150, 145),
        ]});
        let out = spec(&tree);
        let n = &out["node"];
        assert_eq!(n["css"]["position"], "relative");
        assert_eq!(n["css"]["overflow"], "hidden");
        assert_eq!(
            n["children"][0]["css"],
            json!({"position": "absolute", "right": "10px", "bottom": "10px",
                "width": "20px", "height": "10px"})
        );
        assert_eq!(
            n["children"][1]["css"],
            json!({"position": "absolute", "left": "50%",
                "top": "10px", "bottom": "80px", "width": "20px",
                "transform": "translateX(-50%)"})
        );
        assert_eq!(
            n["children"][2]["css"],
            json!({"position": "absolute", "left": "25%", "width": "10%",
                "top": "50%", "height": "10px", "transform": "translateY(-50%)"})
        );
    }

    #[test]
    fn grid_tracks_gaps_and_spans() {
        let tree = json!({"id": "1:1", "type": "FRAME", "layoutMode": "GRID",
            "absoluteBoundingBox": {"x": 0, "y": 0, "width": 10, "height": 10},
            "gridColumnCount": 2, "gridRowCount": 1, "gridColumnGap": 8, "gridRowGap": 0,
            "gridColumnsSizing": "1fr 1fr",
            "children": [{"id": "1:2", "type": "RECTANGLE", "gridColumnSpan": 2,
                "gridColumnAnchorIndex": 0, "gridRowAnchorIndex": 0,
                "layoutSizingHorizontal": "FILL", "layoutSizingVertical": "FILL"}]});
        let out = spec(&tree);
        let n = &out["node"];
        assert_eq!(n["css"]["display"], "grid");
        assert_eq!(n["css"]["grid-template-columns"], "1fr 1fr");
        assert_eq!(n["css"]["column-gap"], "8px");
        assert_eq!(n["layout"]["grid"]["columns"], 2);
        assert_eq!(
            n["children"][0]["css"],
            json!({"grid-column": "1 / span 2", "grid-row": "1 / span 1",
                "justify-self": "stretch", "align-self": "stretch"})
        );
        assert_eq!(
            out["unsupported"],
            json!([{"id": "1:1", "reason": "gridRowsSizing not recorded: grid-template-rows not specified"}])
        );
    }

    #[test]
    fn unread_bindings_are_listed() {
        let tree = json!({"id": "1:1", "type": "TEXT",
            "style": {"textAutoResize": "WIDTH_AND_HEIGHT"},
            "boundVariables": {"visible": {"type": "VARIABLE_ALIAS", "id": "V:1"}}});
        assert_eq!(
            spec(&tree)["unsupported"],
            json!([{"id": "1:1", "reason": "/boundVariables/visible: bound to a variable: not specified"}])
        );
    }

    /// A snapshot with one unitless (ALL_SCOPES) FLOAT, one `%` (OPACITY)
    /// FLOAT and one px (GAP) FLOAT.
    fn unit_vars() -> Value {
        let row = |id: &str, name: &str, scopes: Value, v: f64| {
            json!({"source": "imported", "variable_id": id, "name": name,
                "resolved_type": "FLOAT", "collection": "C", "collection_id": "C:1",
                "default_mode": "M", "scopes": scopes, "values_by_mode": {"M": v}})
        };
        json!({
            "snapshot": {"state": "current", "source": "file", "format": "bare",
                "format_version": 1, "exported_at": "2026-01-01T00:00:00Z",
                "digest": "fnv1a64:0", "variables": 3, "collections": 1},
            "collections": [{"collection_id": "C:1", "name": "C", "modes": ["M"],
                "default_mode": "M", "source": "imported"}],
            "variables": [
                row("V:1", "space/raw", json!(["ALL_SCOPES"]), 12.0),
                row("V:2", "fade", json!(["OPACITY"]), 50.0),
                row("V:3", "space/px", json!(["GAP"]), 4.0),
            ],
        })
    }

    #[test]
    fn unitless_tokens_get_units_in_css_and_mismatches_are_listed() {
        let alias = |v: &str| json!({"type": "VARIABLE_ALIAS", "id": v});
        let tree = json!({"id": "1:1", "type": "FRAME", "layoutMode": "VERTICAL",
            "paddingTop": 12, "paddingLeft": 4, "opacity": 0.5, "itemSpacing": 50,
            "absoluteBoundingBox": {"x": 0, "y": 0, "width": 10, "height": 10},
            "boundVariables": {"paddingTop": alias("V:1"), "paddingLeft": alias("V:3"),
                "opacity": alias("V:2"), "itemSpacing": alias("V:2")},
            "children": []});
        let out = build(
            &tree,
            &unit_vars(),
            &json!({}),
            &ExportOptions::default(),
            true,
            None,
        );
        assert_eq!(
            out["tokens_snapshot"]["css_unavailable"],
            Value::Null,
            "{out}"
        );
        let css = &out["node"]["css"];
        assert_eq!(css["padding-top"], "calc(var(--space-raw) * 1px)");
        assert_eq!(css["padding-left"], "var(--space-px)");
        assert_eq!(css["opacity"], "var(--fade)");
        assert!(css.get("gap").is_none(), "{css}");
        assert!(
            out["unsupported"].as_array().unwrap().contains(&json!({"id": "1:1",
                "reason": "gap: token --fade is exported in %, which gap can't take: not specified"})),
            "{out}"
        );
        // A unitless opacity token is a 0–100 number.
        let mut tree = tree;
        tree["boundVariables"]["opacity"] = alias("V:1");
        let out = build(
            &tree,
            &unit_vars(),
            &json!({}),
            &ExportOptions::default(),
            false,
            None,
        );
        assert_eq!(
            out["node"]["css"]["opacity"],
            "calc(var(--space-raw) / 100)"
        );
    }

    #[test]
    fn bindings_css_cant_use_are_still_token_references() {
        let alias = |v: &str| json!({"type": "VARIABLE_ALIAS", "id": v});
        let tree = json!({"id": "1:1", "type": "FRAME", "layoutMode": "HORIZONTAL",
            "primaryAxisAlignItems": "SPACE_BETWEEN", "itemSpacing": 8,
            "absoluteBoundingBox": {"x": 0, "y": 0, "width": 100, "height": 100},
            "layoutSizingHorizontal": "FIXED", "layoutSizingVertical": "FIXED",
            "boundVariables": {"itemSpacing": alias("V:3")},
            "children": [{"id": "1:2", "type": "RECTANGLE", "layoutPositioning": "ABSOLUTE",
                "constraints": {"horizontal": "LEFT_RIGHT", "vertical": "TOP"},
                "absoluteBoundingBox": {"x": 10, "y": 10, "width": 80, "height": 10},
                "boundVariables": {"size": {"x": alias("V:3")}}}]});
        let out = build(
            &tree,
            &unit_vars(),
            &json!({}),
            &ExportOptions::default(),
            false,
            None,
        );
        let node = &out["node"];
        assert_eq!(node["layout"]["gap"], "auto");
        assert_eq!(node["layout"]["item_spacing"]["variable_id"], "V:3");
        assert_eq!(node["children"][0]["layout"]["width"]["variable_id"], "V:3");
        assert_eq!(
            node["children"][0]["layout"]["width"]["bound_at"],
            json!(["/boundVariables/size/x"])
        );
        let reasons: Vec<&str> = out["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["reason"].as_str().unwrap())
            .collect();
        assert!(
            reasons
                .iter()
                .any(|r| r.starts_with("/boundVariables/itemSpacing: itemSpacing is bound")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|r| r.starts_with("/boundVariables/size/x: width is bound")),
            "{reasons:?}"
        );
    }

    #[test]
    fn absolute_children_inside_a_border_are_offset_from_the_padding_box() {
        let tree = json!({"id": "1:1", "type": "FRAME", "layoutMode": "HORIZONTAL",
            "strokes": [{"type": "SOLID", "color": {"r": 0, "g": 0, "b": 0, "a": 1}}],
            "strokeWeight": 2, "strokeAlign": "INSIDE", "strokesIncludedInLayout": true,
            "layoutSizingHorizontal": "FIXED", "layoutSizingVertical": "FIXED",
            "absoluteBoundingBox": {"x": 0, "y": 0, "width": 100, "height": 50},
            "children": [{"id": "1:2", "type": "RECTANGLE", "layoutPositioning": "ABSOLUTE",
                "constraints": {"horizontal": "RIGHT", "vertical": "SCALE"},
                "absoluteBoundingBox": {"x": 80, "y": 10, "width": 10, "height": 10}}]});
        let out = spec(&tree);
        assert_eq!(out["node"]["css"]["border-width"], "2px");
        let child = &out["node"]["children"][0];
        assert_eq!(child["css"]["right"], "8px");
        assert!(
            out["unsupported"]
                .as_array()
                .unwrap()
                .iter()
                .any(|u| u["reason"]
                    .as_str()
                    .unwrap()
                    .starts_with("constraints.vertical SCALE inside a border")),
            "{out}"
        );
    }

    #[test]
    fn several_shadows_are_unsupported_one_is_css() {
        let shadow = |y: i64| {
            json!({"type": "DROP_SHADOW", "visible": true, "blendMode": "NORMAL",
            "color": {"r": 0, "g": 0, "b": 0, "a": 1}, "offset": {"x": 0, "y": y},
            "radius": 2, "spread": 0, "showShadowBehindNode": false})
        };
        let mut tree = json!({"id": "1:1", "type": "RECTANGLE",
            "absoluteBoundingBox": {"x": 0, "y": 0, "width": 10, "height": 10},
            "effects": [shadow(1)]});
        assert_eq!(
            spec(&tree)["node"]["css"]["box-shadow"],
            "0px 1px 2px 0px #000000"
        );
        tree["effects"] = json!([shadow(1), shadow(2)]);
        let out = spec(&tree);
        assert!(out["node"]["css"].get("box-shadow").is_none());
        assert_eq!(
            out["node"]["values"]["effects"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            out["unsupported"][0]["reason"],
            "effects: 2 shadows: their stacking order is unverified: box-shadow not specified"
        );
    }
}
