//! A tolerant HTML parser producing the element tree a composition describes.
//!
//! This is not a general-purpose HTML5 parser — no error recovery algorithm, no
//! scripting, no `<table>` foster-parenting. It handles the subset a
//! composition is written in, which is well-formed markup authored by a tool,
//! and it is strict where being lenient would hide a real problem.
//!
//! Two details of real compositions drove the design. Close tags may contain
//! whitespace before `>` — glossa's terminal lines are written `</span\n>` to
//! suppress the whitespace an inter-element newline would otherwise introduce,
//! which is meaningful, not incidental. And character entities carry visible
//! content (`&rsquo;`, `&mdash;`, `&#10003;`), so decoding them is part of
//! parsing, not a later nicety.

use std::collections::HashMap;

/// Elements that never have children or a close tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is raw text, not markup.
const RAW_TEXT: &[&str] = &["script", "style"];

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Element(Element),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    pub tag: String,
    pub attrs: HashMap<String, String>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(|s| s.as_str())
    }

    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    /// The `class` attribute split on whitespace.
    pub fn classes(&self) -> Vec<&str> {
        self.attr("class")
            .map(|c| c.split_whitespace().collect())
            .unwrap_or_default()
    }

    /// All text in this subtree, concatenated.
    pub fn text(&self) -> String {
        let mut s = String::new();
        self.collect_text(&mut s);
        s
    }

    fn collect_text(&self, out: &mut String) {
        for c in &self.children {
            match c {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => e.collect_text(out),
            }
        }
    }

    /// Depth-first iterator over this element and all descendants.
    pub fn descendants(&self) -> Vec<&Element> {
        let mut out = vec![self];
        for c in &self.children {
            if let Node::Element(e) = c {
                out.extend(e.descendants());
            }
        }
        out
    }

    /// First descendant (or self) carrying the given attribute.
    pub fn find_with_attr(&self, name: &str) -> Option<&Element> {
        self.descendants()
            .into_iter()
            .find(|e| e.attrs.contains_key(name))
    }

    /// Concatenated contents of every `<style>` element in the subtree.
    pub fn collect_styles(&self) -> String {
        self.descendants()
            .iter()
            .filter(|e| e.tag == "style")
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Concatenated contents of every inline `<script>` in the subtree.
    /// Scripts with a `src` contribute nothing — their body is not local.
    pub fn collect_inline_scripts(&self) -> String {
        self.descendants()
            .iter()
            .filter(|e| e.tag == "script" && e.attr("src").is_none())
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Parse a document, returning its root element.
///
/// The root is `<html>` when present; otherwise a synthetic root wrapping
/// whatever top-level nodes were found, so a fragment parses too.
pub fn parse(src: &str) -> Result<Element, String> {
    let mut p = Parser {
        b: src.as_bytes(),
        i: 0,
    };
    let nodes = p.parse_nodes(None)?;

    let elements: Vec<Element> = nodes
        .iter()
        .filter_map(|n| match n {
            Node::Element(e) => Some(e.clone()),
            _ => None,
        })
        .collect();

    if let Some(html) = elements.iter().find(|e| e.tag == "html") {
        return Ok(html.clone());
    }
    Ok(Element {
        tag: "#document".to_string(),
        attrs: HashMap::new(),
        children: nodes,
    })
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn eof(&self) -> bool {
        self.i >= self.b.len()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.b[self.i..].starts_with(s.as_bytes())
    }

    fn parse_nodes(&mut self, parent: Option<&str>) -> Result<Vec<Node>, String> {
        let mut out = Vec::new();
        loop {
            if self.eof() {
                return Ok(out);
            }
            if self.starts_with("</") {
                // A close tag belongs to our parent; leave it for them.
                return Ok(out);
            }
            if self.starts_with("<!--") {
                self.skip_past("-->");
                continue;
            }
            if self.starts_with("<!") {
                // doctype and friends carry nothing we need.
                self.skip_past(">");
                continue;
            }
            if self.b[self.i] == b'<' {
                match self.parse_element()? {
                    Some(e) => out.push(Node::Element(e)),
                    None => continue,
                }
                continue;
            }
            // Text up to the next tag.
            let start = self.i;
            while !self.eof() && self.b[self.i] != b'<' {
                self.i += 1;
            }
            let raw = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
            let text = decode_entities(raw);
            if !text.is_empty() {
                out.push(Node::Text(text));
            }
            let _ = parent;
        }
    }

    /// Advance past the next occurrence of `marker`, or to the end.
    fn skip_past(&mut self, marker: &str) {
        while !self.eof() && !self.starts_with(marker) {
            self.i += 1;
        }
        self.i = (self.i + marker.len()).min(self.b.len());
    }

    fn parse_element(&mut self) -> Result<Option<Element>, String> {
        self.i += 1; // '<'
        let name_start = self.i;
        while !self.eof() && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'-') {
            self.i += 1;
        }
        let tag = std::str::from_utf8(&self.b[name_start..self.i])
            .map_err(|e| e.to_string())?
            .to_ascii_lowercase();
        if tag.is_empty() {
            // A stray '<' that is not a tag — treat it as text.
            return Err("HTML: '<' not followed by a tag name".into());
        }

        let mut attrs = HashMap::new();
        let mut self_closing = false;
        loop {
            self.skip_ws();
            if self.eof() {
                return Err(format!("HTML: unterminated <{tag}>"));
            }
            if self.b[self.i] == b'>' {
                self.i += 1;
                break;
            }
            if self.starts_with("/>") {
                self.i += 2;
                self_closing = true;
                break;
            }
            let (k, v) = self.parse_attr()?;
            attrs.insert(k, v);
        }

        if self_closing || VOID.contains(&tag.as_str()) {
            return Ok(Some(Element {
                tag,
                attrs,
                children: Vec::new(),
            }));
        }

        let children = if RAW_TEXT.contains(&tag.as_str()) {
            // Raw-text elements end at their literal close tag; markup and
            // entities inside are content, not structure. A CSS `content: "<"`
            // or a JS `a < b` would otherwise derail the parse.
            let close = format!("</{tag}");
            let start = self.i;
            while !self.eof() && !self.b[self.i..].starts_with(close.as_bytes()) {
                self.i += 1;
            }
            let body = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
            vec![Node::Text(body.to_string())]
        } else {
            self.parse_nodes(Some(&tag))?
        };

        // Consume the matching close tag if present. Whitespace before '>' is
        // legal and load-bearing in real compositions (`</span\n>` suppresses
        // the space an inter-element newline would add), so it is skipped
        // rather than treated as a parse failure.
        if self.starts_with("</") {
            self.i += 2;
            let cname_start = self.i;
            while !self.eof() && self.b[self.i] != b'>' && !self.b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            let cname = std::str::from_utf8(&self.b[cname_start..self.i])
                .map_err(|e| e.to_string())?
                .to_ascii_lowercase();
            self.skip_ws();
            if !self.eof() && self.b[self.i] == b'>' {
                self.i += 1;
            }
            if cname != tag {
                return Err(format!("HTML: </{cname}> closes <{tag}>"));
            }
        }

        Ok(Some(Element {
            tag,
            attrs,
            children,
        }))
    }

    fn parse_attr(&mut self) -> Result<(String, String), String> {
        let start = self.i;
        while !self.eof()
            && !self.b[self.i].is_ascii_whitespace()
            && self.b[self.i] != b'='
            && self.b[self.i] != b'>'
            && !self.starts_with("/>")
        {
            self.i += 1;
        }
        let name = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|e| e.to_string())?
            .to_ascii_lowercase();
        if name.is_empty() {
            return Err("HTML: empty attribute name".into());
        }
        self.skip_ws();
        if self.eof() || self.b[self.i] != b'=' {
            // A valueless attribute is present-and-empty, e.g. `hidden`.
            return Ok((name, String::new()));
        }
        self.i += 1;
        self.skip_ws();
        if self.eof() {
            return Err("HTML: attribute value missing".into());
        }
        let quote = self.b[self.i];
        let value = if quote == b'"' || quote == b'\'' {
            self.i += 1;
            let vs = self.i;
            while !self.eof() && self.b[self.i] != quote {
                self.i += 1;
            }
            let v = std::str::from_utf8(&self.b[vs..self.i]).map_err(|e| e.to_string())?;
            self.i += 1; // closing quote
            v
        } else {
            let vs = self.i;
            while !self.eof() && !self.b[self.i].is_ascii_whitespace() && self.b[self.i] != b'>' {
                self.i += 1;
            }
            std::str::from_utf8(&self.b[vs..self.i]).map_err(|e| e.to_string())?
        };
        Ok((name, decode_entities(value)))
    }

    fn skip_ws(&mut self) {
        while !self.eof() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
}

/// Decode the character entities compositions actually contain.
///
/// An unknown entity is left as written rather than dropped: showing
/// `&frobnicate;` on screen is a visible, diagnosable bug, whereas silently
/// deleting it is not.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let named = [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("nbsp", '\u{00A0}'),
        ("rsquo", '\u{2019}'),
        ("lsquo", '\u{2018}'),
        ("rdquo", '\u{201D}'),
        ("ldquo", '\u{201C}'),
        ("mdash", '\u{2014}'),
        ("ndash", '\u{2013}'),
        ("hellip", '\u{2026}'),
        ("micro", '\u{00B5}'),
        ("times", '\u{00D7}'),
        ("middot", '\u{00B7}'),
        ("bull", '\u{2022}'),
        ("deg", '\u{00B0}'),
        ("copy", '\u{00A9}'),
        ("reg", '\u{00AE}'),
        ("trade", '\u{2122}'),
        ("check", '\u{2713}'),
    ];

    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'&' {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        // Entities are short; a ';' far away is a literal ampersand.
        let end = s[i..].find(';').filter(|&e| e <= 10).map(|e| i + e);
        let Some(end) = end else {
            out.push('&');
            i += 1;
            continue;
        };
        let body = &s[i + 1..end];
        let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(dec) = body.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            named.iter().find(|(n, _)| *n == body).map(|(_, c)| *c)
        };
        match decoded {
            Some(c) => {
                out.push(c);
                i = end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(src: &str) -> Element {
        parse(src).expect("parse")
    }

    #[test]
    fn parses_nested_elements_and_attributes() {
        let root = el(r#"<div id="a" class="x y"><span data-k="1">hi</span></div>"#);
        let div = &root.descendants()[1];
        assert_eq!(div.tag, "div");
        assert_eq!(div.id(), Some("a"));
        assert_eq!(div.classes(), vec!["x", "y"]);
        assert_eq!(div.text(), "hi");
        let span = &div.descendants()[1];
        assert_eq!(span.attr("data-k"), Some("1"));
    }

    #[test]
    fn handles_whitespace_inside_a_close_tag() {
        // glossa writes `</span\n>` to suppress the space a newline would add.
        // Mis-parsing this shifts every following glyph.
        let root = el("<p><span>a</span\n><span>b</span></p>");
        let p = root.descendants()[1];
        assert_eq!(p.text(), "ab", "no whitespace should be introduced");
        assert_eq!(p.children.len(), 2);
    }

    #[test]
    fn void_elements_need_no_close_tag() {
        let root = el(r#"<head><meta charset="UTF-8"><title>t</title></head>"#);
        let head = root.descendants()[1];
        assert_eq!(head.children.len(), 2);
        assert_eq!(head.descendants()[1].tag, "meta");
        assert_eq!(head.descendants()[2].tag, "title");
    }

    #[test]
    fn style_and_script_bodies_are_raw_text() {
        let root = el("<div><style>a { content: \"<\" } b > c { x: 1 }</style></div>");
        let css = root.collect_styles();
        assert!(css.contains("b > c"), "raw text must survive intact: {css}");
    }

    #[test]
    fn scripts_with_src_contribute_no_body() {
        let root = el(r#"<div><script src="x.js"></script><script>var a = 1 < 2;</script></div>"#);
        let js = root.collect_inline_scripts();
        assert!(js.contains("1 < 2"));
        assert_eq!(js.matches("var a").count(), 1);
    }

    #[test]
    fn comments_and_doctype_are_skipped() {
        let root = el("<!doctype html><!-- note --><div>x</div>");
        assert_eq!(root.text(), "x");
    }

    #[test]
    fn decodes_the_entities_compositions_use() {
        assert_eq!(decode_entities("didn&rsquo;t"), "didn\u{2019}t");
        assert_eq!(decode_entities("a &mdash; b"), "a \u{2014} b");
        assert_eq!(decode_entities("&#10007;"), "\u{2717}");
        assert_eq!(decode_entities("&#10003;"), "\u{2713}");
        assert_eq!(decode_entities("1.8&micro;s"), "1.8\u{00B5}s");
        assert_eq!(decode_entities("7.9&nbsp;MB"), "7.9\u{00A0}MB");
        assert_eq!(decode_entities("a &amp; b"), "a & b");
    }

    #[test]
    fn unknown_entities_are_left_visible() {
        // Silently dropping these would hide a real authoring bug.
        assert_eq!(decode_entities("&frobnicate;"), "&frobnicate;");
        assert_eq!(decode_entities("a & b"), "a & b");
    }

    #[test]
    fn mismatched_close_tag_is_an_error() {
        assert!(parse("<div><span>x</div></span>").is_err());
    }

    #[test]
    fn parses_the_real_composition() {
        let path = "/home/ctown/projects/glossa/brag-output/composition/index.html";
        let Ok(src) = std::fs::read_to_string(path) else {
            eprintln!("skip: composition not present");
            return;
        };
        let root = parse(&src).expect("the real composition must parse");
        assert_eq!(root.tag, "html");

        let comp = root
            .find_with_attr("data-composition-id")
            .expect("composition root");
        assert_eq!(comp.attr("data-composition-id"), Some("main"));
        assert_eq!(comp.attr("data-width"), Some("1920"));
        assert_eq!(comp.attr("data-height"), Some("1080"));
        assert_eq!(comp.attr("data-duration"), Some("20"));

        // Four scenes, each a section.scene.
        let scenes: Vec<_> = comp
            .descendants()
            .into_iter()
            .filter(|e| e.tag == "section" && e.classes().contains(&"scene"))
            .collect();
        assert_eq!(scenes.len(), 4, "expected four scenes");

        // The CSS and the GSAP timeline both survived parsing.
        let css = root.collect_styles();
        assert!(css.contains("@font-face"), "stylesheet missing");
        assert!(css.contains(".term-card"));
        let js = root.collect_inline_scripts();
        assert!(js.contains("gsap.timeline"), "timeline script missing");

        // Entity decoding reached real content.
        let text = comp.text();
        assert!(
            text.contains('\u{2019}'),
            "didn’t should carry a curly apostrophe"
        );
        assert!(text.contains('\u{2014}'), "em dash expected");
    }
}
