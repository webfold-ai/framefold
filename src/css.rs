//! CSS parsing and the cascade.
//!
//! Enough CSS to compute the styles a composition's own stylesheet specifies —
//! selectors, specificity, source order, inheritance, and the shorthand
//! expansions real stylesheets use. Not a full CSS engine: no media queries,
//! no pseudo-classes, no `calc()`, no custom properties, no `!important`
//! (accepted and stripped, since ignoring it silently would mis-cascade).
//!
//! # Why specificity is computed rather than approximated
//!
//! glossa's stylesheet sets `.scene { background: #f8f8f8 }` and later
//! `#s4 { background: #1b1b1f }`. Get the ordering wrong by treating source
//! order as decisive and the outro scene renders on the wrong background —
//! a whole-frame error from one mis-ranked rule. Specificity is
//! (ids, classes, tags), compared before source order, exactly as CSS says.

use crate::html::Element;
use std::collections::HashMap;

/// Properties children inherit when they do not set them.
///
/// Inheriting the wrong set is a quiet failure: miss `letter-spacing` and text
/// is subtly too narrow everywhere; inherit `padding` and every nested box
/// gains space it never asked for.
const INHERITED: &[&str] = &[
    "color",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "line-height",
    "letter-spacing",
    "text-align",
    "text-transform",
    "white-space",
    "word-spacing",
    "visibility",
];

#[derive(Clone, Debug, PartialEq)]
pub struct FontFace {
    pub family: String,
    /// The `url(...)` target, relative to the stylesheet.
    pub src: String,
    /// As written — may be a single weight or a variable range like "300 700".
    pub weight: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Simple {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

impl Simple {
    fn matches(&self, e: &Element) -> bool {
        if let Some(t) = &self.tag {
            if !t.eq_ignore_ascii_case(&e.tag) {
                return false;
            }
        }
        if let Some(i) = &self.id {
            if e.id() != Some(i.as_str()) {
                return false;
            }
        }
        let have = e.classes();
        self.classes.iter().all(|c| have.contains(&c.as_str()))
    }
}

/// A descendant chain: `.a .b c` is three simple selectors, rightmost last.
#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    parts: Vec<Simple>,
}

impl Selector {
    /// Specificity as (ids, classes, tags) — the CSS a/b/c triple minus inline
    /// styles, which a stylesheet cannot express.
    fn specificity(&self) -> (u32, u32, u32) {
        let mut s = (0, 0, 0);
        for p in &self.parts {
            if p.id.is_some() {
                s.0 += 1;
            }
            s.1 += p.classes.len() as u32;
            if p.tag.is_some() {
                s.2 += 1;
            }
        }
        s
    }

    /// Match against an ancestor path ending at the element under test.
    ///
    /// Walked right to left: the last part must match the element itself, and
    /// each earlier part must match *some* ancestor, in order. Greedy matching
    /// is correct here because descendant combinators impose no adjacency.
    fn matches(&self, path: &[&Element]) -> bool {
        let Some(last) = path.last() else { return false };
        let Some((target, ancestors)) = self.parts.split_last() else {
            return false;
        };
        if !target.matches(last) {
            return false;
        }
        let mut i = path.len() - 1;
        for part in ancestors.iter().rev() {
            let mut found = false;
            while i > 0 {
                i -= 1;
                if part.matches(path[i]) {
                    found = true;
                    break;
                }
            }
            if !found {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Debug)]
struct Rule {
    selectors: Vec<Selector>,
    decls: Vec<(String, String)>,
    order: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    pub font_faces: Vec<FontFace>,
}

/// A computed property map for one element.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Styles(pub HashMap<String, String>);

impl Styles {
    pub fn get(&self, prop: &str) -> Option<&str> {
        self.0.get(prop).map(|s| s.as_str())
    }

    /// A length in pixels, resolving `em`/`rem` against `font_size` and `%`
    /// against `basis`.
    pub fn px(&self, prop: &str, font_size: f32, basis: f32) -> Option<f32> {
        self.get(prop).and_then(|v| length(v, font_size, basis))
    }

    pub fn color(&self, prop: &str) -> Option<crate::raster::Rgba> {
        self.get(prop).and_then(parse_color)
    }

    pub fn is(&self, prop: &str, value: &str) -> bool {
        self.get(prop) == Some(value)
    }
}

/// Parse a length. Unitless zero is allowed; other unitless numbers are not,
/// since a bare `12` is a stylesheet bug rather than 12 pixels.
pub fn length(v: &str, font_size: f32, basis: f32) -> Option<f32> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse::<f32>().ok();
    }
    if let Some(n) = v.strip_suffix("rem") {
        // No root-font-size tracking: compositions set sizes in px, and rem
        // appears only in authored spacing where 16 is the browser default.
        return n.trim().parse::<f32>().ok().map(|x| x * 16.0);
    }
    if let Some(n) = v.strip_suffix("em") {
        return n.trim().parse::<f32>().ok().map(|x| x * font_size);
    }
    if let Some(n) = v.strip_suffix('%') {
        return n.trim().parse::<f32>().ok().map(|x| x / 100.0 * basis);
    }
    match v.parse::<f32>() {
        Ok(n) if n == 0.0 => Some(0.0),
        _ => None,
    }
}

/// Parse a colour: hex, `rgb()`, `rgba()`, or the handful of named colours a
/// composition uses.
pub fn parse_color(v: &str) -> Option<crate::raster::Rgba> {
    use crate::raster::Rgba;
    let v = v.trim();
    if v.starts_with('#') {
        return Rgba::from_hex(v);
    }
    if let Some(inner) = v.strip_prefix("rgba(").or_else(|| v.strip_prefix("rgb(")) {
        let inner = inner.strip_suffix(')')?;
        let parts: Vec<&str> = inner.split(&[',', '/'][..]).map(|s| s.trim()).collect();
        if parts.len() < 3 {
            return None;
        }
        let c = |s: &str| s.parse::<f32>().ok().map(|x| x.clamp(0.0, 255.0) as u8);
        let a = if parts.len() > 3 {
            let raw = parts[3].trim_end_matches('%');
            let f: f32 = raw.parse().ok()?;
            let f = if parts[3].ends_with('%') { f / 100.0 } else { f };
            (f.clamp(0.0, 1.0) * 255.0).round() as u8
        } else {
            255
        };
        return Some(Rgba::new(c(parts[0])?, c(parts[1])?, c(parts[2])?, a));
    }
    let named = [
        ("black", Rgba::BLACK),
        ("white", Rgba::WHITE),
        ("transparent", Rgba::TRANSPARENT),
        ("red", Rgba::rgb(255, 0, 0)),
        ("green", Rgba::rgb(0, 128, 0)),
        ("blue", Rgba::rgb(0, 0, 255)),
        ("gray", Rgba::rgb(128, 128, 128)),
        ("grey", Rgba::rgb(128, 128, 128)),
    ];
    named
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(v))
        .map(|(_, c)| *c)
}

impl Stylesheet {
    /// Compute styles for the element at the end of `path`, given its parent's
    /// already-computed styles.
    pub fn compute(&self, path: &[&Element], parent: Option<&Styles>) -> Styles {
        let mut out: HashMap<String, String> = HashMap::new();

        // Inherited properties start from the parent.
        if let Some(p) = parent {
            for k in INHERITED {
                if let Some(v) = p.0.get(*k) {
                    out.insert((*k).to_string(), v.clone());
                }
            }
        }

        // Collect matching declarations, then apply in cascade order so later
        // winners simply overwrite.
        let mut matched: Vec<((u32, u32, u32), usize, &Vec<(String, String)>)> = Vec::new();
        for rule in &self.rules {
            if let Some(best) = rule
                .selectors
                .iter()
                .filter(|s| s.matches(path))
                .map(|s| s.specificity())
                .max()
            {
                matched.push((best, rule.order, &rule.decls));
            }
        }
        matched.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        for (_, _, decls) in matched {
            for (k, v) in decls {
                expand_into(&mut out, k, v);
            }
        }

        // An inline style attribute outranks every stylesheet rule.
        if let Some(e) = path.last() {
            if let Some(style) = e.attr("style") {
                for (k, v) in parse_declarations(style) {
                    expand_into(&mut out, &k, &v);
                }
            }
        }

        Styles(out)
    }
}

/// Apply one declaration, expanding the shorthands compositions use.
fn expand_into(out: &mut HashMap<String, String>, prop: &str, value: &str) {
    let prop = prop.trim().to_ascii_lowercase();
    let value = value.trim().trim_end_matches("!important").trim().to_string();

    let sides = |v: &str| -> [String; 4] {
        let parts: Vec<&str> = v.split_whitespace().collect();
        match parts.len() {
            1 => [parts[0], parts[0], parts[0], parts[0]],
            2 => [parts[0], parts[1], parts[0], parts[1]],
            3 => [parts[0], parts[1], parts[2], parts[1]],
            _ if parts.len() >= 4 => [parts[0], parts[1], parts[2], parts[3]],
            _ => ["0", "0", "0", "0"],
        }
        .map(|s| s.to_string())
    };

    match prop.as_str() {
        "margin" | "padding" => {
            let [t, r, b, l] = sides(&value);
            out.insert(format!("{prop}-top"), t);
            out.insert(format!("{prop}-right"), r);
            out.insert(format!("{prop}-bottom"), b);
            out.insert(format!("{prop}-left"), l);
        }
        "inset" => {
            let [t, r, b, l] = sides(&value);
            out.insert("top".into(), t);
            out.insert("right".into(), r);
            out.insert("bottom".into(), b);
            out.insert("left".into(), l);
        }
        "gap" => {
            let parts: Vec<&str> = value.split_whitespace().collect();
            let row = parts.first().copied().unwrap_or("0");
            let col = parts.get(1).copied().unwrap_or(row);
            out.insert("row-gap".into(), row.to_string());
            out.insert("column-gap".into(), col.to_string());
        }
        // `border: 1px solid #e2e2e2` — width is the length, colour is the
        // colour, style is whatever remains.
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let mut width = None;
            let mut color = None;
            for tok in value.split_whitespace() {
                if length(tok, 16.0, 0.0).is_some() {
                    width = Some(tok.to_string());
                } else if parse_color(tok).is_some() {
                    color = Some(tok.to_string());
                }
            }
            let targets: &[&str] = if prop == "border" {
                &["top", "right", "bottom", "left"]
            } else {
                match prop.as_str() {
                    "border-top" => &["top"],
                    "border-right" => &["right"],
                    "border-bottom" => &["bottom"],
                    _ => &["left"],
                }
            };
            for side in targets {
                if let Some(w) = &width {
                    out.insert(format!("border-{side}-width"), w.clone());
                }
                if let Some(c) = &color {
                    out.insert(format!("border-{side}-color"), c.clone());
                }
            }
        }
        // Only the colour form of `background` is meaningful here; gradients
        // and images are recorded verbatim so a caller can detect and decline
        // them rather than render a wrong solid fill.
        "background" => {
            if parse_color(&value).is_some() {
                out.insert("background-color".into(), value);
            } else {
                out.insert("background".into(), value);
            }
        }
        "flex" => {
            // `flex: 1` expands to `1 1 0%` — note the basis is ZERO, not the
            // item's content size. Growing from content width instead makes
            // three text-filled cards each keep their own width and overflow
            // the row, which is a silent and very visible layout bug.
            let parts: Vec<&str> = value.split_whitespace().collect();
            match parts.len() {
                1 if parts[0].parse::<f32>().is_ok() => {
                    out.insert("flex-grow".into(), parts[0].to_string());
                    out.insert("flex-shrink".into(), "1".into());
                    out.insert("flex-basis".into(), "0".into());
                }
                1 => {
                    // A single non-numeric value is a basis, e.g. `flex: auto`.
                    out.insert("flex-basis".into(), parts[0].to_string());
                }
                2 => {
                    out.insert("flex-grow".into(), parts[0].to_string());
                    if parts[1].parse::<f32>().is_ok() {
                        out.insert("flex-shrink".into(), parts[1].to_string());
                        out.insert("flex-basis".into(), "0".into());
                    } else {
                        out.insert("flex-basis".into(), parts[1].to_string());
                    }
                }
                _ => {
                    out.insert("flex-grow".into(), parts[0].to_string());
                    out.insert("flex-shrink".into(), parts[1].to_string());
                    out.insert("flex-basis".into(), parts[2].to_string());
                }
            }
            out.insert("flex".into(), value);
        }
        "font" => {
            // Not expanded — the shorthand's grammar is ambiguous without a
            // full parser, and misreading it would set the wrong size.
            out.insert("font".into(), value);
        }
        _ => {
            out.insert(prop, value);
        }
    }
}

/// Parse `a: 1; b: 2` into pairs.
fn parse_declarations(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in split_top_level(src, ';') {
        let Some(colon) = find_top_level(&chunk, ':') else {
            continue;
        };
        let k = chunk[..colon].trim().to_ascii_lowercase();
        let v = chunk[colon + 1..].trim().to_string();
        if !k.is_empty() && !v.is_empty() {
            out.push((k, v));
        }
    }
    out
}

/// Split on `sep`, ignoring separators inside parentheses or quotes — so
/// `inset(0 0% 0 0)` and `url("a;b.woff2")` survive intact.
fn split_top_level(src: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    for c in src.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
                continue;
            }
            None => {}
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn find_top_level(src: &str, target: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, c) in src.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                continue;
            }
            None => {}
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == target && depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_selector(src: &str) -> Option<Selector> {
    let mut parts = Vec::new();
    for token in src.split_whitespace() {
        // Combinators other than descendant are not supported; a selector
        // using one is dropped rather than silently treated as descendant,
        // which would apply it far too broadly.
        if token == ">" || token == "+" || token == "~" {
            return None;
        }
        let mut s = Simple::default();
        let mut cur = String::new();
        let mut kind = ' ';
        let flush = |s: &mut Simple, kind: char, cur: &mut String| {
            if cur.is_empty() {
                return;
            }
            match kind {
                '#' => s.id = Some(std::mem::take(cur)),
                '.' => s.classes.push(std::mem::take(cur)),
                _ => s.tag = Some(std::mem::take(cur)),
            }
        };
        for c in token.chars() {
            match c {
                '#' | '.' => {
                    flush(&mut s, kind, &mut cur);
                    kind = c;
                }
                '*' => kind = '*',
                ':' | '[' => return None, // pseudo-classes / attribute selectors
                c => cur.push(c),
            }
        }
        flush(&mut s, kind, &mut cur);
        if s.tag.is_none() && s.id.is_none() && s.classes.is_empty() && kind != '*' {
            return None;
        }
        parts.push(s);
    }
    (!parts.is_empty()).then_some(Selector { parts })
}

/// Parse a stylesheet.
pub fn parse(src: &str) -> Stylesheet {
    let mut sheet = Stylesheet::default();
    let bytes: Vec<char> = strip_comments(src).chars().collect();
    let mut i = 0usize;
    let mut order = 0usize;

    while i < bytes.len() {
        // Selector (or at-rule prelude) up to '{'.
        let start = i;
        while i < bytes.len() && bytes[i] != '{' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let prelude: String = bytes[start..i].iter().collect();
        i += 1; // '{'

        // Body up to the matching '}'.
        let body_start = i;
        let mut depth = 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        let body: String = bytes[body_start..i.saturating_sub(1)].iter().collect();
        let prelude = prelude.trim();

        if prelude.starts_with('@') {
            let lower = prelude.to_ascii_lowercase();
            if lower.starts_with("@font-face") {
                let decls = parse_declarations(&body);
                let get = |k: &str| {
                    decls
                        .iter()
                        .find(|(p, _)| p == k)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let family = get("font-family")
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string();
                let src_val = get("src");
                let url = src_val
                    .split("url(")
                    .nth(1)
                    .and_then(|s| s.split(')').next())
                    .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                    .unwrap_or_default();
                if !family.is_empty() && !url.is_empty() {
                    sheet.font_faces.push(FontFace {
                        family,
                        src: url,
                        weight: get("font-weight"),
                    });
                }
            }
            // Other at-rules (@media, @keyframes) are skipped wholesale: their
            // bodies are not plain declarations, and treating them as such
            // would leak nested rules into the global cascade.
            continue;
        }

        let selectors: Vec<Selector> = split_top_level(prelude, ',')
            .iter()
            .filter_map(|s| parse_selector(s.trim()))
            .collect();
        if selectors.is_empty() {
            continue;
        }
        let decls = parse_declarations(&body);
        if decls.is_empty() {
            continue;
        }
        sheet.rules.push(Rule {
            selectors,
            decls,
            order,
        });
        order += 1;
    }
    sheet
}

fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"/*") {
            match src[i + 2..].find("*/") {
                Some(e) => i = i + 2 + e + 2,
                None => break,
            }
            continue;
        }
        let start = i;
        while i < b.len() && !b[i..].starts_with(b"/*") {
            i += 1;
        }
        out.push_str(&src[start..i]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn doc(src: &str) -> html::Element {
        html::parse(src).unwrap()
    }

    /// Build the ancestor path to the first element matching a predicate.
    fn path_to<'a>(root: &'a html::Element, pred: &dyn Fn(&html::Element) -> bool) -> Vec<&'a html::Element> {
        fn walk<'a>(
            e: &'a html::Element,
            pred: &dyn Fn(&html::Element) -> bool,
            cur: &mut Vec<&'a html::Element>,
        ) -> bool {
            cur.push(e);
            if pred(e) {
                return true;
            }
            for c in &e.children {
                if let html::Node::Element(ch) = c {
                    if walk(ch, pred, cur) {
                        return true;
                    }
                }
            }
            cur.pop();
            false
        }
        let mut v = Vec::new();
        walk(root, pred, &mut v);
        v
    }

    fn computed(html_src: &str, css_src: &str, pred: &dyn Fn(&html::Element) -> bool) -> Styles {
        let root = doc(html_src);
        let sheet = parse(css_src);
        let path = path_to(&root, pred);
        assert!(!path.is_empty(), "target element not found");
        // Compute down the chain so inheritance is real.
        let mut parent: Option<Styles> = None;
        for n in 1..=path.len() {
            let s = sheet.compute(&path[..n], parent.as_ref());
            parent = Some(s);
        }
        parent.unwrap()
    }

    #[test]
    fn specificity_beats_source_order() {
        // The exact shape glossa relies on: a later class rule must NOT beat an
        // id rule, or scene 4 renders on the wrong background.
        let s = computed(
            r#"<div id="s4" class="scene"></div>"#,
            "#s4 { background: #1b1b1f } .scene { background: #f8f8f8 }",
            &|e| e.id() == Some("s4"),
        );
        assert_eq!(s.get("background-color"), Some("#1b1b1f"));
    }

    #[test]
    fn source_order_breaks_specificity_ties() {
        let s = computed(
            r#"<div class="a b"></div>"#,
            ".a { color: #111 } .b { color: #222 }",
            &|e| e.tag == "div",
        );
        assert_eq!(s.get("color"), Some("#222"));
    }

    #[test]
    fn descendant_selectors_match_through_ancestors() {
        let s = computed(
            r#"<div class="hook"><section><h1>x</h1></section></div>"#,
            ".hook h1 { font-size: 60px }",
            &|e| e.tag == "h1",
        );
        assert_eq!(s.get("font-size"), Some("60px"));
    }

    #[test]
    fn descendant_selector_does_not_match_outside_its_ancestor() {
        let s = computed(
            r#"<div><h1>x</h1></div>"#,
            ".hook h1 { font-size: 60px }",
            &|e| e.tag == "h1",
        );
        assert_eq!(s.get("font-size"), None);
    }

    #[test]
    fn grouped_selectors_all_apply() {
        for id in ["s2", "s3", "s4"] {
            let s = computed(
                &format!(r#"<div id="{id}"></div>"#),
                "#s2, #s3, #s4 { opacity: 0 }",
                &|e| e.tag == "div",
            );
            assert_eq!(s.get("opacity"), Some("0"), "{id} should be hidden");
        }
    }

    #[test]
    fn inheritance_passes_text_properties_only() {
        let s = computed(
            r#"<div id="root"><span>x</span></div>"#,
            "#root { color: #1b1b1f; font-family: \"Space Grotesk\"; padding-top: 20px }",
            &|e| e.tag == "span",
        );
        assert_eq!(s.get("color"), Some("#1b1b1f"), "colour inherits");
        assert_eq!(s.get("font-family"), Some("\"Space Grotesk\""));
        assert_eq!(s.get("padding-top"), None, "padding must NOT inherit");
    }

    #[test]
    fn shorthands_expand() {
        let s = computed(
            r#"<div class="c"></div>"#,
            ".c { padding: 40px 44px; margin: 0; inset: 0; gap: 22px; border: 1px solid #e2e2e2 }",
            &|e| e.tag == "div",
        );
        assert_eq!(s.get("padding-top"), Some("40px"));
        assert_eq!(s.get("padding-right"), Some("44px"));
        assert_eq!(s.get("padding-bottom"), Some("40px"));
        assert_eq!(s.get("padding-left"), Some("44px"));
        assert_eq!(s.get("margin-left"), Some("0"));
        assert_eq!(s.get("top"), Some("0"));
        assert_eq!(s.get("row-gap"), Some("22px"));
        assert_eq!(s.get("column-gap"), Some("22px"));
        assert_eq!(s.get("border-top-width"), Some("1px"));
        assert_eq!(s.get("border-left-color"), Some("#e2e2e2"));
    }

    #[test]
    fn border_top_only_sets_one_side() {
        let s = computed(
            r#"<div class="card"></div>"#,
            ".card { border-top: 3px solid #e2e2e2 }",
            &|e| e.tag == "div",
        );
        assert_eq!(s.get("border-top-width"), Some("3px"));
        assert_eq!(s.get("border-bottom-width"), None);
    }

    #[test]
    fn inline_style_outranks_the_sheet() {
        let s = computed(
            r#"<div class="c" style="color: #f00"></div>"#,
            "#nope { color: #0f0 } .c { color: #00f }",
            &|e| e.tag == "div",
        );
        assert_eq!(s.get("color"), Some("#f00"));
    }

    #[test]
    fn lengths_and_colours_resolve() {
        assert_eq!(length("60px", 16.0, 0.0), Some(60.0));
        assert_eq!(length("0.06em", 26.0, 0.0), Some(26.0 * 0.06));
        assert_eq!(length("50%", 16.0, 200.0), Some(100.0));
        assert_eq!(length("0", 16.0, 0.0), Some(0.0));
        assert_eq!(length("12", 16.0, 0.0), None, "bare numbers are not lengths");

        use crate::raster::Rgba;
        assert_eq!(parse_color("#0044ff"), Some(Rgba::rgb(0, 68, 255)));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(Rgba::new(1, 2, 3, 255)));
        assert_eq!(
            parse_color("rgba(0, 0, 0, 0.5)"),
            Some(Rgba::new(0, 0, 0, 128))
        );
        assert_eq!(parse_color("#f8f8f8b3"), Some(Rgba::new(248, 248, 248, 179)));
    }

    #[test]
    fn values_with_separators_inside_parens_survive() {
        let s = computed(
            r#"<div class="w"></div>"#,
            ".w { clip-path: inset(0 100% 0 0); transform-origin: 50% 100% }",
            &|e| e.tag == "div",
        );
        assert_eq!(s.get("clip-path"), Some("inset(0 100% 0 0)"));
        assert_eq!(s.get("transform-origin"), Some("50% 100%"));
    }

    #[test]
    fn comments_and_at_rules_do_not_leak() {
        let sheet = parse(
            "/* c */ .a { color: red } @media screen { .b { color: blue } } .c { color: green }",
        );
        let root = doc(r#"<div class="b"></div>"#);
        let path = path_to(&root, &|e| e.tag == "div");
        let s = sheet.compute(&path, None);
        assert_eq!(s.get("color"), None, "@media contents must not apply");
    }

    #[test]
    fn reads_the_real_stylesheet() {
        let path = "/home/ctown/projects/glossa/brag-output/composition/index.html";
        let Ok(src) = std::fs::read_to_string(path) else {
            eprintln!("skip: composition not present");
            return;
        };
        let root = html::parse(&src).unwrap();
        let sheet = parse(&root.collect_styles());

        // Three @font-face rules, all WOFF2.
        assert_eq!(sheet.font_faces.len(), 3, "expected 3 font faces");
        let grotesk = sheet
            .font_faces
            .iter()
            .find(|f| f.family == "Space Grotesk")
            .expect("Space Grotesk face");
        assert!(grotesk.src.ends_with(".woff2"), "src: {}", grotesk.src);
        assert_eq!(grotesk.weight, "300 700", "variable weight range");

        // The card rule computes as authored.
        let card = path_to(&root, &|e| e.classes().contains(&"term-card"));
        assert!(!card.is_empty(), "term-card not found");
        let mut parent: Option<Styles> = None;
        for n in 1..=card.len() {
            parent = Some(sheet.compute(&card[..n], parent.as_ref()));
        }
        let s = parent.unwrap();
        assert_eq!(s.get("background-color"), Some("#f0f0f0"));
        assert_eq!(s.get("border-radius"), Some("8px"));
        assert_eq!(s.get("padding-top"), Some("40px"));
        assert_eq!(s.get("padding-left"), Some("44px"));
        assert_eq!(s.get("display"), Some("flex"));
        assert_eq!(s.get("flex-direction"), Some("column"));
        // Inherited from #root three levels up.
        assert_eq!(s.get("color"), Some("#1b1b1f"));

        // And the id-over-class case that decides scene 4's background.
        let s4 = path_to(&root, &|e| e.id() == Some("s4"));
        let mut parent: Option<Styles> = None;
        for n in 1..=s4.len() {
            parent = Some(sheet.compute(&s4[..n], parent.as_ref()));
        }
        assert_eq!(
            parent.unwrap().get("background-color"),
            Some("#1b1b1f"),
            "#s4 must beat .scene"
        );
    }
}

/// Find every element matching a CSS selector, as document-order indices.
///
/// Indices count *all* elements in `root.descendants()` order, including
/// `<script>` and `<style>`. Layout skips those when building boxes but still
/// counts them, so an index means the same element on both sides — which is
/// what lets a timeline that selected an element by CSS address the box that
/// was laid out for it.
pub fn query_indices(root: &Element, selector: &str) -> Vec<usize> {
    let Some(sel) = parse_selector(selector.trim()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut path: Vec<&Element> = Vec::new();
    let mut idx = 0usize;
    walk_query(root, &sel, &mut path, &mut idx, &mut out);
    out
}

fn walk_query<'a>(
    el: &'a Element,
    sel: &Selector,
    path: &mut Vec<&'a Element>,
    idx: &mut usize,
    out: &mut Vec<usize>,
) {
    path.push(el);
    let here = *idx;
    *idx += 1;
    if sel.matches(path) {
        out.push(here);
    }
    for c in &el.children {
        if let crate::html::Node::Element(ce) = c {
            walk_query(ce, sel, path, idx, out);
        }
    }
    path.pop();
}

/// Split a comma-separated selector list and match any of them.
pub fn query_indices_list(root: &Element, selectors: &str) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for one in split_top_level(selectors, ',') {
        for i in query_indices(root, one.trim()) {
            if !out.contains(&i) {
                out.push(i);
            }
        }
    }
    out.sort_unstable();
    out
}
