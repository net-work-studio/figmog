//! `figmog spec <node>` / `figmog_spec` (NWS-2968): a binding-aware spec
//! of one auto-layout subtree — layout (direction, gap, padding,
//! alignment, sizing) and values (fill, stroke, radius, typography), each
//! value shown as the variable token it is bound to, or flagged as
//! untokenized (naming any token whose default/light value it equals).
//!
//! Pure: the inputs are exactly the `figmog_subtree` answer for the node
//! (raw JSON, children nested) and the `figmog_vars {snapshot: true}`
//! answer, so the output is a function of the mirror alone. A bound
//! token's `css` name comes from [`crate::tokens::export_css`] run on the
//! same snapshot, so it is the name `figmog tokens export` emits, never a
//! parallel naming rule.
//!
//! Anything outside the slice — grid or wrapping layout, absolutely
//! positioned children, constraints-based frames, mixed text runs,
//! non-solid paints, effects, layer opacity, hidden layers — is listed
//! under `unsupported` with the node id and reason and left out of the
//! spec, never approximated (SPEC §6 "`figmog_spec`").
//!
//! Determinism: every object is a `serde_json::Map` (sorted keys, no
//! `preserve_order`), `unsupported` is a sorted set, children keep
//! child-index order, and `matches` lists are sorted by token name then
//! variable id.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::deep_json::OwnedValue;
use crate::tokens::{ExportOptions, Theme, color, export_css};

/// One authoritative variable row of `figmog vars --snapshot`.
struct Var<'a> {
    name: String,
    resolved_type: String,
    collection_id: String,
    default_mode_id: Option<String>,
    scopes: Vec<String>,
    /// Mode ID → raw value or alias.
    values: BTreeMap<String, &'a Value>,
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
struct Tokens<'a> {
    vars: BTreeMap<String, Var<'a>>,
    /// Variable id → custom property name, from the token export.
    css: BTreeMap<String, String>,
    /// Variable id → its value in the root mode (the theme collection's
    /// light mode, else the collection's default mode), aliases followed.
    root: BTreeMap<String, Lit>,
}

/// What a property accepts: the variable type and the Figma scopes that
/// make a variable eligible for it (`ALL_SCOPES` always does).
#[derive(Clone, Copy)]
struct Prop {
    resolved_type: &'static str,
    scopes: &'static [&'static str],
}

const GAP: Prop = Prop {
    resolved_type: "FLOAT",
    scopes: &["GAP"],
};
const RADIUS: Prop = Prop {
    resolved_type: "FLOAT",
    scopes: &["CORNER_RADIUS"],
};
const STROKE_COLOR: Prop = Prop {
    resolved_type: "COLOR",
    scopes: &["STROKE_COLOR"],
};
const STROKE_WEIGHT: Prop = Prop {
    resolved_type: "FLOAT",
    scopes: &["STROKE_FLOAT"],
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

/// Build the spec. `tree` is the `figmog_subtree` answer for the node;
/// `vars` the `figmog_vars {snapshot: true}` answer.
pub fn build(tree: &Value, vars: &Value) -> Value {
    let (css, theme, css_unavailable) = match export_css(vars, &ExportOptions::default()) {
        Ok(export) => (export.names, export.theme, Value::Null),
        Err(e) => (BTreeMap::new(), None, Value::String(e)),
    };
    let tokens = Tokens::new(vars, css, theme.as_ref());
    let mut cx = Cx {
        tokens: &tokens,
        unsupported: BTreeSet::new(),
    };
    let node = cx.tree(tree);
    let unsupported: Vec<Value> = cx
        .unsupported
        .into_iter()
        .map(|(id, reason)| json!({"id": id, "reason": reason}))
        .collect();
    let mut snapshot = if vars["snapshot"].is_object() {
        copy_value(&vars["snapshot"])
    } else {
        OwnedValue::object()
    };
    snapshot.insert("css_unavailable".into(), OwnedValue(css_unavailable));
    snapshot.insert("theme".into(), OwnedValue(json!(theme)));
    let mut result = OwnedValue(json!({"unsupported": unsupported}));
    result.insert("node".into(), node);
    result.insert("tokens_snapshot".into(), snapshot);
    result.into_inner()
}

/// Raw properties may contain arbitrary JSON, including malformed values at
/// otherwise scalar fields. Keep their copies guarded and use the growing-stack
/// boundary for containers instead of `Value::clone`'s recursive implementation.
fn copy_value(value: &Value) -> OwnedValue {
    match value {
        Value::Array(_) | Value::Object(_) => crate::deep_json::from_str(
            &crate::deep_json::to_string(value).expect("serialize raw spec property"),
        )
        .expect("parse serialized raw spec property"),
        _ => OwnedValue(value.clone()),
    }
}

impl<'a> Tokens<'a> {
    fn new(vars: &'a Value, css: BTreeMap<String, String>, theme: Option<&Theme>) -> Tokens<'a> {
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
                    collection_id: text("collection_id"),
                    default_mode_id: row["default_mode_id"].as_str().map(str::to_string),
                    scopes: row["scopes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    values: row["values_by_mode_id"]
                        .as_object()
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), v)).collect())
                        .unwrap_or_default(),
                },
            );
        }
        let mut resolved = BTreeMap::new();
        let root = all
            .keys()
            .filter_map(|id| Some((id.clone(), root_value(&all, theme, id, &mut resolved)?)))
            .collect();
        Tokens {
            vars: all,
            css,
            root,
        }
    }

    /// A bound value: the token, its CSS name and the raw value.
    fn bound(&self, variable_id: &str, raw: OwnedValue) -> OwnedValue {
        let mut out = OwnedValue(json!({
            "token": self.vars.get(variable_id).map(|v| v.name.clone()),
            "variable_id": variable_id,
            "css": self.css.get(variable_id),
        }));
        out.insert("raw".into(), raw);
        out
    }

    /// An unbound value: flagged untokenized, plus every eligible token
    /// whose root value equals it.
    fn untokenized(&self, raw: OwnedValue, lit: Option<&Lit>, prop: Prop) -> OwnedValue {
        let mut out = OwnedValue(json!({"untokenized": true}));
        out.insert("raw".into(), raw);
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
            let matches = matches
                .into_iter()
                .map(
                    |(name, id)| json!({"token": name, "variable_id": id, "css": self.css.get(id)}),
                )
                .collect();
            out.insert("matches".into(), OwnedValue(Value::Array(matches)));
        }
        out
    }

    /// The value entry for `raw` with optional binding `variable_id`.
    fn entry(
        &self,
        binding: Option<String>,
        raw: OwnedValue,
        lit: Option<&Lit>,
        prop: Prop,
    ) -> OwnedValue {
        match binding {
            Some(id) => self.bound(&id, raw),
            None => self.untokenized(raw, lit, prop),
        }
    }
}

/// A variable's value in its root mode. Follow aliases without a depth
/// limit, rejecting actual cycles and memoizing both values and failures
/// so shared alias tails are resolved only once.
fn root_value(
    all: &BTreeMap<String, Var<'_>>,
    theme: Option<&Theme>,
    id: &str,
    resolved: &mut BTreeMap<String, Option<Lit>>,
) -> Option<Lit> {
    let mut current = id;
    let mut visited = BTreeSet::new();
    let mut path = Vec::new();
    let value = loop {
        if let Some(value) = resolved.get(current) {
            break value.clone();
        }
        if !visited.insert(current) {
            break None;
        }
        path.push(current.to_string());
        let Some(v) = all.get(current) else {
            break None;
        };
        let mode = match theme {
            Some(t) if t.collection_id == v.collection_id => Some(&t.light_mode_id),
            _ => v.default_mode_id.as_ref(),
        };
        let Some(raw) = mode.and_then(|mode| v.values.get(mode)) else {
            break None;
        };
        if raw["type"] == "VARIABLE_ALIAS" {
            let Some(target) = raw["id"].as_str() else {
                break None;
            };
            current = target;
            continue;
        }
        break match v.resolved_type.as_str() {
            "COLOR" => color(raw).map(Lit::Color),
            "FLOAT" => raw.as_f64().map(Lit::Number),
            "STRING" => raw.as_str().map(|s| Lit::Text(s.to_string())),
            _ => None,
        };
    };
    for id in path {
        resolved.insert(id, value.clone());
    }
    value
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

/// Flat bindings remain supported, while REST also nests corner bindings
/// under rectangleCornerRadii and uniform stroke bindings under its sides.
/// Two representations may agree; conflicting or partially bound uniform
/// strokes cannot be represented as one binding.
fn node_binding(n: &Value, key: &str) -> Result<Option<String>, ()> {
    let bindings = n.get("boundVariables");
    let flat = alias_id(bindings.and_then(|b| b.get(key)))?;
    let nested = if key == "strokeWeight" {
        match bindings.and_then(|b| b.get("individualStrokeWeights")) {
            None => None,
            Some(sides) => {
                let mut variable = None;
                for side in ["top", "right", "bottom", "left"] {
                    let current = alias_id(sides.get(side))?.ok_or(())?;
                    if variable
                        .as_ref()
                        .is_some_and(|previous| previous != &current)
                    {
                        return Err(());
                    }
                    variable = Some(current);
                }
                variable
            }
        }
    } else {
        let corner = match key {
            "topLeftRadius" => Some("RECTANGLE_TOP_LEFT_CORNER_RADIUS"),
            "topRightRadius" => Some("RECTANGLE_TOP_RIGHT_CORNER_RADIUS"),
            "bottomRightRadius" => Some("RECTANGLE_BOTTOM_RIGHT_CORNER_RADIUS"),
            "bottomLeftRadius" => Some("RECTANGLE_BOTTOM_LEFT_CORNER_RADIUS"),
            _ => None,
        };
        alias_id(corner.and_then(|corner| {
            bindings
                .and_then(|b| b.get("rectangleCornerRadii"))
                .and_then(|radii| radii.get(corner))
        }))?
    };
    match (flat, nested) {
        (Some(flat), Some(nested)) if flat != nested => Err(()),
        (Some(id), _) | (_, Some(id)) => Ok(Some(id)),
        _ => Ok(None),
    }
}

/// The node's layout parent, as far as sizing is concerned.
#[derive(Clone, Copy, PartialEq)]
enum Parent {
    /// The spec's root: its parent is outside the spec.
    Root,
    /// An auto-layout parent with this direction.
    Auto(&'static str),
    /// A parent whose layout isn't specified (none, grid, wrap).
    Other,
}

struct Cx<'a, 'vars> {
    tokens: &'a Tokens<'vars>,
    unsupported: BTreeSet<(String, String)>,
}

/// `SOLID` paint color as `#rrggbb[aa]`, the paint's opacity folded into
/// alpha — the same rendering `figmog tokens export` uses for a COLOR.
fn paint_color(paint: &Value) -> Option<String> {
    let c = paint.get("color")?;
    let opacity = paint.get("opacity").and_then(Value::as_f64).unwrap_or(1.0);
    let a = c.get("a").and_then(Value::as_f64).unwrap_or(1.0);
    // Only channels influence the color; unrelated, potentially deep paint
    // properties never need to be copied.
    color(&json!({
        "r": c.get("r")?.as_f64()?,
        "g": c.get("g")?.as_f64()?,
        "b": c.get("b")?.as_f64()?,
        "a": a * opacity,
    }))
}

fn lower(v: &Value, default: &str) -> String {
    v.as_str().unwrap_or(default).to_ascii_lowercase()
}

impl Cx<'_, '_> {
    fn unsupported(&mut self, id: &str, reason: impl Into<String>) {
        self.unsupported.insert((id.to_string(), reason.into()));
    }

    /// A node's binding for `key` (node `boundVariables`, then — for text
    /// properties — the type style's own), reporting a mixed binding.
    fn binding(&mut self, n: &Value, id: &str, key: &str) -> Result<Option<String>, ()> {
        let found = node_binding(n, key).and_then(|b| match b {
            Some(b) => Ok(Some(b)),
            None => alias_id(
                n.get("style")
                    .and_then(|s| s.get("boundVariables"))
                    .and_then(|b| b.get(key)),
            ),
        });
        if found.is_err() {
            let reason = if key == "strokeWeight"
                && n["boundVariables"].get("individualStrokeWeights").is_some()
            {
                "strokeWeight: mixed or partially bound sides: not specified".into()
            } else {
                format!("{key}: bound to several variables (mixed): not specified")
            };
            self.unsupported(id, reason);
        }
        found
    }

    /// A numeric property: bound or untokenized, raw value as stored.
    fn number(
        &mut self,
        n: &Value,
        id: &str,
        key: &str,
        raw: OwnedValue,
        prop: Prop,
    ) -> Option<OwnedValue> {
        let binding = self.binding(n, id, key).ok()?;
        let lit = raw.as_f64().map(Lit::Number);
        Some(self.tokens.entry(binding, raw, lit.as_ref(), prop))
    }

    /// Visit in child order and assemble children in postorder. Both pending
    /// and finished outputs remain guarded if construction exits early.
    fn tree(&mut self, root: &Value) -> OwnedValue {
        enum Task<'a> {
            Visit(&'a Value, Parent),
            Finish(OwnedValue, usize),
        }
        let mut pending = vec![Task::Visit(root, Parent::Root)];
        let mut finished = Vec::new();
        while let Some(task) = pending.pop() {
            match task {
                Task::Visit(n, parent) => {
                    let (out, child_parent) = self.node(n, parent);
                    if let Some(child_parent) = child_parent {
                        let kids = n["children"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                        pending.push(Task::Finish(out, kids.len()));
                        pending.extend(kids.iter().rev().map(|n| Task::Visit(n, child_parent)));
                    } else {
                        finished.push(out);
                    }
                }
                Task::Finish(mut out, count) => {
                    let mut children = OwnedValue::array();
                    for child in finished.drain(finished.len() - count..) {
                        children.push(child);
                    }
                    out.insert("children".into(), children);
                    finished.push(out);
                }
            }
        }
        finished.pop().expect("one root")
    }

    /// One node's own fields and its children's layout context. Hidden layers
    /// omit their descendants, matching the recursive traversal's contract.
    fn node(&mut self, n: &Value, parent: Parent) -> (OwnedValue, Option<Parent>) {
        let id = n["id"].as_str().unwrap_or_default().to_string();
        let node_type = n["type"].as_str().unwrap_or_default().to_string();
        let mut out = OwnedValue::object();
        out.insert("id".into(), OwnedValue(json!(id)));
        out.insert("name".into(), copy_value(&n["name"]));
        out.insert("type".into(), OwnedValue(json!(node_type)));
        if n["visible"] == false {
            self.unsupported(&id, "hidden layer (visible: false): not specified");
            return (out, None);
        }
        if let Some(o) = n["opacity"].as_f64().filter(|o| *o < 1.0) {
            self.unsupported(&id, format!("layer opacity {o}: not specified"));
        }
        let effects: BTreeSet<&str> = n["effects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["visible"] != false)
            .map(|e| e["type"].as_str().unwrap_or("unknown"))
            .collect();
        if !effects.is_empty() {
            let list: Vec<&str> = effects.into_iter().collect();
            self.unsupported(&id, format!("effects ({}): not specified", list.join(", ")));
        }

        // Layout.
        let mode = n["layoutMode"].as_str().unwrap_or("NONE");
        let kids = n["children"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let mut dir = match mode {
            "HORIZONTAL" => Some("HORIZONTAL"),
            "VERTICAL" => Some("VERTICAL"),
            "NONE" => {
                if !kids.is_empty() {
                    self.unsupported(
                        &id,
                        "no auto layout (constraints-based frame): child positions not specified",
                    );
                }
                None
            }
            other => {
                self.unsupported(
                    &id,
                    format!("layoutMode {other}: own layout and child positions not specified"),
                );
                None
            }
        };
        if dir.is_some() && n["layoutWrap"] == "WRAP" {
            self.unsupported(
                &id,
                "layoutWrap WRAP: own layout and child positions not specified",
            );
            dir = None;
        }
        let absolute = n["layoutPositioning"] == "ABSOLUTE";
        if absolute {
            self.unsupported(
                &id,
                "layoutPositioning ABSOLUTE: position and sizing not specified",
            );
        }
        let mut layout = OwnedValue::object();
        if let Some(d) = dir {
            layout.insert(
                "direction".into(),
                OwnedValue(json!(d.to_ascii_lowercase())),
            );
            let primary = n["primaryAxisAlignItems"].as_str().unwrap_or("MIN");
            let gap = if primary == "SPACE_BETWEEN" {
                Some(OwnedValue(json!("auto")))
            } else {
                let raw = n
                    .get("itemSpacing")
                    .map(copy_value)
                    .unwrap_or(OwnedValue(json!(0)));
                self.number(n, &id, "itemSpacing", raw, GAP)
            };
            if let Some(gap) = gap {
                layout.insert("gap".into(), gap);
            }
            let mut padding = OwnedValue::object();
            for (side, key) in [
                ("top", "paddingTop"),
                ("right", "paddingRight"),
                ("bottom", "paddingBottom"),
                ("left", "paddingLeft"),
            ] {
                let raw = n.get(key).map(copy_value).unwrap_or(OwnedValue(json!(0)));
                if let Some(e) = self.number(n, &id, key, raw, GAP) {
                    padding.insert(side.into(), e);
                }
            }
            layout.insert("padding".into(), padding);
            let align = |v: &Value| match v.as_str().unwrap_or("MIN") {
                "MIN" => "start".to_string(),
                "MAX" => "end".to_string(),
                "SPACE_BETWEEN" => "space-between".to_string(),
                other => other.to_ascii_lowercase(),
            };
            layout.insert(
                "align".into(),
                OwnedValue(json!({
                    "primary": align(&n["primaryAxisAlignItems"]),
                    "counter": align(&n["counterAxisAlignItems"]),
                })),
            );
        }
        let has_sizing_fields =
            n.get("layoutSizingHorizontal").is_some() || n.get("layoutSizingVertical").is_some();
        let emit_sizing = !absolute
            && match parent {
                Parent::Auto(_) => true,
                Parent::Root => dir.is_some() || has_sizing_fields,
                Parent::Other => dir.is_some(),
            };
        if emit_sizing {
            let parent_dir = match parent {
                Parent::Auto(d) => Some(d),
                _ => None,
            };
            layout.insert(
                "sizing".into(),
                OwnedValue(json!({
                    "horizontal": sizing(n, "HORIZONTAL", dir, parent_dir),
                    "vertical": sizing(n, "VERTICAL", dir, parent_dir),
                })),
            );
        }
        let has_layout = !layout.as_object().expect("layout object").is_empty();
        if has_layout && let Some(b) = n.get("absoluteBoundingBox") {
            let mut size = OwnedValue::object();
            size.insert("width".into(), copy_value(&b["width"]));
            size.insert("height".into(), copy_value(&b["height"]));
            layout.insert("size".into(), size);
        }
        if has_layout {
            out.insert("layout".into(), layout);
        }

        // Values.
        let mut values = OwnedValue::object();
        let mixed_text = node_type == "TEXT"
            && n["characterStyleOverrides"]
                .as_array()
                .is_some_and(|o| o.iter().any(|x| x.as_u64() != Some(0)));
        if mixed_text {
            self.unsupported(
                &id,
                "mixed text runs (characterStyleOverrides): fill and typography not specified",
            );
        } else {
            let fills = self.paints(n, &id, "fills", fill_prop(&node_type));
            if !fills.is_empty() {
                values.insert("fill".into(), OwnedValue(Value::Array(fills)));
            }
        }
        let strokes = self.paints(n, &id, "strokes", STROKE_COLOR);
        if !strokes.is_empty() {
            values.insert("stroke".into(), OwnedValue(Value::Array(strokes)));
            if n.get("individualStrokeWeights").is_some() {
                self.unsupported(&id, "individualStrokeWeights: stroke weight not specified");
            } else {
                let raw = n
                    .get("strokeWeight")
                    .map(copy_value)
                    .unwrap_or(OwnedValue(json!(1)));
                if let Some(e) = self.number(n, &id, "strokeWeight", raw, STROKE_WEIGHT) {
                    values.insert("stroke_weight".into(), e);
                }
            }
        }
        if let Some(radius) = self.radius(n, &id) {
            values.insert("radius".into(), radius);
        }
        if node_type == "TEXT" && !mixed_text {
            let typography = self.typography(n, &id);
            if !typography
                .as_object()
                .expect("typography object")
                .is_empty()
            {
                values.insert("typography".into(), typography);
            }
        }
        out.insert("values".into(), values);

        let child_parent = match dir {
            Some(d) => Parent::Auto(d),
            None => Parent::Other,
        };
        (out, Some(child_parent))
    }

    /// Visible `SOLID` paints of `fills`/`strokes` as value entries;
    /// any other paint type is unsupported.
    fn paints(&mut self, n: &Value, id: &str, key: &str, prop: Prop) -> Vec<Value> {
        let mut out = Vec::new();
        for (i, paint) in n[key].as_array().into_iter().flatten().enumerate() {
            if paint["visible"] == false {
                continue;
            }
            let ty = paint["type"].as_str().unwrap_or("unknown");
            let hex = (ty == "SOLID").then(|| paint_color(paint)).flatten();
            let Some(hex) = hex else {
                self.unsupported(id, format!("{key}[{i}]: {ty} paint not specified"));
                continue;
            };
            // The node-level binding is a fallback only when the paint has
            // none of its own: a mixed paint binding stays an error.
            let binding = match alias_id(paint.get("boundVariables").and_then(|b| b.get("color"))) {
                Ok(None) => alias_id(
                    n.get("boundVariables")
                        .and_then(|b| b.get(key))
                        .and_then(|b| b.get(i)),
                ),
                other => other,
            };
            let Ok(binding) = binding else {
                self.unsupported(id, format!("{key}[{i}]: mixed variable bindings"));
                continue;
            };
            let lit = Lit::Color(hex.clone());
            out.push(
                self.tokens
                    .entry(binding, OwnedValue(json!(hex)), Some(&lit), prop)
                    .into_inner(),
            );
        }
        out
    }

    /// The four corner radii, each bound or untokenized; `None` when the
    /// node has no radius.
    fn radius(&mut self, n: &Value, id: &str) -> Option<OwnedValue> {
        const CORNERS: [(&str, &str); 4] = [
            ("top_left", "topLeftRadius"),
            ("top_right", "topRightRadius"),
            ("bottom_right", "bottomRightRadius"),
            ("bottom_left", "bottomLeftRadius"),
        ];
        let bv = n.get("boundVariables");
        let any_bound = bv.and_then(|b| b.get("rectangleCornerRadii")).is_some()
            || CORNERS
                .iter()
                .any(|(_, k)| bv.and_then(|b| b.get(*k)).is_some());
        let per_corner = n["rectangleCornerRadii"].as_array();
        if n.get("cornerRadius").is_none() && per_corner.is_none() && !any_bound {
            return None;
        }
        let mut out = OwnedValue::object();
        for (i, (side, key)) in CORNERS.iter().enumerate() {
            let raw = per_corner
                .and_then(|r| r.get(i))
                .or_else(|| n.get("cornerRadius"))
                .map(copy_value)
                .unwrap_or(OwnedValue(json!(0)));
            if let Some(e) = self.number(n, id, key, raw, RADIUS) {
                out.insert(side.to_string(), e);
            }
        }
        Some(out)
    }

    /// A TEXT node's type style: family, size, weight, line height and
    /// letter spacing.
    fn typography(&mut self, n: &Value, id: &str) -> OwnedValue {
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
            (
                "font_size",
                "fontSize",
                "fontSize",
                Prop {
                    resolved_type: "FLOAT",
                    scopes: &["FONT_SIZE"],
                },
            ),
            (
                "font_weight",
                "fontWeight",
                "fontWeight",
                Prop {
                    resolved_type: "FLOAT",
                    scopes: &["FONT_WEIGHT"],
                },
            ),
            (
                "line_height",
                "lineHeightPx",
                "lineHeight",
                Prop {
                    resolved_type: "FLOAT",
                    scopes: &["LINE_HEIGHT"],
                },
            ),
            (
                "letter_spacing",
                "letterSpacing",
                "letterSpacing",
                Prop {
                    resolved_type: "FLOAT",
                    scopes: &["LETTER_SPACING"],
                },
            ),
        ];
        let style = &n["style"];
        let mut out = OwnedValue::object();
        for (name, field, binding_key, prop) in FIELDS {
            let Ok(binding) = self.binding(n, id, binding_key) else {
                continue;
            };
            let auto_line = name == "line_height" && style["lineHeightUnit"] == "INTRINSIC_%";
            let raw = if auto_line {
                OwnedValue(json!("auto"))
            } else {
                match style.get(field) {
                    Some(raw) => copy_value(raw),
                    None if binding.is_some() => OwnedValue(Value::Null),
                    None => continue,
                }
            };
            let lit = match (&*raw, auto_line) {
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
            "fills": [{"type": "SOLID", "color": {"r": 0, "g": 0, "b": 0, "a": 1},
                "boundVariables": {"color": [
                    {"type": "VARIABLE_ALIAS", "id": "V:1"},
                    {"type": "VARIABLE_ALIAS", "id": "V:2"}]}}],
            "children": []});
        let vars = json!({"snapshot": {"state": "none"}, "variables": []});
        let spec = build(&tree, &vars);
        assert!(spec["node"]["values"].get("fill").is_none(), "{spec}");
        assert_eq!(
            spec["unsupported"],
            json!([{"id": "1:1", "reason": "fills[0]: mixed variable bindings"}])
        );
        // A paint without its own binding still falls back to the node's.
        let mut tree = tree;
        tree["fills"][0]
            .as_object_mut()
            .unwrap()
            .remove("boundVariables");
        tree["boundVariables"] = json!({"fills": [{"type": "VARIABLE_ALIAS", "id": "V:3"}]});
        let spec = build(&tree, &vars);
        assert_eq!(spec["node"]["values"]["fill"][0]["variable_id"], "V:3");
        assert_eq!(spec["unsupported"], json!([]));
    }

    #[test]
    fn omitted_stroke_weight_uses_one_and_keeps_binding_or_matches() {
        let tree = json!({"id": "1:1", "name": "Box", "type": "RECTANGLE",
            "strokes": [{"type": "SOLID", "color": {"r": 0, "g": 0, "b": 0}}],
            "boundVariables": {"strokeWeight": {"type": "VARIABLE_ALIAS", "id": "V:weight"}},
            "children": []});
        let vars = json!({"snapshot": {"state": "current"}, "variables": [{
            "variable_id": "V:weight", "name": "stroke/weight", "source": "imported",
            "resolved_type": "FLOAT", "collection": "Stroke", "collection_id": "C:stroke",
            "default_mode_id": "Value", "mode_names": {"Value": "Value"},
            "scopes": ["STROKE_FLOAT"], "values_by_mode_id": {"Value": 1}
        }]});
        let spec = build(&tree, &vars);
        assert_eq!(
            spec["node"]["values"]["stroke_weight"],
            json!({
            "token": "stroke/weight", "variable_id": "V:weight", "css": "--stroke-weight", "raw": 1})
        );
        assert_eq!(spec["unsupported"], json!([]));
        let mut unbound = tree.clone();
        unbound.as_object_mut().unwrap().remove("boundVariables");
        let spec = build(&unbound, &vars);
        assert_eq!(
            spec["node"]["values"]["stroke_weight"],
            json!({"untokenized": true, "raw": 1,
            "matches": [{"token": "stroke/weight", "variable_id": "V:weight", "css": "--stroke-weight"}]})
        );
        let mut explicit = tree.clone();
        explicit["strokeWeight"] = json!(3);
        assert_eq!(
            build(&explicit, &vars)["node"]["values"]["stroke_weight"]["raw"],
            3
        );
        let mut individual = tree;
        individual["individualStrokeWeights"] =
            json!({"top": 2, "right": 1, "bottom": 1, "left": 1});
        let spec = build(&individual, &vars);
        assert!(spec["node"]["values"].get("stroke_weight").is_none());
        assert_eq!(
            spec["unsupported"],
            json!([{"id": "1:1", "reason": "individualStrokeWeights: stroke weight not specified"}])
        );
        unbound["strokes"][0]["visible"] = json!(false);
        assert!(
            build(&unbound, &vars)["node"]["values"]
                .get("stroke_weight")
                .is_none()
        );
    }

    #[test]
    fn matching_uses_collection_identity_when_display_names_repeat() {
        let tree = json!({"id": "1:1", "name": "Card", "type": "FRAME", "layoutMode": "VERTICAL",
            "itemSpacing": 8, "children": []});
        let vars = json!({"snapshot": {"state": "current"}, "variables": [
            {"variable_id": "V:theme", "name": "theme-gap", "source": "imported", "resolved_type": "FLOAT",
                "collection": "Same", "collection_id": "C:theme", "default_mode_id": "Light", "scopes": ["GAP"],
                "mode_names": {"Light": "Light", "Dark": "Dark"}, "values_by_mode_id": {"Light": 16, "Dark": 24}},
            {"variable_id": "V:other", "name": "other-gap", "source": "imported", "resolved_type": "FLOAT",
                "collection": "Same", "collection_id": "C:other", "default_mode_id": "Value", "scopes": ["GAP"],
                "mode_names": {"Light": "Light", "Value": "Value"}, "values_by_mode_id": {"Light": 99, "Value": 8}}
        ]});
        let spec = build(&tree, &vars);
        assert_eq!(spec["tokens_snapshot"]["theme"]["collection_id"], "C:theme");
        assert_eq!(
            spec["node"]["layout"]["gap"]["matches"],
            json!([
            {"token": "other-gap", "variable_id": "V:other", "css": "--other-gap"}])
        );
    }

    #[test]
    fn matching_uses_the_default_mode_id_when_mode_names_repeat() {
        let mut tree = json!({"id": "1:1", "name": "Card", "type": "FRAME",
            "layoutMode": "VERTICAL", "itemSpacing": 16, "children": []});
        let vars = json!({"snapshot": {"state": "current"}, "variables": [{
            "variable_id": "V:gap", "name": "gap", "source": "imported", "resolved_type": "FLOAT",
            "collection": "Spacing", "collection_id": "C:spacing", "default_mode_id": "M:1",
            "scopes": ["GAP"], "mode_names": {"M:1": "Value", "M:2": "Value"},
            "values_by_mode_id": {"M:1": 16, "M:2": 32}}]});
        let spec = build(&tree, &vars);
        assert_eq!(
            spec["node"]["layout"]["gap"]["matches"],
            json!([
            {"token": "gap", "variable_id": "V:gap", "css": "--gap"}])
        );
        tree["itemSpacing"] = json!(32);
        let spec = build(&tree, &vars);
        assert!(spec["node"]["layout"]["gap"].get("matches").is_none());
    }

    #[test]
    fn alias_cycles_and_missing_targets_do_not_become_matches() {
        let tree = json!({"id": "1:1", "name": "Card", "type": "FRAME",
            "layoutMode": "VERTICAL", "itemSpacing": 16, "children": []});
        let variable = |id: &str, value: Value| {
            json!({"variable_id": id, "name": id, "source": "imported", "resolved_type": "FLOAT",
                "collection": "Spacing", "collection_id": "C:spacing", "default_mode_id": "M:1",
                "scopes": ["GAP"], "mode_names": {"M:1": "Value"}, "values_by_mode_id": {"M:1": value}})
        };
        let alias = |id: &str| json!({"type": "VARIABLE_ALIAS", "id": id});
        let vars = json!({"snapshot": {"state": "current"}, "variables": [
            variable("V:cycle-a", alias("V:cycle-b")),
            variable("V:cycle-b", alias("V:cycle-a")),
            variable("V:cycle-prefix", alias("V:cycle-a")),
            variable("V:missing", alias("V:absent")),
            variable("V:literal", json!(16))
        ]});
        let spec = build(&tree, &vars);
        assert_eq!(
            spec["node"]["layout"]["gap"]["matches"],
            json!([{"token": "V:literal", "variable_id": "V:literal", "css": "--v-literal"}])
        );
    }

    #[test]
    fn rest_uniform_stroke_bindings_keep_the_actual_variable() {
        let alias = json!({"type": "VARIABLE_ALIAS", "id": "V:weight"});
        let tree = json!({"id": "1:1", "name": "Box", "type": "RECTANGLE",
            "strokes": [{"type": "SOLID", "color": {"r": 0, "g": 0, "b": 0}}],
            "strokeWeight": 1,
            "boundVariables": {"individualStrokeWeights": {
                "top": alias, "right": alias, "bottom": alias, "left": alias}},
            "children": []});
        let vars = json!({"snapshot": {"state": "current"}, "variables": [{
            "variable_id": "V:weight", "name": "stroke/thin", "source": "imported",
            "resolved_type": "FLOAT", "collection": "Stroke", "collection_id": "C:stroke",
            "default_mode_id": "M:1", "mode_names": {"M:1": "Value"},
            "scopes": ["STROKE_FLOAT"], "values_by_mode_id": {"M:1": 1}
        }]});
        for explicit_weight in [true, false] {
            let mut node = tree.clone();
            if !explicit_weight {
                node.as_object_mut().unwrap().remove("strokeWeight");
            }
            let spec = build(&node, &vars);
            assert_eq!(
                spec["node"]["values"]["stroke_weight"],
                json!({
                "raw": 1, "variable_id": "V:weight", "token": "stroke/thin", "css": "--stroke-thin"})
            );
            assert_eq!(spec["unsupported"], json!([]));
        }

        let mut partial = tree.clone();
        partial["boundVariables"]["individualStrokeWeights"]
            .as_object_mut()
            .unwrap()
            .remove("left");
        let mut mixed = tree.clone();
        mixed["boundVariables"]["individualStrokeWeights"]["left"]["id"] = json!("V:other");
        let mut conflicting = tree;
        conflicting["boundVariables"]["strokeWeight"] =
            json!({"type": "VARIABLE_ALIAS", "id": "V:other"});
        for node in [partial, mixed, conflicting] {
            let spec = build(&node, &vars);
            assert!(
                spec["node"]["values"].get("stroke_weight").is_none(),
                "{spec}"
            );
            assert_eq!(
                spec["unsupported"],
                json!([{
                "id": "1:1", "reason": "strokeWeight: mixed or partially bound sides: not specified"}])
            );
        }
    }

    #[test]
    fn rest_corner_bindings_keep_each_variable_and_report_mixed_corners() {
        let corners = [
            ("top_left", "RECTANGLE_TOP_LEFT_CORNER_RADIUS"),
            ("top_right", "RECTANGLE_TOP_RIGHT_CORNER_RADIUS"),
            ("bottom_right", "RECTANGLE_BOTTOM_RIGHT_CORNER_RADIUS"),
            ("bottom_left", "RECTANGLE_BOTTOM_LEFT_CORNER_RADIUS"),
        ];
        let mut bindings = serde_json::Map::new();
        let mut variables = Vec::new();
        for (corner, rest) in corners {
            let id = format!("V:{corner}");
            bindings.insert(rest.into(), json!({"type": "VARIABLE_ALIAS", "id": id}));
            variables.push(json!({
                "variable_id": id, "name": format!("radius/{corner}"), "source": "imported",
                "resolved_type": "FLOAT", "collection": "Radii", "collection_id": "C:radii",
                "default_mode_id": "M:1", "mode_names": {"M:1": "Value"},
                "scopes": ["CORNER_RADIUS"], "values_by_mode_id": {"M:1": 12}}));
        }
        let vars = json!({"snapshot": {"state": "current"}, "variables": variables});
        let tree = json!({"id": "1:1", "type": "RECTANGLE", "name": "Box",
            "rectangleCornerRadii": [12, 12, 12, 12],
            "boundVariables": {"rectangleCornerRadii": bindings}, "children": []});
        let spec = build(&tree, &vars);
        for (corner, _) in corners {
            let entry = &spec["node"]["values"]["radius"][corner];
            assert_eq!(entry["variable_id"], format!("V:{corner}"));
            assert_eq!(entry["raw"], 12);
            assert!(entry["css"].is_string(), "{entry}");
            assert!(entry.get("untokenized").is_none(), "{entry}");
        }
        assert_eq!(spec["unsupported"], json!([]));

        // Nested bindings alone still establish radius presence; the REST
        // default raw value is zero when both raw radius fields are absent.
        let mut absent_raw = tree.clone();
        absent_raw
            .as_object_mut()
            .unwrap()
            .remove("rectangleCornerRadii");
        let spec = build(&absent_raw, &vars);
        assert_eq!(spec["node"]["values"]["radius"]["top_left"]["raw"], 0);
        assert_eq!(
            spec["node"]["values"]["radius"]["top_left"]["variable_id"],
            "V:top_left"
        );

        let mut mixed = tree.clone();
        mixed["boundVariables"]["rectangleCornerRadii"][corners[0].1] = json!([
            {"type": "VARIABLE_ALIAS", "id": "V:top_left"},
            {"type": "VARIABLE_ALIAS", "id": "V:other"}]);
        let mut conflicting = tree.clone();
        conflicting["boundVariables"]["topLeftRadius"] =
            json!({"type": "VARIABLE_ALIAS", "id": "V:other"});
        for node in [mixed, conflicting] {
            let spec = build(&node, &vars);
            assert!(spec["node"]["values"]["radius"].get("top_left").is_none());
            assert_eq!(
                spec["node"]["values"]["radius"]["top_right"]["variable_id"],
                "V:top_right"
            );
            assert_eq!(
                spec["unsupported"],
                json!([{
                "id": "1:1", "reason": "topLeftRadius: bound to several variables (mixed): not specified"}])
            );
        }
        let mut flat = tree;
        let nested = flat["boundVariables"]
            .as_object_mut()
            .unwrap()
            .remove("rectangleCornerRadii")
            .unwrap();
        flat["boundVariables"]["topLeftRadius"] = nested[corners[0].1].clone();
        let spec = build(&flat, &vars);
        assert_eq!(
            spec["node"]["values"]["radius"]["top_left"]["variable_id"],
            "V:top_left"
        );
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
        let vars = json!({"snapshot": {"state": "none"}, "variables": []});
        let spec = build(&tree, &vars);
        assert_eq!(
            spec["node"]["layout"]["gap"],
            json!({"token": null, "variable_id": "V:9", "css": null, "raw": 8})
        );
        assert_eq!(
            spec["node"]["layout"]["padding"]["top"],
            json!({"untokenized": true, "raw": 0})
        );
        assert!(
            spec["tokens_snapshot"]["css_unavailable"]
                .as_str()
                .unwrap()
                .contains("state: none")
        );
    }
}
