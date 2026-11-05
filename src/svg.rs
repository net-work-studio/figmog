//! A small, strict, allowlist-based SVG sanitizer (NWS-2952) for the icon
//! renders `figmog assets export` turns into committed `.svg` files and
//! React components. The markup comes from Figma's render endpoint, but
//! the file it renders is untrusted input, so nothing in it is passed
//! through unless it is on an allowlist.
//!
//! **Parsing.** A hand-written, non-validating XML reader (no new
//! dependency): elements, attributes (single or double quoted), text,
//! CDATA, comments and processing instructions. The five predefined
//! entities and numeric character references are decoded; any other
//! entity, and any `<!DOCTYPE`/`<!ENTITY` declaration, rejects the whole
//! document (no entity expansion, ever). Size, depth, element and
//! attribute counts are capped. Mismatched or unclosed tags are errors.
//!
//! **Sanitizing.** Only the elements in [`ELEMENTS`] survive; any other
//! element — `script`, `style`, `foreignObject`, `a`, animations,
//! `feImage`, namespaced (`sodipodi:…`) elements, anything unknown — is
//! dropped with its whole content. Only the attributes in [`ATTRS`]
//! survive (so every `on*` handler, `class`, `xml:base` and unknown
//! attribute is dropped). Values must be plain text: a value containing a
//! quote, `<`, `>`, `&`, a backtick, a brace, a backslash or a control
//! character is dropped. URL-bearing values are reduced to local
//! references: `href`/`xlink:href` must be `#<id>` (or, on `<image>`, a
//! base64 `data:image/{png,jpeg,gif,webp}` URI); `fill`, `stroke`,
//! `clip-path`, `mask`, `filter` may hold `url(#<id>)` and nothing else
//! URL-like; a `url(#id)` to an id the document doesn't define is dropped.
//! The `style` attribute keeps only `mask-type`, `mix-blend-mode` and
//! `isolation` with keyword values (Figma writes `style="mask-type:alpha"`).
//! Every removal is reported ([`Sanitized::removed`]).
//!
//! **Optimizing** (lightly, never touching geometry): comments, PIs,
//! whitespace-only text, empty `<defs>`, unreferenced ids and attribute-less
//! `<g>` wrappers are removed. Numbers and path data are kept verbatim.
//!
//! **Ids.** Every kept id is renumbered in document order and rendered
//! with a caller-chosen prefix, so two inlined copies never share a
//! gradient, mask, clip-path or filter id: [`Sanitized::to_svg`] takes a
//! fixed prefix, [`Sanitized::to_jsx`] a JS expression (a React `useId`).
//!
//! **Colors.** [`Sanitized::single_color`] is the one paint color an icon
//! uses — fills, strokes, stops and floods outside `<clipPath>`/`<mask>`
//! (whose colors are geometry, not paint) — or `None` when it uses several,
//! none, or any gradient, pattern, filter or image.
//! [`Sanitized::use_current_color`] replaces that color with
//! `currentColor`.
//!
//! **Limitations.** Not a general XML or SVG implementation: no namespaces
//! beyond dropping prefixed names (and reading `xlink:href` as `href`), no
//! `<style>` sheets (dropped whole, so CSS-styled SVGs lose those styles),
//! no `<symbol>`, `<marker>`, `<switch>` or `<text>` on a path. It is meant
//! for Figma's own SVG export shape; anything outside that shape is
//! dropped rather than interpreted.

use std::collections::{BTreeMap, BTreeSet};

/// Largest SVG accepted, in bytes.
pub(crate) const MAX_SVG_BYTES: usize = 4 * 1024 * 1024;
/// Deepest element nesting accepted.
const MAX_DEPTH: usize = 64;
/// Most elements accepted.
const MAX_ELEMENTS: usize = 20_000;
/// Most attributes on one element.
const MAX_ATTRS: usize = 64;
/// Longest id kept.
const MAX_ID: usize = 256;

/// Elements kept (SVG's own camelCase names).
const ELEMENTS: [&str; 39] = [
    "svg",
    "g",
    "defs",
    "path",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "linearGradient",
    "radialGradient",
    "stop",
    "clipPath",
    "mask",
    "pattern",
    "use",
    "image",
    "title",
    "desc",
    "text",
    "tspan",
    "filter",
    "feBlend",
    "feColorMatrix",
    "feComponentTransfer",
    "feComposite",
    "feDisplacementMap",
    "feDropShadow",
    "feFlood",
    "feFuncA",
    "feFuncB",
    "feFuncG",
    "feFuncR",
    "feGaussianBlur",
    "feMerge",
    "feMergeNode",
    "feMorphology",
    "feOffset",
];

/// Elements whose text content is kept.
const TEXT_ELEMENTS: [&str; 4] = ["title", "desc", "text", "tspan"];

/// Elements that paint with something other than a flat color.
const NON_FLAT: [&str; 5] = [
    "linearGradient",
    "radialGradient",
    "pattern",
    "filter",
    "image",
];

/// Attributes kept: the SVG name and the React (JSX) name.
const ATTRS: [(&str, &str); 97] = [
    ("id", "id"),
    ("d", "d"),
    ("x", "x"),
    ("y", "y"),
    ("x1", "x1"),
    ("y1", "y1"),
    ("x2", "x2"),
    ("y2", "y2"),
    ("cx", "cx"),
    ("cy", "cy"),
    ("r", "r"),
    ("rx", "rx"),
    ("ry", "ry"),
    ("fx", "fx"),
    ("fy", "fy"),
    ("fr", "fr"),
    ("width", "width"),
    ("height", "height"),
    ("viewBox", "viewBox"),
    ("preserveAspectRatio", "preserveAspectRatio"),
    ("points", "points"),
    ("transform", "transform"),
    ("offset", "offset"),
    ("opacity", "opacity"),
    ("fill", "fill"),
    ("fill-rule", "fillRule"),
    ("fill-opacity", "fillOpacity"),
    ("clip-rule", "clipRule"),
    ("clip-path", "clipPath"),
    ("mask", "mask"),
    ("filter", "filter"),
    ("stroke", "stroke"),
    ("stroke-width", "strokeWidth"),
    ("stroke-linecap", "strokeLinecap"),
    ("stroke-linejoin", "strokeLinejoin"),
    ("stroke-miterlimit", "strokeMiterlimit"),
    ("stroke-dasharray", "strokeDasharray"),
    ("stroke-dashoffset", "strokeDashoffset"),
    ("stroke-opacity", "strokeOpacity"),
    ("stop-color", "stopColor"),
    ("stop-opacity", "stopOpacity"),
    ("flood-color", "floodColor"),
    ("flood-opacity", "floodOpacity"),
    ("color", "color"),
    ("color-interpolation", "colorInterpolation"),
    ("color-interpolation-filters", "colorInterpolationFilters"),
    ("shape-rendering", "shapeRendering"),
    ("vector-effect", "vectorEffect"),
    ("paint-order", "paintOrder"),
    ("visibility", "visibility"),
    ("display", "display"),
    ("gradientUnits", "gradientUnits"),
    ("gradientTransform", "gradientTransform"),
    ("spreadMethod", "spreadMethod"),
    ("clipPathUnits", "clipPathUnits"),
    ("maskUnits", "maskUnits"),
    ("maskContentUnits", "maskContentUnits"),
    ("patternUnits", "patternUnits"),
    ("patternContentUnits", "patternContentUnits"),
    ("patternTransform", "patternTransform"),
    ("filterUnits", "filterUnits"),
    ("primitiveUnits", "primitiveUnits"),
    ("in", "in"),
    ("in2", "in2"),
    ("result", "result"),
    ("stdDeviation", "stdDeviation"),
    ("dx", "dx"),
    ("dy", "dy"),
    ("values", "values"),
    ("type", "type"),
    ("mode", "mode"),
    ("operator", "operator"),
    ("k1", "k1"),
    ("k2", "k2"),
    ("k3", "k3"),
    ("k4", "k4"),
    ("radius", "radius"),
    ("scale", "scale"),
    ("xChannelSelector", "xChannelSelector"),
    ("yChannelSelector", "yChannelSelector"),
    ("tableValues", "tableValues"),
    ("slope", "slope"),
    ("intercept", "intercept"),
    ("amplitude", "amplitude"),
    ("exponent", "exponent"),
    ("href", "href"),
    ("font-family", "fontFamily"),
    ("font-size", "fontSize"),
    ("font-weight", "fontWeight"),
    ("font-style", "fontStyle"),
    ("letter-spacing", "letterSpacing"),
    ("text-anchor", "textAnchor"),
    ("dominant-baseline", "dominantBaseline"),
    ("textLength", "textLength"),
    ("lengthAdjust", "lengthAdjust"),
    ("xml:space", "xmlSpace"),
    ("style", "style"),
];

/// Attributes that hold a paint (a color, `none` or `url(#id)`).
const PAINT_ATTRS: [&str; 4] = ["fill", "stroke", "stop-color", "flood-color"];
/// Attributes that may only reference a local id via `url(#id)` (or `none`).
const REF_ATTRS: [&str; 3] = ["clip-path", "mask", "filter"];
/// `style` properties kept, with their React names.
const STYLE_PROPS: [(&str, &str); 3] = [
    ("mask-type", "maskType"),
    ("mix-blend-mode", "mixBlendMode"),
    ("isolation", "isolation"),
];

/// A kept attribute value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Val {
    /// Plain text (already checked safe).
    Lit(String),
    /// This element's id: the index of a kept id.
    Id(usize),
    /// `url(#<id>)` with the referenced id's index.
    Url(usize),
    /// `#<id>` (an `href`) with the referenced id's index.
    Hash(usize),
    /// `style`: kept `(property, keyword)` pairs.
    Style(Vec<(&'static str, String)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    El(El),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct El {
    name: &'static str,
    attrs: Vec<(&'static str, Val)>,
    children: Vec<Node>,
}

/// A sanitized SVG document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sanitized {
    root: El,
    /// How many distinct ids the output defines.
    ids: usize,
    /// What was removed, sorted and deduplicated (`dropped <script>`, …).
    pub(crate) removed: Vec<String>,
}

// ---- parsing ----

/// A parsed (not yet sanitized) element.
#[derive(Debug)]
struct RawEl {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<RawNode>,
}

#[derive(Debug)]
enum RawNode {
    El(RawEl),
    Text(String),
}

struct Reader<'a> {
    s: &'a str,
    pos: usize,
    elements: usize,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.pos..]
    }

    fn err(&self, message: &str) -> String {
        format!("invalid SVG at byte {}: {message}", self.pos)
    }

    fn skip_ws(&mut self) {
        let rest = self.rest();
        self.pos += rest.len() - rest.trim_start().len();
    }

    fn name(&mut self) -> Result<String, String> {
        let rest = self.rest();
        let first = rest.chars().next();
        if !first.is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
            return Err(self.err("expected a name"));
        }
        let end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
        self.pos += end;
        Ok(rest[..end].to_string())
    }

    /// Skip past `end`, which must occur.
    fn skip_past(&mut self, end: &str, what: &str) -> Result<&'a str, String> {
        let rest = self.rest();
        let Some(i) = rest.find(end) else {
            return Err(self.err(&format!("unterminated {what}")));
        };
        self.pos += i + end.len();
        Ok(&rest[..i])
    }

    /// Children until `</parent>` (or the end of input for the document).
    fn content(&mut self, parent: Option<&str>, depth: usize) -> Result<Vec<RawNode>, String> {
        let mut out = Vec::new();
        loop {
            if self.pos >= self.s.len() {
                return match parent {
                    Some(name) => Err(self.err(&format!("<{name}> is not closed"))),
                    None => Ok(out),
                };
            }
            let rest = self.rest();
            if rest.starts_with("<?") {
                self.skip_past("?>", "processing instruction")?;
            } else if rest.starts_with("<!--") {
                self.pos += 4;
                self.skip_past("-->", "comment")?;
            } else if rest.starts_with("<![CDATA[") {
                self.pos += 9;
                let text = self.skip_past("]]>", "CDATA section")?;
                out.push(RawNode::Text(text.to_string()));
            } else if rest.starts_with("<!") {
                return Err(self.err("DOCTYPE and entity declarations are not allowed"));
            } else if rest.starts_with("</") {
                self.pos += 2;
                let name = self.name()?;
                self.skip_ws();
                if !self.rest().starts_with('>') {
                    return Err(self.err("expected `>`"));
                }
                self.pos += 1;
                return match parent {
                    Some(open) if open == name => Ok(out),
                    Some(open) => Err(self.err(&format!("</{name}> closes <{open}>"))),
                    None => Err(self.err(&format!("</{name}> without an open element"))),
                };
            } else if rest.starts_with('<') {
                if depth >= MAX_DEPTH {
                    return Err(self.err("elements nested too deeply"));
                }
                self.elements += 1;
                if self.elements > MAX_ELEMENTS {
                    return Err(self.err("too many elements"));
                }
                self.pos += 1;
                out.push(RawNode::El(self.element(depth + 1)?));
            } else {
                let end = rest.find('<').unwrap_or(rest.len());
                let text = decode(&rest[..end]).map_err(|e| self.err(&e))?;
                self.pos += end;
                out.push(RawNode::Text(text));
            }
        }
    }

    /// An element after its `<`.
    fn element(&mut self, depth: usize) -> Result<RawEl, String> {
        let name = self.name()?;
        let mut attrs: Vec<(String, String)> = Vec::new();
        loop {
            let before = self.pos;
            self.skip_ws();
            let rest = self.rest();
            if rest.starts_with("/>") {
                self.pos += 2;
                return Ok(RawEl {
                    name,
                    attrs,
                    children: Vec::new(),
                });
            }
            if rest.starts_with('>') {
                self.pos += 1;
                let children = self.content(Some(&name), depth)?;
                return Ok(RawEl {
                    name,
                    attrs,
                    children,
                });
            }
            if self.pos == before {
                return Err(self.err("expected whitespace before an attribute"));
            }
            let attr = self.name()?;
            self.skip_ws();
            if !self.rest().starts_with('=') {
                return Err(self.err(&format!("attribute {attr} has no value")));
            }
            self.pos += 1;
            self.skip_ws();
            let quote = self.rest().chars().next();
            let Some(quote @ ('"' | '\'')) = quote else {
                return Err(self.err("expected a quoted attribute value"));
            };
            self.pos += 1;
            let raw = self.skip_past(&quote.to_string(), "attribute value")?;
            if raw.contains('<') {
                return Err(self.err("`<` in an attribute value"));
            }
            let value = decode(raw).map_err(|e| self.err(&e))?;
            if attrs.iter().any(|(a, _)| *a == attr) {
                return Err(self.err(&format!("duplicate attribute {attr}")));
            }
            if attrs.len() >= MAX_ATTRS {
                return Err(self.err("too many attributes"));
            }
            attrs.push((attr, value));
        }
    }
}

/// Decode the predefined entities and character references.
fn decode(s: &str) -> Result<String, String> {
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            return Err("`&` that is not an entity".into());
        };
        let entity = &rest[..end];
        let c = match entity {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(dec) = entity.strip_prefix('#') {
                    dec.parse::<u32>().ok()
                } else {
                    return Err(format!("entity &{entity}; is not allowed"));
                };
                code.and_then(char::from_u32)
                    .filter(|c| *c != '\0')
                    .ok_or_else(|| format!("invalid character reference &{entity};"))?
            }
        };
        out.push(c);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn parse(input: &str) -> Result<RawEl, String> {
    if input.len() > MAX_SVG_BYTES {
        return Err(format!(
            "SVG is {} bytes; the limit is {MAX_SVG_BYTES}",
            input.len()
        ));
    }
    let mut reader = Reader {
        s: input.strip_prefix('\u{feff}').unwrap_or(input),
        pos: 0,
        elements: 0,
    };
    let nodes = reader.content(None, 0)?;
    let mut root = None;
    for node in nodes {
        match node {
            RawNode::Text(t) if t.trim().is_empty() => {}
            RawNode::Text(_) => return Err("invalid SVG: text outside the root element".into()),
            RawNode::El(el) if root.is_none() => root = Some(el),
            RawNode::El(_) => return Err("invalid SVG: more than one root element".into()),
        }
    }
    let root = root.ok_or("invalid SVG: no root element")?;
    if root.name != "svg" {
        return Err(format!(
            "invalid SVG: the root element is <{}>, not <svg>",
            root.name
        ));
    }
    Ok(root)
}

// ---- sanitizing ----

/// Text safe to emit inside a double-quoted XML attribute and a JSX
/// string attribute: no quotes, markup, entity, template or escape
/// characters, no control characters (whitespace is folded to spaces).
fn safe_text(value: &str) -> Option<String> {
    let folded: String = value
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let folded = folded.trim().to_string();
    let bad = |c: char| {
        matches!(c, '"' | '\'' | '<' | '>' | '&' | '`' | '{' | '}' | '\\') || c.is_control()
    };
    (!folded.chars().any(bad)).then_some(folded)
}

fn is_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// `url(#id)` (optionally quoted) → `id`.
fn local_url(value: &str) -> Option<&str> {
    let inner = value.trim().strip_prefix("url(")?.strip_suffix(')')?.trim();
    let inner = inner
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| inner.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(inner);
    inner.strip_prefix('#').filter(|id| is_id(id))
}

/// A base64 raster `data:` URI (the only external-looking `href` kept, on
/// `<image>`): it embeds its bytes, it fetches nothing.
fn is_raster_data_uri(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("data:image/") else {
        return false;
    };
    let Some((kind, data)) = rest.split_once(";base64,") else {
        return false;
    };
    matches!(kind, "png" | "jpeg" | "gif" | "webp")
        && !data.is_empty()
        && data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

fn canonical<'a>(list: &[&'a str], name: &str) -> Option<&'a str> {
    list.iter().copied().find(|n| *n == name)
}

/// Pending reference before ids are numbered.
enum PreVal {
    Lit(String),
    Id(String),
    Url(String),
    Hash(String),
    Style(Vec<(&'static str, String)>),
}

struct PreEl {
    name: &'static str,
    attrs: Vec<(&'static str, PreVal)>,
    children: Vec<PreNode>,
}

enum PreNode {
    El(PreEl),
    Text(String),
}

struct Sanitizer {
    removed: BTreeSet<String>,
    /// Every id defined, in document order (first definition wins).
    defined: Vec<String>,
}

impl Sanitizer {
    fn element(&mut self, raw: RawEl, in_text: bool) -> Option<PreEl> {
        let Some(name) = canonical(&ELEMENTS, &raw.name) else {
            self.removed.insert(format!("dropped <{}>", raw.name));
            return None;
        };
        let mut attrs = Vec::new();
        for (attr, value) in raw.attrs {
            if attr == "xmlns" || attr.starts_with("xmlns:") {
                // Namespace declarations: the output declares its own.
                continue;
            }
            let key = if attr == "xlink:href" { "href" } else { &attr };
            let Some((svg_name, _)) = ATTRS.iter().find(|(svg, _)| *svg == key) else {
                self.removed
                    .insert(format!("dropped attribute {attr} on <{name}>"));
                continue;
            };
            if attrs.iter().any(|(a, _)| a == svg_name) {
                self.removed
                    .insert(format!("dropped duplicate attribute {attr} on <{name}>"));
                continue;
            }
            match self.value(name, svg_name, &value) {
                Some(v) => attrs.push((*svg_name, v)),
                None => {
                    self.removed
                        .insert(format!("dropped unsafe {attr} value on <{name}>"));
                }
            }
        }
        let keeps_text = TEXT_ELEMENTS.contains(&name) || in_text;
        let mut children = Vec::new();
        for child in raw.children {
            match child {
                RawNode::El(el) => {
                    if let Some(el) = self.element(el, keeps_text) {
                        children.push(PreNode::El(el));
                    }
                }
                RawNode::Text(t) if keeps_text => {
                    let text: String = t
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect();
                    if !text.trim().is_empty() {
                        children.push(PreNode::Text(text));
                    }
                }
                RawNode::Text(_) => {}
            }
        }
        Some(PreEl {
            name,
            attrs,
            children,
        })
    }

    fn value(&mut self, element: &str, attr: &'static str, value: &str) -> Option<PreVal> {
        match attr {
            "id" => {
                let id = value.trim();
                if !is_id(id) {
                    return None;
                }
                if !self.defined.iter().any(|d| d == id) {
                    self.defined.push(id.to_string());
                }
                Some(PreVal::Id(id.to_string()))
            }
            "href" => {
                let v = value.trim();
                if let Some(id) = v.strip_prefix('#').filter(|id| is_id(id)) {
                    Some(PreVal::Hash(id.to_string()))
                } else if element == "image" && is_raster_data_uri(v) {
                    Some(PreVal::Lit(v.to_string()))
                } else {
                    None
                }
            }
            "style" => {
                let mut kept = Vec::new();
                for decl in value.split(';') {
                    let Some((prop, val)) = decl.split_once(':') else {
                        if !decl.trim().is_empty() {
                            self.removed
                                .insert(format!("dropped style declaration on <{element}>"));
                        }
                        continue;
                    };
                    let (prop, val) = (prop.trim(), val.trim());
                    let keyword = !val.is_empty()
                        && val.len() <= 32
                        && val.chars().all(|c| c.is_ascii_alphabetic() || c == '-');
                    match STYLE_PROPS.iter().find(|(p, _)| *p == prop) {
                        Some((p, _)) if keyword && !kept.iter().any(|(k, _)| k == p) => {
                            kept.push((*p, val.to_string()));
                        }
                        _ => {
                            self.removed.insert(format!(
                                "dropped style declaration {} on <{element}>",
                                safe_text(prop)
                                    .filter(|p| p.len() <= 40 && !p.is_empty())
                                    .unwrap_or_else(|| "(unnamed)".into())
                            ));
                        }
                    }
                }
                (!kept.is_empty()).then_some(PreVal::Style(kept))
            }
            _ => {
                let lower = value.to_ascii_lowercase();
                if lower.contains("url(") {
                    let refs_allowed = PAINT_ATTRS.contains(&attr) || REF_ATTRS.contains(&attr);
                    return local_url(value)
                        .filter(|_| refs_allowed)
                        .map(|id| PreVal::Url(id.to_string()));
                }
                if REF_ATTRS.contains(&attr) && value.trim() != "none" {
                    return None;
                }
                if lower.contains("javascript:") || lower.contains("data:") {
                    return None;
                }
                safe_text(value).map(PreVal::Lit)
            }
        }
    }
}

/// Number the ids that are both defined and referenced; resolve every
/// reference, dropping those to undefined ids.
struct Numbering {
    index: BTreeMap<String, usize>,
}

fn collect_refs(el: &PreEl, out: &mut BTreeSet<String>) {
    for (_, v) in &el.attrs {
        if let PreVal::Url(id) | PreVal::Hash(id) = v {
            out.insert(id.clone());
        }
    }
    for c in &el.children {
        if let PreNode::El(e) = c {
            collect_refs(e, out);
        }
    }
}

impl Numbering {
    /// `emitted`: ids already given to an element (a later duplicate
    /// definition loses its id).
    fn finish(
        &self,
        el: PreEl,
        removed: &mut BTreeSet<String>,
        emitted: &mut BTreeSet<usize>,
    ) -> Option<El> {
        let mut attrs = Vec::new();
        for (name, v) in el.attrs {
            let v = match v {
                PreVal::Lit(s) => Val::Lit(s),
                PreVal::Style(s) => Val::Style(s),
                // Unreferenced ids are dropped (optimization).
                PreVal::Id(id) => match self.index.get(&id) {
                    Some(i) if emitted.insert(*i) => Val::Id(*i),
                    Some(_) => {
                        removed.insert(format!("dropped duplicate id on <{}>", el.name));
                        continue;
                    }
                    None => continue,
                },
                PreVal::Url(id) => match self.index.get(&id) {
                    Some(i) => Val::Url(*i),
                    None => {
                        removed.insert(format!(
                            "dropped {name} reference to an undefined id on <{}>",
                            el.name
                        ));
                        continue;
                    }
                },
                PreVal::Hash(id) => match self.index.get(&id) {
                    Some(i) => Val::Hash(*i),
                    None => {
                        removed.insert(format!(
                            "dropped {name} reference to an undefined id on <{}>",
                            el.name
                        ));
                        continue;
                    }
                },
            };
            attrs.push((name, v));
        }
        // A `<use>` or `<image>` with nothing to show is pointless.
        if matches!(el.name, "use" | "image") && !attrs.iter().any(|(n, _)| *n == "href") {
            return None;
        }
        let mut children = Vec::new();
        for c in el.children {
            match c {
                PreNode::Text(t) => children.push(Node::Text(t)),
                PreNode::El(e) => {
                    let Some(e) = self.finish(e, removed, emitted) else {
                        continue;
                    };
                    // Attribute-less groups are unwrapped; empty defs and
                    // empty groups vanish.
                    if e.name == "g" && e.attrs.is_empty() {
                        children.extend(e.children);
                    } else if matches!(e.name, "defs" | "g") && e.children.is_empty() {
                        continue;
                    } else {
                        children.push(Node::El(e));
                    }
                }
            }
        }
        Some(El {
            name: el.name,
            attrs,
            children,
        })
    }
}

/// Parse and sanitize `input` (see the module doc).
pub(crate) fn sanitize(input: &str) -> Result<Sanitized, String> {
    let raw = parse(input)?;
    let mut s = Sanitizer {
        removed: BTreeSet::new(),
        defined: Vec::new(),
    };
    let pre = s
        .element(raw, false)
        .expect("the root is <svg>, which is allowed");
    let mut refs = BTreeSet::new();
    collect_refs(&pre, &mut refs);
    let mut index = BTreeMap::new();
    for id in &s.defined {
        if refs.contains(id) {
            let n = index.len();
            index.insert(id.clone(), n);
        }
    }
    let numbering = Numbering { index };
    let mut removed = s.removed;
    let root = numbering
        .finish(pre, &mut removed, &mut BTreeSet::new())
        .expect("the root is never a <use>");
    Ok(Sanitized {
        root,
        ids: numbering.index.len(),
        removed: removed.into_iter().collect(),
    })
}

// ---- colors ----

/// A paint value's color, normalized (`#rrggbb`, lowercase), or `None`
/// for `none`, `transparent` and `currentColor`.
fn color_of(value: &str) -> Option<String> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "none" | "transparent" | "currentcolor" | "inherit" => return None,
        "black" => return Some("#000000".into()),
        "white" => return Some("#ffffff".into()),
        _ => {}
    }
    if let Some(hex) = v.strip_prefix('#')
        && hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Some(match hex.len() {
            3 => format!("#{}", hex.chars().flat_map(|c| [c, c]).collect::<String>()),
            _ => format!("#{hex}"),
        });
    }
    Some(v)
}

impl Sanitized {
    /// The icon's one paint color (see the module doc), normalized.
    pub(crate) fn single_color(&self) -> Option<String> {
        fn walk(el: &El, colors: &mut BTreeSet<String>, flat: &mut bool) {
            if matches!(el.name, "clipPath" | "mask") {
                return;
            }
            if NON_FLAT.contains(&el.name) {
                *flat = false;
            }
            for (name, v) in &el.attrs {
                match v {
                    Val::Url(_) if PAINT_ATTRS.contains(name) => *flat = false,
                    Val::Lit(s) if PAINT_ATTRS.contains(name) || *name == "color" => {
                        colors.extend(color_of(s));
                    }
                    _ => {}
                }
            }
            for c in &el.children {
                if let Node::El(e) = c {
                    walk(e, colors, flat);
                }
            }
        }
        let mut colors = BTreeSet::new();
        let mut flat = true;
        walk(&self.root, &mut colors, &mut flat);
        (flat && colors.len() == 1)
            .then(|| colors.into_iter().next())
            .flatten()
    }

    /// Replace `color` (as [`Sanitized::single_color`] returned it) with
    /// `currentColor` wherever it paints, outside `<clipPath>`/`<mask>`.
    pub(crate) fn use_current_color(&mut self, color: &str) {
        fn walk(el: &mut El, color: &str) {
            if matches!(el.name, "clipPath" | "mask") {
                return;
            }
            for (name, v) in &mut el.attrs {
                if let Val::Lit(s) = v
                    && (PAINT_ATTRS.contains(name) || *name == "color")
                    && color_of(s).as_deref() == Some(color)
                {
                    *s = "currentColor".into();
                }
            }
            for c in &mut el.children {
                if let Node::El(e) = c {
                    walk(e, color);
                }
            }
        }
        walk(&mut self.root, color);
    }

    /// Whether the output defines any id (and so needs a per-instance
    /// prefix when inlined).
    pub(crate) fn has_ids(&self) -> bool {
        self.ids > 0
    }

    // ---- output ----

    /// Standalone SVG markup, one element per line, ids as
    /// `<prefix>-<n>`. `prefix` must be id-safe.
    pub(crate) fn to_svg(&self, prefix: &str) -> String {
        let mut out = String::new();
        self.svg_el(&self.root, 0, prefix, &mut out);
        out
    }

    fn svg_el(&self, el: &El, depth: usize, prefix: &str, out: &mut String) {
        let indent = "  ".repeat(depth);
        out.push_str(&indent);
        out.push('<');
        out.push_str(el.name);
        if depth == 0 {
            out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
        }
        for (name, v) in &el.attrs {
            let value = match v {
                Val::Lit(s) => s.clone(),
                Val::Id(i) => format!("{prefix}-{i}"),
                Val::Url(i) => format!("url(#{prefix}-{i})"),
                Val::Hash(i) => format!("#{prefix}-{i}"),
                Val::Style(decls) => decls
                    .iter()
                    .map(|(p, v)| format!("{p}:{v}"))
                    .collect::<Vec<_>>()
                    .join(";"),
            };
            out.push_str(&format!(" {name}=\"{value}\""));
        }
        if el.children.is_empty() {
            out.push_str("/>\n");
            return;
        }
        let texty = el.children.iter().any(|c| matches!(c, Node::Text(_)));
        if texty {
            out.push('>');
            for c in &el.children {
                match c {
                    Node::Text(t) => out.push_str(&xml_escape(t)),
                    Node::El(e) => {
                        let mut inner = String::new();
                        self.svg_el(e, 0, prefix, &mut inner);
                        out.push_str(inner.trim_end());
                    }
                }
            }
        } else {
            out.push_str(">\n");
            for c in &el.children {
                if let Node::El(e) = c {
                    self.svg_el(e, depth + 1, prefix, out);
                }
            }
            out.push_str(&indent);
        }
        out.push_str(&format!("</{}>\n", el.name));
    }

    /// JSX for the element tree, indented by `depth` levels of two spaces.
    /// Ids are `` `${<uid>}-<n>` `` (`uid` is a JS expression in scope);
    /// `root_extra` is spliced verbatim after the root's own attributes
    /// (e.g. `{...props}`), so callers can override them.
    pub(crate) fn to_jsx(&self, depth: usize, uid: &str, root_extra: &[&str]) -> String {
        let mut out = String::new();
        jsx_el(&self.root, depth, uid, root_extra, &mut out);
        out
    }
}

fn jsx_el(el: &El, depth: usize, uid: &str, extra: &[&str], out: &mut String) {
    let indent = "  ".repeat(depth);
    let mut attrs: Vec<String> = Vec::new();
    if el.name == "svg" && !extra.is_empty() {
        attrs.push("xmlns=\"http://www.w3.org/2000/svg\"".into());
    }
    for (name, v) in &el.attrs {
        let jsx_name = ATTRS
            .iter()
            .find(|(svg, _)| svg == name)
            .map(|(_, jsx)| *jsx)
            .expect("only allowlisted attributes are kept");
        let value = match v {
            Val::Lit(s) => format!("\"{s}\""),
            Val::Id(i) => format!("{{`${{{uid}}}-{i}`}}"),
            Val::Url(i) => format!("{{`url(#${{{uid}}}-{i})`}}"),
            Val::Hash(i) => format!("{{`#${{{uid}}}-{i}`}}"),
            Val::Style(decls) => {
                let body = decls
                    .iter()
                    .map(|(p, v)| {
                        let react = STYLE_PROPS
                            .iter()
                            .find(|(svg, _)| svg == p)
                            .map(|(_, r)| *r)
                            .expect("only allowlisted style properties are kept");
                        format!("{react}: \"{v}\"")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{{ {body} }}}}")
            }
        };
        attrs.push(format!("{jsx_name}={value}"));
    }
    if el.name == "svg" {
        attrs.extend(extra.iter().map(|s| s.to_string()));
    }
    out.push_str(&indent);
    out.push('<');
    out.push_str(el.name);
    for a in &attrs {
        out.push(' ');
        out.push_str(a);
    }
    if el.children.is_empty() {
        out.push_str(" />\n");
        return;
    }
    out.push_str(">\n");
    for c in &el.children {
        match c {
            Node::El(e) => jsx_el(e, depth + 1, uid, &[], out),
            Node::Text(t) => {
                out.push_str(&"  ".repeat(depth + 1));
                out.push('{');
                out.push_str(&serde_json::to_string(t).expect("strings serialize"));
                out.push_str("}\n");
            }
        }
    }
    out.push_str(&indent);
    out.push_str(&format!("</{}>\n", el.name));
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIGMA_LIKE: &str = r##"<?xml version="1.0"?>
<svg width="24" height="24" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
<g clip-path="url(#clip0_1_2)">
<path d="M5 12H19M19 12L12 5" stroke="#0F172A" stroke-width="2" stroke-linecap="round"/>
<circle cx="12" cy="12" r="2" fill="#0f172a"/>
</g>
<defs>
<clipPath id="clip0_1_2">
<rect width="24" height="24" fill="white"/>
</clipPath>
</defs>
</svg>
"##;

    #[test]
    fn keeps_figma_shape_and_renumbers_ids() {
        let s = sanitize(FIGMA_LIKE).unwrap();
        assert!(s.removed.is_empty(), "{:?}", s.removed);
        assert_eq!(
            s.to_svg("icon-a"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\" fill=\"none\">\n  <g clip-path=\"url(#icon-a-0)\">\n    <path d=\"M5 12H19M19 12L12 5\" stroke=\"#0F172A\" stroke-width=\"2\" stroke-linecap=\"round\"/>\n    <circle cx=\"12\" cy=\"12\" r=\"2\" fill=\"#0f172a\"/>\n  </g>\n  <defs>\n    <clipPath id=\"icon-a-0\">\n      <rect width=\"24\" height=\"24\" fill=\"white\"/>\n    </clipPath>\n  </defs>\n</svg>\n"
        );
        assert!(s.has_ids());
    }

    #[test]
    fn single_color_ignores_clip_geometry_and_converts_on_request() {
        let mut s = sanitize(FIGMA_LIKE).unwrap();
        assert_eq!(s.single_color().as_deref(), Some("#0f172a"));
        s.use_current_color("#0f172a");
        let out = s.to_svg("p");
        assert!(out.contains("stroke=\"currentColor\""), "{out}");
        assert!(out.contains("fill=\"currentColor\""), "{out}");
        // The clip rectangle is geometry: untouched.
        assert!(out.contains("fill=\"white\""), "{out}");
        // The root's `none` is not a color.
        assert!(out.contains("fill=\"none\""), "{out}");
    }

    #[test]
    fn multicolor_and_gradients_are_not_single_color() {
        let two = r##"<svg viewBox="0 0 2 2"><rect width="1" height="1" fill="#f00"/><rect x="1" width="1" height="1" fill="#00f"/></svg>"##;
        assert_eq!(sanitize(two).unwrap().single_color(), None);
        let gradient = r##"<svg viewBox="0 0 2 2"><rect width="2" height="2" fill="url(#g)"/><defs><linearGradient id="g"><stop stop-color="#f00"/><stop offset="1" stop-color="#f00"/></linearGradient></defs></svg>"##;
        assert_eq!(sanitize(gradient).unwrap().single_color(), None);
        let none = r#"<svg viewBox="0 0 2 2" fill="none"/>"#;
        assert_eq!(sanitize(none).unwrap().single_color(), None);
    }

    #[test]
    fn strips_scripts_handlers_foreign_objects_styles_and_external_refs() {
        let evil = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" onload="alert(1)" viewBox="0 0 10 10">
<script>alert(1)</script>
<style>@import url(https://evil.example/x.css); path{fill:red}</style>
<foreignObject width="10" height="10"><div xmlns="http://www.w3.org/1999/xhtml">hi</div></foreignObject>
<a href="https://evil.example"><path d="M0 0"/></a>
<path d="M1 1" onclick="steal()" fill="url(https://evil.example/p.svg#x)" class="c"/>
<use xlink:href="https://evil.example/sprite.svg#icon"/>
<use href="javascript:alert(1)"/>
<image href="https://evil.example/track.png" width="1" height="1"/>
<image href="data:image/svg+xml;base64,PHN2Zz4=" width="1" height="1"/>
<rect width="1" height="1" style="fill:url(https://evil.example);mask-type:alpha" filter="url(https://evil.example/f)"/>
<rect width="2" height="2" fill="red&#34;onload=&#34;x"/>
<feImage href="https://evil.example"/>
<animate attributeName="href" to="javascript:alert(1)"/>
<set attributeName="onload" to="alert(1)"/>
</svg>"##;
        let s = sanitize(evil).unwrap();
        let out = s.to_svg("p");
        let lower = out.to_ascii_lowercase();
        for needle in [
            "script",
            "onload",
            "onclick",
            "style>",
            "@import",
            "foreignobject",
            "evil",
            "javascript",
            "<a",
            "class",
            "svg+xml",
            "animate",
            "<set",
            "feimage",
            "<use",
            "\"red",
        ] {
            assert!(!lower.contains(needle), "{needle} survived: {out}");
        }
        // The safe parts stay.
        assert!(out.contains("style=\"mask-type:alpha\""), "{out}");
        assert!(out.contains("<path d=\"M1 1\"/>"), "{out}");
        assert!(s.removed.contains(&"dropped <script>".to_string()));
        assert!(
            s.removed
                .contains(&"dropped attribute onclick on <path>".to_string())
        );
        // Images: external dropped (and an <image> without href is kept
        // as an empty box — harmless).
        assert!(!out.contains("href"), "{out}");
    }

    #[test]
    fn raster_data_uris_are_kept_on_images_only() {
        let svg = r##"<svg viewBox="0 0 1 1"><image width="1" height="1" href="data:image/png;base64,iVBORw0KGgo="/><rect width="1" height="1" fill="url(#p)"/><defs><pattern id="p"><use href="#i"/></pattern><image id="i" xlink:href="data:image/jpeg;base64,/9j/4AAQ"/></defs></svg>"##;
        let s = sanitize(svg).unwrap();
        let out = s.to_svg("x");
        assert!(out.contains("href=\"data:image/png;base64,iVBORw0KGgo=\""));
        assert!(out.contains("<use href=\"#x-1\"/>"), "{out}");
        assert!(out.contains("<image id=\"x-1\" href=\"data:image/jpeg"));
        assert_eq!(s.single_color(), None);
    }

    #[test]
    fn rejects_doctype_entities_and_malformed_markup() {
        for bad in [
            "<!DOCTYPE svg [<!ENTITY x \"y\">]><svg>&x;</svg>",
            "<svg><text>&x;</text></svg>",
            "<svg><g></svg>",
            "<svg><g></path></g></svg>",
            "<svg a=\"1\" a=\"2\"/>",
            "<svg a=1/>",
            "<html/>",
            "<svg/><svg/>",
            "",
            "text<svg/>",
            "<svg><text>a & b</text></svg>",
        ] {
            assert!(sanitize(bad).is_err(), "accepted {bad:?}");
        }
        let deep = format!("<svg>{}{}</svg>", "<g>".repeat(70), "</g>".repeat(70));
        assert!(sanitize(&deep).unwrap_err().contains("nested too deeply"));
    }

    #[test]
    fn text_is_escaped_in_both_outputs() {
        let svg = r#"<svg viewBox="0 0 1 1"><title>a &lt;b&gt; &amp; "c" {d} `e`</title></svg>"#;
        let s = sanitize(svg).unwrap();
        assert!(
            s.to_svg("x")
                .contains("<title>a &lt;b&gt; &amp; \"c\" {d} `e`</title>")
        );
        let jsx = s.to_jsx(0, "uid", &["{...props}"]);
        assert!(jsx.contains("{\"a <b> & \\\"c\\\" {d} `e`\"}"), "{jsx}");
    }

    #[test]
    fn jsx_uses_react_names_and_prefixed_ids() {
        let s = sanitize(FIGMA_LIKE).unwrap();
        let jsx = s.to_jsx(2, "uid", &["{...props}"]);
        assert_eq!(
            jsx,
            "    <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\" fill=\"none\" {...props}>\n      <g clipPath={`url(#${uid}-0)`}>\n        <path d=\"M5 12H19M19 12L12 5\" stroke=\"#0F172A\" strokeWidth=\"2\" strokeLinecap=\"round\" />\n        <circle cx=\"12\" cy=\"12\" r=\"2\" fill=\"#0f172a\" />\n      </g>\n      <defs>\n        <clipPath id={`${uid}-0`}>\n          <rect width=\"24\" height=\"24\" fill=\"white\" />\n        </clipPath>\n      </defs>\n    </svg>\n"
        );
    }

    #[test]
    fn undefined_references_and_unused_ids_are_dropped() {
        let svg = r##"<svg viewBox="0 0 1 1"><rect id="unused" width="1" height="1" fill="url(#missing)" mask="url(#m)"/><mask id="m" style="mask-type:luminance"><rect width="1" height="1" fill="white"/></mask></svg>"##;
        let s = sanitize(svg).unwrap();
        let out = s.to_svg("p");
        assert!(!out.contains("unused"), "{out}");
        assert!(!out.contains("missing"), "{out}");
        assert!(out.contains("mask=\"url(#p-0)\""), "{out}");
        assert!(out.contains("<mask id=\"p-0\" style=\"mask-type:luminance\">"));
        assert_eq!(s.ids, 1);
    }
}
