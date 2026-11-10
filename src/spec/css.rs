//! The CSS layer of `figmog spec` (NWS-2947): one declaration map per
//! node — figmog's interpretation of the recorded design — and the
//! optional Tailwind class hint derived from exactly those declarations.
//!
//! A declaration's value is a literal (`16px`, `#1a1a1f`) or, for a value
//! bound to a variable, `var(--name)` with the token's Tailwind theme name
//! (`--color-brand`) alongside, so the hint can say `bg-brand` instead of
//! an arbitrary value. Unbound values never become theme utilities: a raw
//! `8px` gap is `gap-[8px]` even when a token happens to equal it.
//!
//! Determinism: declarations live in a `BTreeMap` keyed by property; the
//! hint orders classes by a fixed property rank, then property name.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// A CSS value, plus the bound token's Tailwind theme name when the value
/// is that token's `var()`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CssVal {
    pub(super) value: String,
    pub(super) theme: Option<String>,
}

impl CssVal {
    pub(super) fn lit(value: impl Into<String>) -> CssVal {
        CssVal {
            value: value.into(),
            theme: None,
        }
    }

    /// Whether this is a token's `var()` rather than a literal.
    pub(super) fn is_var(&self) -> bool {
        self.value.starts_with("var(")
    }

    /// A numeric literal with the requested unit; expressions stay symbolic.
    pub(super) fn literal_number(&self, unit: &str) -> Option<f64> {
        self.value.strip_suffix(unit)?.parse().ok()
    }

    /// `self × factor`, retaining symbolic lengths in a multiplication calc.
    pub(super) fn scaled(&self, factor: f64, unit: &str) -> CssVal {
        match self.literal_number(unit) {
            Some(n) => CssVal::lit(format!("{}{unit}", num(n * factor))),
            None => CssVal::lit(format!("calc({} * {})", self.value, num(factor))),
        }
    }

    /// An inset measured inside this border, retaining symbolic lengths.
    pub(super) fn inset(&self, value: f64) -> String {
        match self.literal_number("px") {
            Some(n) => px(value - n),
            None => format!("calc({} - {})", px(value), self.value),
        }
    }
}

/// One node's declarations.
#[derive(Debug, Default)]
pub(super) struct Css {
    decls: BTreeMap<&'static str, CssVal>,
}

impl Css {
    pub(super) fn set(&mut self, prop: &'static str, value: CssVal) {
        self.decls.insert(prop, value);
    }

    pub(super) fn lit(&mut self, prop: &'static str, value: impl Into<String>) {
        self.set(prop, CssVal::lit(value));
    }

    pub(super) fn get(&self, prop: &str) -> Option<&CssVal> {
        self.decls.get(prop)
    }

    pub(super) fn has(&self, prop: &str) -> bool {
        self.decls.contains_key(prop)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.decls.is_empty()
    }

    /// Four per-side values as one shorthand when they are equal, else
    /// the longhands; `None` sides (an unbound zero) are left out.
    pub(super) fn four(
        &mut self,
        shorthand: &'static str,
        longhands: [&'static str; 4],
        sides: [Option<CssVal>; 4],
    ) {
        if sides.iter().all(Option::is_none) {
            return;
        }
        if sides.iter().all(|s| *s == sides[0]) {
            if let Some(v) = sides[0].clone() {
                self.set(shorthand, v);
            }
            return;
        }
        for (prop, side) in longhands.into_iter().zip(sides) {
            if let Some(v) = side {
                self.set(prop, v);
            }
        }
    }

    /// `{property: value}`, sorted by property.
    pub(super) fn to_json(&self) -> Value {
        Value::Object(
            self.decls
                .iter()
                .map(|(p, v)| (p.to_string(), Value::String(v.value.clone())))
                .collect::<Map<String, Value>>(),
        )
    }

    /// The Tailwind class hint: one class per declaration (some, like
    /// `-webkit-box-orient`, are covered by another's class), ordered by
    /// [`RANK`].
    pub(super) fn tailwind(&self) -> String {
        let mut classes: Vec<(usize, &str, String)> = self
            .decls
            .iter()
            .filter_map(|(prop, v)| {
                let rank = RANK.iter().position(|p| p == prop).unwrap_or(RANK.len());
                Some((rank, *prop, class(prop, v)?))
            })
            .collect();
        classes.sort();
        let mut out: Vec<String> = classes.into_iter().map(|(_, _, c)| c).collect();
        for (a, b, both) in [("pt-", "pb-", "py-"), ("pl-", "pr-", "px-")] {
            pair(&mut out, a, b, both);
        }
        out.join(" ")
    }
}

/// `a<X> … b<X>` (same `X`) → `both<X>` in `a`'s place: `pt-4 pb-4` →
/// `py-4`.
fn pair(classes: &mut Vec<String>, a: &str, b: &str, both: &str) {
    let Some(i) = classes.iter().position(|c| c.starts_with(a)) else {
        return;
    };
    let value = classes[i][a.len()..].to_string();
    let Some(j) = classes.iter().position(|c| *c == format!("{b}{value}")) else {
        return;
    };
    classes[i] = format!("{both}{value}");
    classes.remove(j);
}

/// `v` with at most three decimals, no trailing zeros, no `-0`.
pub(super) fn num(v: f64) -> String {
    let r = (v * 1000.0).round() / 1000.0;
    if r == 0.0 {
        return "0".into();
    }
    if r == r.trunc() && r.abs() < 1e15 {
        return format!("{}", r as i64);
    }
    let s = format!("{r:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `v` px.
pub(super) fn px(v: f64) -> String {
    format!("{}px", num(v))
}

/// Class order: position, box layout, own size, then paint and text.
const RANK: &[&str] = &[
    "position",
    "top",
    "right",
    "bottom",
    "left",
    "transform",
    "display",
    "flex-direction",
    "flex-wrap",
    "grid-template-columns",
    "grid-template-rows",
    "grid-column",
    "grid-row",
    "justify-content",
    "align-items",
    "align-content",
    "gap",
    "row-gap",
    "column-gap",
    "padding",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "margin-top",
    "margin-left",
    "box-sizing",
    "width",
    "height",
    "min-width",
    "max-width",
    "min-height",
    "max-height",
    "flex",
    "flex-shrink",
    "align-self",
    "justify-self",
    "overflow",
    "background-color",
    "border-width",
    "border-style",
    "border-color",
    "outline-width",
    "outline-style",
    "outline-color",
    "outline-offset",
    "border-radius",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-bottom-right-radius",
    "border-bottom-left-radius",
    "box-shadow",
    "text-shadow",
    "opacity",
    "color",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "line-height",
    "letter-spacing",
    "text-align",
    "text-transform",
    "font-variant-caps",
    "text-decoration-line",
    "white-space",
    "text-overflow",
    "-webkit-box-orient",
    "-webkit-line-clamp",
];

/// Utility prefix and Tailwind theme namespace for value properties.
fn utility(prop: &str) -> Option<(&'static str, Option<&'static str>)> {
    const SPACING: Option<&str> = Some("--spacing-");
    const COLOR: Option<&str> = Some("--color-");
    const RADIUS: Option<&str> = Some("--radius-");
    Some(match prop {
        "gap" => ("gap", SPACING),
        "row-gap" => ("gap-y", SPACING),
        "column-gap" => ("gap-x", SPACING),
        "padding" => ("p", SPACING),
        "padding-top" => ("pt", SPACING),
        "padding-right" => ("pr", SPACING),
        "padding-bottom" => ("pb", SPACING),
        "padding-left" => ("pl", SPACING),
        "margin-top" => ("mt", SPACING),
        "margin-left" => ("ml", SPACING),
        "width" => ("w", SPACING),
        "height" => ("h", SPACING),
        "min-width" => ("min-w", SPACING),
        "max-width" => ("max-w", SPACING),
        "min-height" => ("min-h", SPACING),
        "max-height" => ("max-h", SPACING),
        "top" => ("top", SPACING),
        "right" => ("right", SPACING),
        "bottom" => ("bottom", SPACING),
        "left" => ("left", SPACING),
        "background-color" => ("bg", COLOR),
        "color" => ("text", COLOR),
        "border-color" => ("border", COLOR),
        "outline-color" => ("outline", COLOR),
        "border-radius" => ("rounded", RADIUS),
        "border-top-left-radius" => ("rounded-tl", RADIUS),
        "border-top-right-radius" => ("rounded-tr", RADIUS),
        "border-bottom-right-radius" => ("rounded-br", RADIUS),
        "border-bottom-left-radius" => ("rounded-bl", RADIUS),
        "grid-template-columns" => ("grid-cols", None),
        "grid-template-rows" => ("grid-rows", None),
        "grid-column" => ("col", None),
        "grid-row" => ("row", None),
        "line-height" => ("leading", None),
        "letter-spacing" => ("tracking", None),
        "border-width" => ("border", None),
        "outline-width" => ("outline", None),
        "box-shadow" => ("shadow", None),
        "opacity" => ("opacity", None),
        "font-size" => ("text", None),
        _ => return None,
    })
}

/// Spaces become `_` inside a Tailwind arbitrary value; `"` becomes `'`.
fn arbitrary(value: &str) -> String {
    value.replace(' ', "_").replace('"', "'")
}

/// The class for one declaration (`None`: another class covers it).
fn class(prop: &str, v: &CssVal) -> Option<String> {
    let value = v.value.as_str();
    let keyword = match (prop, value) {
        ("display", "flex") => "flex",
        ("display", "grid") => "grid",
        ("display", "-webkit-box") | ("-webkit-box-orient", _) | ("box-sizing", "border-box") => {
            // `line-clamp-N` sets the box; Tailwind's preflight is border-box.
            return None;
        }
        ("flex-direction", "row") => "flex-row",
        ("flex-direction", "column") => "flex-col",
        ("flex-wrap", "wrap") => "flex-wrap",
        ("justify-content", "flex-start") => "justify-start",
        ("justify-content", "center") => "justify-center",
        ("justify-content", "flex-end") => "justify-end",
        ("justify-content", "space-between") => "justify-between",
        ("align-items", "flex-start") => "items-start",
        ("align-items", "center") => "items-center",
        ("align-items", "flex-end") => "items-end",
        ("align-items", "baseline") => "items-baseline",
        ("align-content", "flex-start") => "content-start",
        ("align-content", "center") => "content-center",
        ("align-content", "flex-end") => "content-end",
        ("align-content", "space-between") => "content-between",
        ("align-self", "stretch") => "self-stretch",
        ("align-self", "start") => "self-start",
        ("align-self", "center") => "self-center",
        ("align-self", "end") => "self-end",
        ("justify-self", "stretch") => "justify-self-stretch",
        ("justify-self", "start") => "justify-self-start",
        ("justify-self", "center") => "justify-self-center",
        ("justify-self", "end") => "justify-self-end",
        ("position", "absolute") => "absolute",
        ("position", "relative") => "relative",
        ("overflow", "hidden") => "overflow-hidden",
        ("flex-shrink", "0") => "shrink-0",
        ("flex", "1 1 0%") => "flex-1",
        ("width", "fit-content") => "w-fit",
        ("height", "fit-content") => "h-fit",
        ("width", "100%") => "w-full",
        ("height", "100%") => "h-full",
        ("white-space", "nowrap") => "whitespace-nowrap",
        ("white-space", "pre") => "whitespace-pre",
        ("white-space", "pre-wrap") => "whitespace-pre-wrap",
        ("min-width", "0") => "min-w-0",
        ("min-height", "0") => "min-h-0",
        ("text-overflow", "ellipsis") => "text-ellipsis",
        ("text-align", "center") => "text-center",
        ("text-align", "right") => "text-right",
        ("text-align", "justify") => "text-justify",
        ("text-transform", "uppercase") => "uppercase",
        ("text-transform", "lowercase") => "lowercase",
        ("text-transform", "capitalize") => "capitalize",
        ("text-decoration-line", "underline") => "underline",
        ("text-decoration-line", "line-through") => "line-through",
        ("font-style", "italic") => "italic",
        ("border-style", "solid") => "border-solid",
        ("outline-style", "solid") => "outline-solid",
        ("-webkit-line-clamp", n) => return Some(format!("line-clamp-{n}")),
        ("grid-column" | "grid-row", span) if span.starts_with("span ") => {
            let n = &span["span ".len()..];
            let axis = if prop == "grid-column" { "col" } else { "row" };
            return Some(format!("{axis}-span-{n}"));
        }
        _ => "",
    };
    if !keyword.is_empty() {
        return Some(keyword.to_string());
    }
    let Some((prefix, namespace)) = utility(prop) else {
        return Some(format!("[{prop}:{}]", arbitrary(value)));
    };
    if let (Some(ns), Some(theme)) = (namespace, v.theme.as_deref())
        && theme.len() > ns.len()
        && let Some(suffix) = theme.strip_prefix(ns)
    {
        return Some(format!("{prefix}-{suffix}"));
    }
    if v.is_var() {
        // A token with no theme utility: the arbitrary property, which
        // never has to guess the value's type.
        return Some(format!("[{prop}:{}]", arbitrary(value)));
    }
    Some(format!("{prefix}-[{}]", arbitrary(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_lengths_distinguish_literals_from_expressions() {
        for (value, scaled, inset) in [
            ("2px", "-1px", "8px"),
            (
                "var(--stroke)",
                "calc(var(--stroke) * -0.5)",
                "calc(10px - var(--stroke))",
            ),
            (
                "calc(var(--stroke) * 1px)",
                "calc(calc(var(--stroke) * 1px) * -0.5)",
                "calc(10px - calc(var(--stroke) * 1px))",
            ),
            (
                "min(2px, 1vw)",
                "calc(min(2px, 1vw) * -0.5)",
                "calc(10px - min(2px, 1vw))",
            ),
        ] {
            let length = CssVal::lit(value);
            assert_eq!(length.scaled(-0.5, "px").value, scaled);
            assert_eq!(length.inset(10.0), inset);
        }
    }

    #[test]
    fn numbers_are_short_and_stable() {
        assert_eq!(num(16.0), "16");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(7.999_999_9), "8");
        assert_eq!(num(0.125), "0.125");
        assert_eq!(num(33.333_333), "33.333");
        assert_eq!(px(-8.0), "-8px");
    }

    #[test]
    fn classes_use_theme_utilities_only_for_bound_tokens() {
        let token = |v: &str, t: &str| CssVal {
            value: format!("var({v})"),
            theme: Some(t.into()),
        };
        let mut css = Css::default();
        css.lit("display", "flex");
        css.lit("flex-direction", "column");
        css.set("gap", token("--spacing-4", "--spacing-4"));
        css.set("background-color", token("--surface", "--color-surface"));
        css.set("border-radius", token("--radius-md", "--radius-md"));
        css.lit("padding", "24px");
        css.set(
            "font-weight",
            CssVal {
                value: "var(--font-weight-bold)".into(),
                theme: None,
            },
        );
        css.lit("color", "#1a1a1f");
        css.lit("box-shadow", "0px 4px 8px 0px #00000040");
        assert_eq!(
            css.tailwind(),
            "flex flex-col gap-4 p-[24px] bg-surface rounded-md shadow-[0px_4px_8px_0px_#00000040] text-[#1a1a1f] [font-weight:var(--font-weight-bold)]"
        );
    }

    #[test]
    fn four_sides_collapse_only_when_equal() {
        let mut css = Css::default();
        let s = |v: &str| Some(CssVal::lit(v));
        css.four(
            "padding",
            [
                "padding-top",
                "padding-right",
                "padding-bottom",
                "padding-left",
            ],
            [s("8px"), s("8px"), s("8px"), s("8px")],
        );
        assert_eq!(css.to_json(), serde_json::json!({"padding": "8px"}));
        let mut css = Css::default();
        css.four(
            "padding",
            [
                "padding-top",
                "padding-right",
                "padding-bottom",
                "padding-left",
            ],
            [s("8px"), None, s("8px"), None],
        );
        assert_eq!(
            css.to_json(),
            serde_json::json!({"padding-bottom": "8px", "padding-top": "8px"})
        );
    }
}
