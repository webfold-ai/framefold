//! Box layout — turning computed styles into placed rectangles.
//!
//! Enough of CSS layout to place a composition: block flow, flex (row and
//! column, with `gap`, `align-items`, `justify-content` and `flex-grow`),
//! absolute positioning against the nearest positioned ancestor, and inline
//! runs of text measured with the composition's own fonts.
//!
//! # Two passes, because CSS needs both
//!
//! Sizes flow up and positions flow down. `.scene` centres a `.hook` whose
//! width is its content — the longest line of nowrap text — so the parent
//! cannot place the child until the child has measured itself, and the child
//! cannot resolve `width: 100%` until the parent has sized itself. So
//! [`measure`] computes intrinsic sizes bottom-up against an available width,
//! and [`place`] assigns final rectangles top-down. Collapsing these into one
//! pass is what makes hand-rolled layout engines mysteriously wrong on
//! centred content.
//!
//! Not implemented: floats, tables, grid, multi-line inline wrapping with
//! mixed styles, writing modes, and margin collapsing. Compositions do not use
//! them, and guessing would place boxes confidently in the wrong spot.

use crate::css::{length, Styles, Stylesheet};
use crate::fonts::{weight_value, Fonts};
use crate::html::{Element, Node as HtmlNode};
use crate::raster::Rect;

/// A laid-out box.
#[derive(Clone, Debug)]
pub struct Boxes {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub styles: Styles,
    /// Border box: the rectangle backgrounds and borders paint into.
    pub rect: Rect,
    /// Inside padding and borders, where children and text go.
    pub content: Rect,
    /// Text carried directly by this box, already case-transformed.
    pub text: Option<String>,
    /// Resolved font size in pixels, needed for em units and drawing.
    pub font_size: f32,
    pub children: Vec<Boxes>,
    /// Intrinsic size measured in pass one, kept for diagnostics.
    pub intrinsic: (f32, f32),
    /// Document-order index of the source element, matching
    /// [`crate::css::query_indices`]. This is how a timeline addresses a box it
    /// selected by CSS, including elements that carry no id.
    pub node_index: usize,
    /// The width this box's text was wrapped to when it was measured, or
    /// `None` if it was measured unwrapped.
    ///
    /// Paint reuses this rather than deciding again. Re-deriving it there
    /// looks equivalent but is not: a shrink-to-fit box ends up exactly as wide
    /// as its own unwrapped text, and re-wrapping at that same width lets float
    /// rounding push the final word onto a second line.
    pub text_wrap: Option<f32>,
}

impl Boxes {
    /// Depth-first walk, self first.
    pub fn walk(&self) -> Vec<&Boxes> {
        let mut out = vec![self];
        for c in &self.children {
            out.extend(c.walk());
        }
        out
    }

    pub fn find_id(&self, id: &str) -> Option<&Boxes> {
        self.walk()
            .into_iter()
            .find(|b| b.id.as_deref() == Some(id))
    }

    pub fn has_class(&self, c: &str) -> bool {
        self.classes.iter().any(|x| x == c)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Display {
    Block,
    Flex,
    Inline,
    None,
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Row,
    Column,
}

/// Edge sizes resolved to pixels.
#[derive(Clone, Copy, Default, Debug)]
struct Edges {
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
}

impl Edges {
    fn horizontal(&self) -> f32 {
        self.left + self.right
    }
    fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

fn edges(s: &Styles, prefix: &str, suffix: &str, fs: f32, basis: f32) -> Edges {
    let g = |side: &str| {
        s.get(&format!("{prefix}-{side}{suffix}"))
            .and_then(|v| length(v, fs, basis))
            .unwrap_or(0.0)
    };
    Edges {
        top: g("top"),
        right: g("right"),
        bottom: g("bottom"),
        left: g("left"),
    }
}

/// Elements whose default display is inline. HTML's user-agent stylesheet, not
/// CSS's initial value — `display`'s initial value is `inline`, but a browser's
/// default sheet sets most elements to `block`, and only these stay inline.
///
/// Defaulting everything to block stacks a line's spans vertically instead of
/// running them along a line, which silently doubles line heights.
const INLINE_TAGS: &[&str] = &[
    "span", "a", "em", "strong", "b", "i", "u", "s", "small", "code", "kbd", "samp", "var", "sub",
    "sup", "abbr", "cite", "q", "mark", "time", "label", "br", "img",
];

fn default_display(tag: &str) -> Display {
    if INLINE_TAGS.contains(&tag) {
        Display::Inline
    } else {
        Display::Block
    }
}

fn display_of(tag: &str, s: &Styles) -> Display {
    match s.get("display") {
        Some("none") => Display::None,
        Some("flex") | Some("inline-flex") => Display::Flex,
        Some("inline") | Some("inline-block") => Display::Inline,
        Some("block") | Some("flow-root") | Some("list-item") => Display::Block,
        Some(_) => Display::Block,
        None => default_display(tag),
    }
}

fn direction_of(s: &Styles) -> Dir {
    match s.get("flex-direction").unwrap_or("row") {
        "column" | "column-reverse" => Dir::Column,
        _ => Dir::Row,
    }
}

/// The tree being laid out: styles resolved, text extracted, geometry pending.
pub struct Builder<'a> {
    sheet: &'a Stylesheet,
    fonts: &'a Fonts,
}

#[derive(Clone)]
struct Pending {
    tag: String,
    id: Option<String>,
    classes: Vec<String>,
    styles: Styles,
    text: Option<String>,
    font_size: f32,
    children: Vec<Pending>,
    /// Measured intrinsic (width, height).
    size: (f32, f32),
    /// Width the text was wrapped to during measurement, if any.
    text_wrap: Option<f32>,
    node_index: usize,
}

impl<'a> Builder<'a> {
    pub fn new(sheet: &'a Stylesheet, fonts: &'a Fonts) -> Builder<'a> {
        Builder { sheet, fonts }
    }

    /// Lay out `root` into `viewport`.
    pub fn layout(&self, root: &Element, viewport: Rect) -> Result<Boxes, String> {
        let mut path = Vec::new();
        let mut counter = 0usize;
        let mut pending = self.build(root, &mut path, None, 16.0, &mut counter)?;
        self.measure(&mut pending, viewport.w, viewport.h, true);
        // The root has no parent to size it, so it takes its own measured size
        // at the viewport origin. (A composition root declaring 100%/100%
        // measures to the full frame; a narrower root stays narrow.)
        let slot = Rect::new(viewport.x, viewport.y, pending.size.0, pending.size.1);
        Ok(self.place(&pending, slot, viewport))
    }

    /// Pass zero: resolve styles and text for every element.
    fn build<'e>(
        &self,
        el: &'e Element,
        path: &mut Vec<&'e Element>,
        parent: Option<&Styles>,
        parent_font_size: f32,
        counter: &mut usize,
    ) -> Result<Pending, String> {
        // The cascade matches against an ancestor path, so it is threaded
        // through the recursion. Borrowing the tree directly keeps this safe:
        // every element outlives the layout call that reads it.
        path.push(el);
        // Claim this element's document-order index before descending, so the
        // numbering matches css::query_indices exactly.
        let node_index = *counter;
        *counter += 1;
        let styles = self.sheet.compute(path, parent);

        let font_size = styles
            .get("font-size")
            .and_then(|v| length(v, parent_font_size, parent_font_size))
            .unwrap_or(parent_font_size);

        // Direct text of this element (not from child elements).
        let own_text: String = el
            .children
            .iter()
            .filter_map(|c| match c {
                HtmlNode::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let has_elements = el
            .children
            .iter()
            .any(|c| matches!(c, HtmlNode::Element(_)));

        // Whitespace between inline elements is meaningful; whitespace around
        // block children is not. Normalising here rather than at draw time
        // keeps measurement and painting consistent.
        let text = if own_text.trim().is_empty() && has_elements {
            None
        } else {
            let collapsed = collapse_ws(&own_text, styles.get("white-space"));
            if collapsed.is_empty() {
                None
            } else {
                Some(transform_case(&collapsed, styles.get("text-transform")))
            }
        };

        let mut children = Vec::new();
        for c in &el.children {
            if let HtmlNode::Element(ce) = c {
                if ce.tag == "script" || ce.tag == "style" {
                    // Skipped for layout but still counted, or every index
                    // after a <script> would be off by one.
                    *counter += 1 + ce.descendants().len() - 1;
                    continue;
                }
                let child = self.build(ce, path, Some(&styles), font_size, counter)?;
                if display_of(&ce.tag, &child.styles) != Display::None {
                    children.push(child);
                }
            }
        }

        path.pop();
        Ok(Pending {
            tag: el.tag.clone(),
            id: el.id().map(|s| s.to_string()),
            classes: el.classes().iter().map(|s| s.to_string()).collect(),
            styles,
            text,
            font_size,
            children,
            size: (0.0, 0.0),
            text_wrap: None,
            node_index,
        })
    }

    /// Pass one: intrinsic sizes, bottom-up.
    ///
    /// `definite_w` says whether `avail_w` is a width the box must fit inside.
    /// It is not always: a flex item in a column aligned to `flex-start` is
    /// sized by its own content, so wrapping its text to the container width
    /// would shrink it and then wrap it again against the narrower result.
    /// Text only wraps against a definite width; otherwise it is measured at
    /// max-content.
    fn measure(&self, n: &mut Pending, avail_w: f32, avail_h: f32, definite_w: bool) {
        let fs = n.font_size;
        let pad = edges(&n.styles, "padding", "", fs, avail_w);
        let bord = edges(&n.styles, "border", "-width", fs, avail_w);
        let mar = edges(&n.styles, "margin", "", fs, avail_w);
        let border_box = n.styles.get("box-sizing") == Some("border-box");

        // An explicit size wins; otherwise the box is content-sized.
        let explicit_w = n.styles.get("width").and_then(|v| length(v, fs, avail_w));
        let explicit_h = n.styles.get("height").and_then(|v| length(v, fs, avail_h));

        // Absolute boxes with both insets take their size from the container.
        let (inset_w, inset_h) = inset_size(&n.styles, fs, avail_w, avail_h);

        let inner_avail_w = explicit_w
            .or(inset_w)
            .map(|w| {
                if border_box {
                    w - pad.horizontal() - bord.horizontal()
                } else {
                    w
                }
            })
            .unwrap_or(avail_w - pad.horizontal() - bord.horizontal() - mar.horizontal())
            .max(0.0);

        // Whether each child's width is definite depends on how this box lays
        // children out.
        let self_display = display_of(&n.tag, &n.styles);
        let child_definite = match self_display {
            // Row items are sized along the main axis by flex distribution,
            // which has not happened yet — they are re-measured in place_flex
            // once their share is known.
            Display::Flex if direction_of(&n.styles) == Dir::Row => false,
            // Column items span the cross axis only when stretched.
            Display::Flex => n.styles.get("align-items").unwrap_or("stretch") == "stretch",
            _ => definite_w || explicit_w.is_some() || inset_w.is_some(),
        };
        for c in &mut n.children {
            self.measure(c, inner_avail_w, avail_h, child_definite);
        }

        let display = self_display;
        let (mut content_w, mut content_h) = match display {
            Display::None => (0.0, 0.0),
            Display::Flex => self.flex_intrinsic(n, inner_avail_w),
            _ => {
                // Block and inline: children stack, width is the widest.
                let mut w: f32 = 0.0;
                let mut h = 0.0;
                let inline_children = n
                    .children
                    .iter()
                    .all(|c| display_of(&c.tag, &c.styles) == Display::Inline);
                if !n.children.is_empty() && inline_children {
                    // One inline run: widths add, height is the tallest.
                    w = n.children.iter().map(|c| c.size.0).sum();
                    h = n.children.iter().map(|c| c.size.1).fold(0.0, f32::max);
                } else {
                    for c in &n.children {
                        let cm = edges(&c.styles, "margin", "", c.font_size, inner_avail_w);
                        w = w.max(c.size.0 + cm.horizontal());
                        h += c.size.1 + cm.vertical();
                    }
                }
                (w, h)
            }
        };

        // Text contributes its measured run, wrapped to the available width
        // unless `white-space: nowrap` forbids it.
        if let Some(t) = &n.text {
            let text_avail = if definite_w || explicit_w.is_some() || inset_w.is_some() {
                inner_avail_w
            } else {
                f32::INFINITY
            };
            let t = t.clone();
            let (tw, th, wrap) = self.measure_text(n, &t, text_avail);
            n.text_wrap = wrap;
            content_w = content_w.max(tw);
            content_h = content_h.max(th);
        }

        let w = explicit_w
            .or(inset_w)
            .map(|w| {
                if border_box {
                    w
                } else {
                    w + pad.horizontal() + bord.horizontal()
                }
            })
            .unwrap_or(content_w + pad.horizontal() + bord.horizontal());
        let h = explicit_h
            .or(inset_h)
            .map(|h| {
                if border_box {
                    h
                } else {
                    h + pad.vertical() + bord.vertical()
                }
            })
            .unwrap_or(content_h + pad.vertical() + bord.vertical());

        n.size = (w, h);
    }

    /// Intrinsic content size of a flex container.
    ///
    /// A row distributes its main axis here, not only when placing, and
    /// re-measures each item at the width it will actually get. Without that
    /// the container's cross size comes from items measured against the whole
    /// container: a `flex: 1` card measured at 1640px fits its text on one line
    /// and reports a short height, then paints at 509px where the text wraps
    /// and is taller. The card itself was already corrected at placement time,
    /// but its *ancestors* had been sized from the short measurement — so a
    /// wrapper ended up 128px tall around 235px of content, and anything
    /// anchored to that wrapper (a `transform-origin: 50% 100%`, say) pivoted
    /// about the wrong point.
    fn flex_intrinsic(&self, n: &mut Pending, avail_w: f32) -> (f32, f32) {
        let fs = n.font_size;
        let dir = direction_of(&n.styles);
        let gap = |prop: &str| {
            n.styles
                .get(prop)
                .and_then(|v| length(v, fs, avail_w))
                .unwrap_or(0.0)
        };
        let (main_gap, cross_gap) = match dir {
            Dir::Row => (gap("column-gap"), gap("row-gap")),
            Dir::Column => (gap("row-gap"), gap("column-gap")),
        };
        let _ = cross_gap;
        let n_gaps = n.children.len().saturating_sub(1) as f32;

        // Resolve the main axis first for a row, so items are measured at the
        // size they will be given rather than the size of the container.
        if dir == Dir::Row {
            let basis = |c: &Pending| -> f32 {
                match c.styles.get("flex-basis") {
                    Some("auto") | Some("content") | None => c.size.0,
                    Some(b) => length(b, c.font_size, avail_w).unwrap_or(c.size.0),
                }
            };
            let grow = |c: &Pending| -> f32 {
                c.styles
                    .get("flex-grow")
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(0.0)
            };
            let used: f32 = n.children.iter().map(basis).sum::<f32>() + main_gap * n_gaps;
            let free = (avail_w - used).max(0.0);
            let total_grow: f32 = n.children.iter().map(grow).sum();
            if total_grow > 0.0 && free > 0.0 {
                let targets: Vec<f32> = n
                    .children
                    .iter()
                    .map(|c| basis(c) + free * grow(c) / total_grow)
                    .collect();
                for (c, w) in n.children.iter_mut().zip(targets) {
                    if (w - c.size.0).abs() > 0.5 {
                        self.measure(c, w, avail_h_hint(c), true);
                    }
                }
            }
        }

        let mut main = 0.0f32;
        let mut cross = 0.0f32;
        for c in &n.children {
            let cm = edges(&c.styles, "margin", "", c.font_size, avail_w);
            match dir {
                Dir::Row => {
                    main += c.size.0 + cm.horizontal();
                    cross = cross.max(c.size.1 + cm.vertical());
                }
                Dir::Column => {
                    main += c.size.1 + cm.vertical();
                    cross = cross.max(c.size.0 + cm.horizontal());
                }
            }
        }
        main += main_gap * n_gaps;
        match dir {
            Dir::Row => (main, cross),
            Dir::Column => (cross, main),
        }
    }

    /// Measure a text run on one line.
    fn measure_text(&self, n: &Pending, text: &str, avail_w: f32) -> (f32, f32, Option<f32>) {
        let family = n.styles.get("font-family").unwrap_or("sans-serif");
        let weight = weight_value(n.styles.get("font-weight"));
        let Some(font) = self.fonts.resolve(family, weight) else {
            return (0.0, n.font_size * 1.2, None);
        };
        let ls = n
            .styles
            .get("letter-spacing")
            .and_then(|v| length(v, n.font_size, 0.0))
            .unwrap_or(0.0);
        let lh_first = n
            .styles
            .get("line-height")
            .and_then(|v| {
                // A unitless line-height is a multiplier, not a length.
                v.trim()
                    .parse::<f32>()
                    .ok()
                    .map(|m| m * n.font_size)
                    .or_else(|| length(v, n.font_size, n.font_size))
            })
            .unwrap_or(n.font_size * 1.2);

        // A run that may wrap is measured as the lines it actually becomes:
        // its width is the widest line and its height is line-count * leading.
        // Measuring the unwrapped width instead sizes the box for one long
        // line, and the text then overflows into whatever sits beside it.
        let nowrap = matches!(n.styles.get("white-space"), Some("nowrap") | Some("pre"));
        let max = if nowrap || !avail_w.is_finite() {
            None
        } else {
            Some(avail_w)
        };
        let lines = crate::text::break_lines(font, n.font_size, ls, text, max)
            .unwrap_or_else(|_| vec![(text.to_string(), 0.0)]);
        let w = lines.iter().map(|(_, w)| *w).fold(0.0, f32::max);
        (w, lh_first * lines.len() as f32, max)
    }

    /// Pass two: assign rectangles, top-down.
    fn place(&self, n: &Pending, containing: Rect, viewport: Rect) -> Boxes {
        let fs = n.font_size;
        let pad = edges(&n.styles, "padding", "", fs, containing.w);
        let bord = edges(&n.styles, "border", "-width", fs, containing.w);
        let border_box = n.styles.get("box-sizing") == Some("border-box");

        let positioned = matches!(n.styles.get("position"), Some("absolute") | Some("fixed"));

        // The box's own rectangle. An absolutely positioned box resolves
        // against its containing block's edges; everything else takes the slot
        // its parent already sized for it.
        //
        // The slot is authoritative, not the intrinsic size: a block child
        // fills its parent's width and a `flex: 1` child is grown to its share,
        // neither of which the child could know while measuring itself. Using
        // the intrinsic size here silently collapses both to zero.
        let rect = if positioned {
            let base = if n.styles.get("position") == Some("fixed") {
                viewport
            } else {
                containing
            };
            absolute_rect(&n.styles, fs, base, n.size)
        } else {
            containing
        };

        let content = Rect::new(
            rect.x + pad.left + bord.left,
            rect.y + pad.top + bord.top,
            (rect.w - pad.horizontal() - bord.horizontal()).max(0.0),
            (rect.h - pad.vertical() - bord.vertical()).max(0.0),
        );
        let _ = border_box;

        let children = match display_of(&n.tag, &n.styles) {
            Display::Flex => self.place_flex(n, content, viewport),
            _ => self.place_block(n, content, viewport),
        };

        Boxes {
            tag: n.tag.clone(),
            id: n.id.clone(),
            classes: n.classes.clone(),
            styles: n.styles.clone(),
            rect,
            content,
            text: n.text.clone(),
            font_size: n.font_size,
            children,
            intrinsic: n.size,
            text_wrap: n.text_wrap,
            node_index: n.node_index,
        }
    }

    fn place_block(&self, n: &Pending, content: Rect, viewport: Rect) -> Vec<Boxes> {
        let mut out = Vec::new();
        let mut y = content.y;
        let mut x = content.x;
        let inline_run = !n.children.is_empty()
            && n.children
                .iter()
                .all(|c| display_of(&c.tag, &c.styles) == Display::Inline);

        for c in &n.children {
            if matches!(c.styles.get("position"), Some("absolute") | Some("fixed")) {
                out.push(self.place(c, content, viewport));
                continue;
            }
            let cm = edges(&c.styles, "margin", "", c.font_size, content.w);
            if inline_run {
                // Inline children flow left to right on one line.
                let slot = Rect::new(x + cm.left, content.y + cm.top, c.size.0, c.size.1);
                out.push(self.place(c, slot, viewport));
                x += c.size.0 + cm.horizontal();
            } else {
                // Block children take the full content width unless they sized
                // themselves, and stack downward.
                let w = if c.styles.get("width").is_some() {
                    c.size.0
                } else {
                    (content.w - cm.horizontal()).max(c.size.0.min(content.w))
                };
                let slot = Rect::new(content.x + cm.left, y + cm.top, w, c.size.1);
                out.push(self.place(c, slot, viewport));
                y += c.size.1 + cm.vertical();
            }
        }
        out
    }

    fn place_flex(&self, n: &Pending, content: Rect, viewport: Rect) -> Vec<Boxes> {
        let fs = n.font_size;
        let dir = direction_of(&n.styles);
        let gap = |prop: &str| {
            n.styles
                .get(prop)
                .and_then(|v| length(v, fs, content.w))
                .unwrap_or(0.0)
        };
        let main_gap = match dir {
            Dir::Row => gap("column-gap"),
            Dir::Column => gap("row-gap"),
        };

        let flow: Vec<&Pending> = n
            .children
            .iter()
            .filter(|c| !matches!(c.styles.get("position"), Some("absolute") | Some("fixed")))
            .collect();
        let n_gaps = flow.len().saturating_sub(1) as f32;

        // An item's base size along the main axis. `flex-basis` overrides the
        // measured size — and `flex: 1` sets it to zero, so the whole container
        // becomes free space to be shared by grow factors.
        let main_size = |c: &Pending| {
            let measured = match dir {
                Dir::Row => c.size.0,
                Dir::Column => c.size.1,
            };
            match c.styles.get("flex-basis") {
                Some("auto") | Some("content") | None => measured,
                Some(b) => {
                    length(b, c.font_size, container_main_of(dir, content)).unwrap_or(measured)
                }
            }
        };
        let container_main = match dir {
            Dir::Row => content.w,
            Dir::Column => content.h,
        };

        // Distribute free space to flex-grow children. `flex: 1` on three cards
        // is how a row splits evenly, so this has to be real rather than an
        // equal-division shortcut — a mix of grow factors must weight correctly.
        let used: f32 = flow.iter().map(|c| main_size(c)).sum::<f32>() + main_gap * n_gaps;
        let free = (container_main - used).max(0.0);
        let total_grow: f32 = flow
            .iter()
            .map(|c| {
                c.styles
                    .get("flex-grow")
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(0.0)
            })
            .sum();

        let mains: Vec<f32> = flow
            .iter()
            .map(|c| {
                let grow = c
                    .styles
                    .get("flex-grow")
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(0.0);
                let extra = if total_grow > 0.0 {
                    free * grow / total_grow
                } else {
                    0.0
                };
                main_size(c) + extra
            })
            .collect();

        let total_main: f32 = mains.iter().sum::<f32>() + main_gap * n_gaps;
        let justify = n.styles.get("justify-content").unwrap_or("flex-start");
        let leftover = (container_main - total_main).max(0.0);
        let (mut cursor, between) = match justify {
            "center" => (container_main / 2.0 - total_main / 2.0, 0.0),
            "flex-end" | "end" => (leftover, 0.0),
            "space-between" if flow.len() > 1 => (0.0, leftover / n_gaps),
            "space-around" if !flow.is_empty() => {
                let each = leftover / flow.len() as f32;
                (each / 2.0, each)
            }
            "space-evenly" if !flow.is_empty() => {
                let each = leftover / (flow.len() + 1) as f32;
                (each, each)
            }
            _ => (0.0, 0.0),
        };

        let align = n.styles.get("align-items").unwrap_or("stretch");
        let mut out = Vec::new();
        let mut flow_i = 0usize;

        for c in &n.children {
            if matches!(c.styles.get("position"), Some("absolute") | Some("fixed")) {
                out.push(self.place(c, content, viewport));
                continue;
            }
            let cm = edges(&c.styles, "margin", "", c.font_size, content.w);
            let m = mains[flow_i];

            // `stretch` only applies when the item has no explicit cross size.
            let explicit_cross = match dir {
                Dir::Row => c.styles.get("height").is_some(),
                Dir::Column => c.styles.get("width").is_some(),
            };

            // Re-measure with the size flex just decided. A row item's width
            // is only known after distribution, so its first measurement used
            // the container's width — text that wraps would otherwise be
            // measured for one line and painted as two, and the box would not
            // grow to hold it.
            let mut resized;
            let child: &Pending = if matches!(dir, Dir::Row) && (m - c.size.0).abs() > 0.5 {
                resized = (*c).clone();
                self.measure(&mut resized, m, content.h, true);
                &resized
            } else {
                c
            };

            let (cross_size, container_cross) = match dir {
                Dir::Row => (child.size.1, content.h),
                Dir::Column => (child.size.0, content.w),
            };
            let stretched = align == "stretch" && !explicit_cross;
            let cross = if stretched {
                container_cross
            } else {
                cross_size
            };
            let cross_off = match align {
                "center" => (container_cross - cross) / 2.0,
                "flex-end" | "end" => container_cross - cross,
                _ => 0.0,
            };

            let slot = match dir {
                Dir::Row => Rect::new(
                    content.x + cursor + cm.left,
                    content.y + cross_off + cm.top,
                    m,
                    cross,
                ),
                Dir::Column => Rect::new(
                    content.x + cross_off + cm.left,
                    content.y + cursor + cm.top,
                    cross,
                    m,
                ),
            };
            out.push(self.place(child, slot, viewport));

            cursor += m + main_gap + between;
            flow_i += 1;
        }
        out
    }
}

/// Size implied by opposing insets, when both are given.
fn inset_size(s: &Styles, fs: f32, avail_w: f32, avail_h: f32) -> (Option<f32>, Option<f32>) {
    if !matches!(s.get("position"), Some("absolute") | Some("fixed")) {
        return (None, None);
    }
    let g = |p: &str, basis: f32| s.get(p).and_then(|v| length(v, fs, basis));
    let w = match (g("left", avail_w), g("right", avail_w)) {
        (Some(l), Some(r)) => Some((avail_w - l - r).max(0.0)),
        _ => None,
    };
    let h = match (g("top", avail_h), g("bottom", avail_h)) {
        (Some(t), Some(b)) => Some((avail_h - t - b).max(0.0)),
        _ => None,
    };
    (w, h)
}

fn absolute_rect(s: &Styles, fs: f32, base: Rect, size: (f32, f32)) -> Rect {
    let g = |p: &str, basis: f32| s.get(p).and_then(|v| length(v, fs, basis));
    let left = g("left", base.w);
    let right = g("right", base.w);
    let top = g("top", base.h);
    let bottom = g("bottom", base.h);

    let x = match (left, right) {
        (Some(l), _) => base.x + l,
        (None, Some(r)) => base.right() - r - size.0,
        (None, None) => base.x,
    };
    let y = match (top, bottom) {
        (Some(t), _) => base.y + t,
        (None, Some(b)) => base.bottom() - b - size.1,
        (None, None) => base.y,
    };
    Rect::new(x, y, size.0, size.1)
}

/// Collapse runs of whitespace, unless `white-space` preserves them.
fn collapse_ws(s: &str, white_space: Option<&str>) -> String {
    match white_space {
        Some("pre") | Some("pre-wrap") | Some("break-spaces") => s.to_string(),
        _ => {
            let mut out = String::with_capacity(s.len());
            let mut last_ws = false;
            for c in s.chars() {
                // A non-breaking space is content, not collapsible whitespace.
                if c.is_whitespace() && c != '\u{00A0}' {
                    if !last_ws {
                        out.push(' ');
                    }
                    last_ws = true;
                } else {
                    out.push(c);
                    last_ws = false;
                }
            }
            out.trim().to_string()
        }
    }
}

fn transform_case(s: &str, transform: Option<&str>) -> String {
    match transform {
        Some("uppercase") => s.to_uppercase(),
        Some("lowercase") => s.to_lowercase(),
        Some("capitalize") => s
            .split(' ')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{css, html};

    const COMP: &str = "/home/ctown/projects/glossa/brag-output/composition";

    struct Fixture {
        root: html::Element,
        sheet: Stylesheet,
        fonts: Fonts,
    }

    fn real() -> Option<Fixture> {
        let src = std::fs::read_to_string(format!("{COMP}/index.html")).ok()?;
        let root = html::parse(&src).ok()?;
        let sheet = css::parse(&root.collect_styles());
        let fonts = Fonts::load(&sheet.font_faces, std::path::Path::new(COMP)).ok()?;
        Some(Fixture { root, sheet, fonts })
    }

    fn lay(f: &Fixture) -> Boxes {
        let comp = f.root.find_with_attr("data-composition-id").unwrap();
        Builder::new(&f.sheet, &f.fonts)
            .layout(comp, Rect::new(0.0, 0.0, 1920.0, 1080.0))
            .expect("layout")
    }

    fn synth(html_src: &str, css_src: &str, w: f32, h: f32) -> Boxes {
        let root = html::parse(html_src).unwrap();
        let sheet = css::parse(css_src);
        let fonts = Fonts::default();
        let target = match &root.children[0] {
            html::Node::Element(e) => e.clone(),
            _ => panic!("expected an element"),
        };
        Builder::new(&sheet, &fonts)
            .layout(&target, Rect::new(0.0, 0.0, w, h))
            .unwrap()
    }

    #[test]
    fn block_children_stack_and_fill_width() {
        let b = synth(
            "<div class='p'><div class='a'></div><div class='b'></div></div>",
            ".p { width: 200px } .a { height: 30px } .b { height: 50px }",
            400.0,
            400.0,
        );
        assert_eq!(b.children[0].rect.y, 0.0);
        assert_eq!(b.children[0].rect.h, 30.0);
        assert_eq!(
            b.children[1].rect.y, 30.0,
            "second child stacks below first"
        );
        assert_eq!(b.children[1].rect.h, 50.0);
        assert_eq!(b.children[0].rect.w, 200.0, "block fills its parent");
    }

    #[test]
    fn flex_row_distributes_grow_evenly_with_gaps() {
        // The .cards-row shape: three flex:1 children with gaps between.
        let b = synth(
            "<div class='r'><div class='c'></div><div class='c'></div><div class='c'></div></div>",
            ".r { display: flex; width: 1640px; gap: 56px } .c { flex: 1; height: 10px }",
            1920.0,
            1080.0,
        );
        let expected = (1640.0 - 112.0) / 3.0;
        for (i, c) in b.children.iter().enumerate() {
            assert!(
                (c.rect.w - expected).abs() < 0.01,
                "card {i} width {} != {expected}",
                c.rect.w
            );
        }
        assert!((b.children[1].rect.x - b.children[0].rect.right() - 56.0).abs() < 0.01);
        assert!((b.children[2].rect.x - b.children[1].rect.right() - 56.0).abs() < 0.01);
    }

    #[test]
    fn flex_grow_weights_are_proportional() {
        let b = synth(
            "<div class='r'><div class='a'></div><div class='b'></div></div>",
            ".r { display: flex; width: 300px } .a { flex: 1 } .b { flex: 2 }",
            300.0,
            100.0,
        );
        assert!((b.children[0].rect.w - 100.0).abs() < 0.01);
        assert!((b.children[1].rect.w - 200.0).abs() < 0.01);
    }

    #[test]
    fn justify_and_align_centre_content() {
        let b = synth(
            "<div class='s'><div class='i'></div></div>",
            ".s { display: flex; width: 100px; height: 100px; align-items: center; justify-content: center } .i { width: 20px; height: 20px }",
            100.0,
            100.0,
        );
        let i = &b.children[0];
        assert!((i.rect.x - 40.0).abs() < 0.01, "x {}", i.rect.x);
        assert!((i.rect.y - 40.0).abs() < 0.01, "y {}", i.rect.y);
    }

    #[test]
    fn space_between_spreads_to_the_edges() {
        let b = synth(
            "<div class='s'><div class='i'></div><div class='i'></div></div>",
            ".s { display: flex; width: 100px; justify-content: space-between } .i { width: 20px; height: 5px }",
            100.0,
            100.0,
        );
        assert!((b.children[0].rect.x - 0.0).abs() < 0.01);
        assert!(
            (b.children[1].rect.right() - 100.0).abs() < 0.01,
            "last item should touch the right edge, got {}",
            b.children[1].rect.right()
        );
    }

    #[test]
    fn border_box_keeps_padding_inside_the_declared_width() {
        let b = synth(
            "<div class='c'></div>",
            "* { box-sizing: border-box } .c { width: 200px; height: 100px; padding: 40px 44px; border: 1px solid #000 }",
            400.0,
            400.0,
        );
        assert_eq!(b.rect.w, 200.0, "border box does not grow with padding");
        assert!(
            (b.content.w - (200.0 - 88.0 - 2.0)).abs() < 0.01,
            "content {}",
            b.content.w
        );
        assert!(
            (b.content.x - 45.0).abs() < 0.01,
            "content x {}",
            b.content.x
        );
    }

    #[test]
    fn absolute_inset_fills_the_containing_block() {
        let b = synth(
            "<div class='root'><div class='s'></div></div>",
            ".root { position: relative; width: 1920px; height: 1080px } .s { position: absolute; inset: 0 }",
            1920.0,
            1080.0,
        );
        let s = &b.children[0];
        assert_eq!((s.rect.x, s.rect.y), (0.0, 0.0));
        assert_eq!((s.rect.w, s.rect.h), (1920.0, 1080.0));
    }

    #[test]
    fn display_none_is_dropped_entirely() {
        let b = synth(
            "<div class='p'><div class='a'></div><div class='hidden'></div></div>",
            ".p { width: 100px } .a { height: 10px } .hidden { display: none; height: 999px }",
            100.0,
            100.0,
        );
        assert_eq!(b.children.len(), 1, "display:none must not be laid out");
    }

    #[test]
    fn whitespace_collapses_but_nbsp_survives() {
        assert_eq!(collapse_ws("  a   b  ", None), "a b");
        assert_eq!(collapse_ws("a\n  b", None), "a b");
        assert_eq!(collapse_ws("7.9\u{00A0}MB", None), "7.9\u{00A0}MB");
        assert_eq!(collapse_ws("  a  ", Some("pre")), "  a  ");
    }

    #[test]
    fn text_transform_applies() {
        assert_eq!(transform_case("one gate", Some("uppercase")), "ONE GATE");
        assert_eq!(transform_case("ONE", Some("lowercase")), "one");
        assert_eq!(transform_case("one gate", None), "one gate");
    }

    // ── against the real composition ────────────────────────────────────────

    #[test]
    fn real_composition_lays_out_at_full_frame() {
        let Some(f) = real() else {
            eprintln!("skip: composition not present");
            return;
        };
        let b = lay(&f);
        assert_eq!(b.rect.w, 1920.0);
        assert_eq!(b.rect.h, 1080.0);
    }

    #[test]
    fn scenes_fill_the_frame_and_respect_side_padding() {
        let Some(f) = real() else { return };
        let b = lay(&f);
        for id in ["s1", "s2", "s3", "s4"] {
            let s = b.find_id(id).unwrap_or_else(|| panic!("{id} missing"));
            assert_eq!((s.rect.w, s.rect.h), (1920.0, 1080.0), "{id} should fill");
            // padding: 0 140px
            assert!(
                (s.content.x - 140.0).abs() < 0.01,
                "{id} content x {}",
                s.content.x
            );
            assert!(
                (s.content.w - 1640.0).abs() < 0.01,
                "{id} content w {}",
                s.content.w
            );
        }
    }

    #[test]
    fn the_terminal_card_spans_the_content_width() {
        let Some(f) = real() else { return };
        let b = lay(&f);
        let card = b
            .walk()
            .into_iter()
            .find(|x| x.has_class("term-card"))
            .expect("term-card");
        // width: 100% of the scene's 1640px content box.
        assert!(
            (card.rect.w - 1640.0).abs() < 0.5,
            "card width {}",
            card.rect.w
        );
        // padding 40/44 plus a 1px border, inside the declared width.
        assert!(
            (card.content.w - (1640.0 - 88.0 - 2.0)).abs() < 0.5,
            "card content {}",
            card.content.w
        );
    }

    #[test]
    fn the_three_cards_split_the_row_evenly() {
        let Some(f) = real() else { return };
        let b = lay(&f);
        let cards: Vec<&Boxes> = b
            .walk()
            .into_iter()
            .filter(|x| x.has_class("card"))
            .collect();
        assert_eq!(cards.len(), 3, "expected three cards");
        let expected = (1640.0 - 2.0 * 56.0) / 3.0;
        for c in &cards {
            assert!(
                (c.rect.w - expected).abs() < 1.0,
                "card width {} != {expected}",
                c.rect.w
            );
        }
        assert!(cards[0].rect.x < cards[1].rect.x && cards[1].rect.x < cards[2].rect.x);
    }

    #[test]
    fn hook_text_is_measured_with_the_real_font() {
        let Some(f) = real() else { return };
        let b = lay(&f);
        // The h1 words are inline-block spans; each must have real width from
        // Space Grotesk, and they must sit left to right on one line.
        let line = b.find_id("s1-line1").expect("s1-line1");
        let words: Vec<&Boxes> = line.children.iter().filter(|c| c.has_class("w")).collect();
        assert!(words.len() >= 6, "expected the hook to be split into words");
        for w in &words {
            assert!(w.rect.w > 0.0, "word '{:?}' has no width", w.text);
        }
        for pair in words.windows(2) {
            assert!(
                pair[1].rect.x >= pair[0].rect.x,
                "words must advance rightward"
            );
        }
        // 60px type: the line box should be about 60*1.2 tall.
        assert!(
            (line.rect.h - 72.0).abs() < 8.0,
            "line height {}",
            line.rect.h
        );
    }

    #[test]
    fn uppercase_labels_are_transformed_before_measurement() {
        let Some(f) = real() else { return };
        let b = lay(&f);
        let label = b
            .walk()
            .into_iter()
            .find(|x| x.has_class("term-label"))
            .expect("term-label");
        let text = label.text.clone().unwrap_or_default();
        assert!(
            text.chars().any(|c| c.is_alphabetic()) && text == text.to_uppercase(),
            ".mono sets text-transform: uppercase, got {text:?}"
        );
    }
}

/// The container's extent along the flex main axis.
fn container_main_of(dir: Dir, content: Rect) -> f32 {
    match dir {
        Dir::Row => content.w,
        Dir::Column => content.h,
    }
}

/// Height available to a flex item being re-measured.
///
/// A row item's height is its own content; the container's height is what this
/// measurement is being used to compute, so it cannot be an input to it.
fn avail_h_hint(c: &Pending) -> f32 {
    c.size.1.max(1.0)
}
