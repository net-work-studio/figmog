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

use serde_json::{Map, Value, json};

use crate::tokens::{ExportOptions, color, export_css};

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
/// `vars` the `figmog_vars {snapshot: true}` answer; `tokens` the token
/// export options (the project's `[tokens]` settings), so the CSS names
/// and root modes match the committed token file.
pub fn build(tree: &Value, vars: &Value, tokens: &ExportOptions) -> Value {
    let (css, theme, roots, css_unavailable) = match export_css(vars, tokens) {
        Ok(export) => (export.names, export.theme, export.root_modes, Value::Null),
        Err(e) => (BTreeMap::new(), None, BTreeMap::new(), Value::String(e)),
    };
    let tokens = Tokens::new(vars, css, &roots);
    let mut cx = Cx {
        tokens: &tokens,
        unsupported: BTreeSet::new(),
    };
    let node = cx.node(tree, Parent::Root);
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

impl Tokens {
    fn new(
        vars: &Value,
        css: BTreeMap<String, String>,
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
            root,
        }
    }

    /// A bound value: the token, its CSS name and the raw value.
    fn bound(&self, variable_id: &str, raw: Value) -> Value {
        json!({
            "token": self.vars.get(variable_id).map(|v| v.name.clone()),
            "variable_id": variable_id,
            "css": self.css.get(variable_id),
            "raw": raw,
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
    fn entry(&self, binding: Option<String>, raw: Value, lit: Option<&Lit>, prop: Prop) -> Value {
        match binding {
            Some(id) => self.bound(&id, raw),
            None => self.untokenized(raw, lit, prop),
        }
    }
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

struct Cx<'a> {
    tokens: &'a Tokens,
    unsupported: BTreeSet<(String, String)>,
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

impl Cx<'_> {
    fn unsupported(&mut self, id: &str, reason: impl Into<String>) {
        self.unsupported.insert((id.to_string(), reason.into()));
    }

    /// A node's binding for `key` (node `boundVariables`, then — for text
    /// properties — the type style's own), reporting a mixed binding.
    fn binding(&mut self, n: &Value, id: &str, key: &str) -> Result<Option<String>, ()> {
        let found =
            alias_id(n.get("boundVariables").and_then(|b| b.get(key))).and_then(|b| match b {
                Some(b) => Ok(Some(b)),
                None => alias_id(
                    n.get("style")
                        .and_then(|s| s.get("boundVariables"))
                        .and_then(|b| b.get(key)),
                ),
            });
        if found.is_err() {
            self.unsupported(
                id,
                format!("{key}: bound to several variables (mixed): not specified"),
            );
        }
        found
    }

    /// A numeric property: bound or untokenized, raw value as stored.
    fn number(&mut self, n: &Value, id: &str, key: &str, raw: Value, prop: Prop) -> Option<Value> {
        let binding = self.binding(n, id, key).ok()?;
        let lit = raw.as_f64().map(Lit::Number);
        Some(self.tokens.entry(binding, raw, lit.as_ref(), prop))
    }

    fn node(&mut self, n: &Value, parent: Parent) -> Value {
        let id = n["id"].as_str().unwrap_or_default().to_string();
        let node_type = n["type"].as_str().unwrap_or_default().to_string();
        let mut out = Map::new();
        out.insert("id".into(), json!(id));
        out.insert("name".into(), n["name"].clone());
        out.insert("type".into(), json!(node_type));
        if n["visible"] == false {
            self.unsupported(&id, "hidden layer (visible: false): not specified");
            return Value::Object(out);
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
        let mut layout = Map::new();
        if let Some(d) = dir {
            layout.insert("direction".into(), json!(d.to_ascii_lowercase()));
            let primary = n["primaryAxisAlignItems"].as_str().unwrap_or("MIN");
            let gap = if primary == "SPACE_BETWEEN" {
                Some(json!("auto"))
            } else {
                let raw = n.get("itemSpacing").cloned().unwrap_or(json!(0));
                self.number(n, &id, "itemSpacing", raw, GAP)
            };
            if let Some(gap) = gap {
                layout.insert("gap".into(), gap);
            }
            let mut padding = Map::new();
            for (side, key) in [
                ("top", "paddingTop"),
                ("right", "paddingRight"),
                ("bottom", "paddingBottom"),
                ("left", "paddingLeft"),
            ] {
                let raw = n.get(key).cloned().unwrap_or(json!(0));
                if let Some(e) = self.number(n, &id, key, raw, GAP) {
                    padding.insert(side.into(), e);
                }
            }
            layout.insert("padding".into(), Value::Object(padding));
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
                json!({
                    "horizontal": sizing(n, "HORIZONTAL", dir, parent_dir),
                    "vertical": sizing(n, "VERTICAL", dir, parent_dir),
                }),
            );
        }
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
        let mut values = Map::new();
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
                values.insert("fill".into(), Value::Array(fills));
            }
        }
        let strokes = self.paints(n, &id, "strokes", STROKE_COLOR);
        if !strokes.is_empty() {
            values.insert("stroke".into(), Value::Array(strokes));
            if n.get("individualStrokeWeights").is_some() {
                self.unsupported(&id, "individualStrokeWeights: stroke weight not specified");
            } else if let Some(raw) = n.get("strokeWeight").cloned()
                && let Some(e) = self.number(n, &id, "strokeWeight", raw, STROKE_WEIGHT)
            {
                values.insert("stroke_weight".into(), e);
            }
        }
        if let Some(radius) = self.radius(n, &id) {
            values.insert("radius".into(), radius);
        }
        if node_type == "TEXT" && !mixed_text {
            let typography = self.typography(n, &id);
            if !typography.is_empty() {
                values.insert("typography".into(), Value::Object(typography));
            }
        }
        out.insert("values".into(), Value::Object(values));

        let child_parent = match dir {
            Some(d) => Parent::Auto(d),
            None => Parent::Other,
        };
        let children: Vec<Value> = kids.iter().map(|k| self.node(k, child_parent)).collect();
        out.insert("children".into(), Value::Array(children));
        Value::Object(out)
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
            out.push(self.tokens.entry(binding, json!(hex), Some(&lit), prop));
        }
        out
    }

    /// The four corner radii, each bound or untokenized; `None` when the
    /// node has no radius.
    fn radius(&mut self, n: &Value, id: &str) -> Option<Value> {
        const CORNERS: [(&str, &str); 4] = [
            ("top_left", "topLeftRadius"),
            ("top_right", "topRightRadius"),
            ("bottom_right", "bottomRightRadius"),
            ("bottom_left", "bottomLeftRadius"),
        ];
        let bv = n.get("boundVariables");
        let any_bound = CORNERS
            .iter()
            .any(|(_, k)| bv.and_then(|b| b.get(*k)).is_some());
        let per_corner = n["rectangleCornerRadii"].as_array();
        if n.get("cornerRadius").is_none() && per_corner.is_none() && !any_bound {
            return None;
        }
        let mut out = Map::new();
        for (i, (side, key)) in CORNERS.iter().enumerate() {
            let raw = per_corner
                .and_then(|r| r.get(i))
                .or_else(|| n.get("cornerRadius"))
                .cloned()
                .unwrap_or(json!(0));
            if let Some(e) = self.number(n, id, key, raw, RADIUS) {
                out.insert(side.to_string(), e);
            }
        }
        Some(Value::Object(out))
    }

    /// A TEXT node's type style: family, size, weight, line height and
    /// letter spacing.
    fn typography(&mut self, n: &Value, id: &str) -> Map<String, Value> {
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
        let mut out = Map::new();
        for (name, field, binding_key, prop) in FIELDS {
            let Ok(binding) = self.binding(n, id, binding_key) else {
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
        let spec = build(&tree, &vars, &ExportOptions::default());
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
        let spec = build(&tree, &vars, &ExportOptions::default());
        assert_eq!(spec["node"]["values"]["fill"][0]["variable_id"], "V:3");
        assert_eq!(spec["unsupported"], json!([]));
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
        let spec = build(&tree, &vars, &ExportOptions::default());
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
