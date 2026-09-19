//! Painting a laid-out box tree onto a canvas.
//!
//! Backgrounds, borders, and text, in document order so later siblings paint
//! over earlier ones. Everything it draws comes from the computed styles the
//! cascade produced; nothing here invents a value.
//!
//! # Baselines are computed, not guessed
//!
//! A line box positions text by its *baseline*, which sits at
//! `half-leading + ascent` below the line's top, where half-leading is
//! `(line-height − (ascent + descent)) / 2`. Guessing "some fraction of the em"
//! instead puts every line at a slightly wrong height — uniformly, plausibly,
//! and wrong in a way that no single frame reveals but every diff does. The
//! metrics come from the font's own `hhea`.

use crate::css::length;
use crate::layout::Boxes;
use crate::raster::{Canvas, Rect, Rgba};
use crate::fonts::{weight_value, Fonts};

/// Per-element visual overrides applied at paint time.
///
/// This is the seam an animation timeline drives: layout runs once, and a
/// frame is produced by painting the same box tree with different overrides.
/// Keeping them out of the cascade is what makes rendering frame `t` a pure
/// function rather than a mutation of shared state.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    /// Multiplied with the element's own opacity.
    pub opacity: Option<f32>,
    /// Translation in pixels, applied to the element and its subtree.
    pub translate: (f32, f32),
    /// Uniform scale about the element's transform origin.
    pub scale: Option<f32>,
    /// `clip-path: inset(top right bottom left)` as fractions of the box,
    /// matching the CSS percentage form compositions use for wipe reveals.
    pub inset_clip: Option<(f32, f32, f32, f32)>,
    /// `filter: blur(Npx)` in pixels.
    pub blur: Option<f32>,
}

impl Overrides {
    pub fn is_identity(&self) -> bool {
        self.opacity.is_none()
            && self.translate == (0.0, 0.0)
            && self.scale.is_none()
            && self.inset_clip.is_none()
            && self.blur.is_none()
    }
}

/// Resolves per-element overrides for the frame being painted.
pub trait Animator {
    /// Overrides for the element with this id, at the current time.
    fn for_id(&self, id: Option<&str>) -> Overrides;

    /// Overrides by document-order index.
    ///
    /// Index rather than id is the primary key because a timeline routinely
    /// targets elements that have none — glossa animates every `<span class="w">`
    /// of its hook, and not one of them carries an id. Defaults to the id
    /// lookup so simple animators need only implement that.
    fn for_node(&self, _node: usize, id: Option<&str>) -> Overrides {
        self.for_id(id)
    }
}

/// An animator that changes nothing — paints the composition at rest.
pub struct Static;

impl Animator for Static {
    fn for_id(&self, _id: Option<&str>) -> Overrides {
        Overrides::default()
    }
}

thread_local! {
    /// Scratch layers, reused across frames on this worker. Nested blurs each
    /// take one, so the pool holds as many as the deepest nesting reached.
    static LAYERS: std::cell::RefCell<Vec<Canvas>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Take a cleared full-frame layer from this thread's pool.
fn take_layer(width: u32, height: u32) -> Canvas {
    LAYERS.with(|p| {
        let mut p = p.borrow_mut();
        while let Some(mut c) = p.pop() {
            if c.width == width && c.height == height {
                // Clearing costs a memset; allocating costs a memset plus a
                // page-fault storm on first touch.
                c.data.fill(0);
                return c;
            }
            // A layer for different dimensions is dropped rather than resized:
            // frame size does not change mid-render, so this happens once.
        }
        Canvas::new(width, height)
    })
}

/// Return a layer for reuse.
fn give_layer(c: Canvas) {
    LAYERS.with(|p| {
        let mut p = p.borrow_mut();
        // Bounded so a pathological composition cannot grow the pool without
        // limit; beyond this the layer is simply dropped.
        //
        // The layer is deliberately NOT released between frames. Freeing and
        // reallocating an 8.3 MB buffer per frame churns large blocks through
        // the allocator, and resident memory tracks the arena high-water mark
        // rather than live bytes — measured, that raised peak RSS from 112 MB to
        // 283 MB at four threads. Holding one idle layer per worker is cheaper
        // than repeatedly proving we do not need it.
        if p.len() < 2 {
            p.push(c);
        }
    })
}

/// Release this thread's pooled layers.
pub fn release_layers() {
    LAYERS.with(|p| p.borrow_mut().clear());
}

/// Forwards to an inner animator but substitutes one node's overrides.
///
/// Used to re-enter painting for a blurred subtree without recursing forever:
/// the second pass sees the same overrides minus the blur.
struct OverrideOnce<'a> {
    inner: &'a dyn Animator,
    node: usize,
    with: Overrides,
}

impl Animator for OverrideOnce<'_> {
    fn for_id(&self, id: Option<&str>) -> Overrides {
        self.inner.for_id(id)
    }
    fn for_node(&self, node: usize, id: Option<&str>) -> Overrides {
        if node == self.node {
            self.with.clone()
        } else {
            self.inner.for_node(node, id)
        }
    }
}

/// Paint a whole box tree.
pub fn paint(
    canvas: &mut Canvas,
    root: &Boxes,
    fonts: &Fonts,
    animator: &dyn Animator,
) -> Result<(), String> {
    paint_box(canvas, root, fonts, animator, 1.0, (0.0, 0.0), None, Xform::IDENTITY)
}

/// An accumulated uniform scale about a fixed origin.
///
/// A CSS transform applies to an element *and its whole subtree* — scaling a
/// card scales its text with it. Scaling only the element's own rectangle
/// leaves the contents at their original size, which reads as "nothing
/// happened" for a slow camera push-in and is wrong in a way that is easy to
/// miss frame by frame and obvious in a diff.
#[derive(Clone, Copy, Debug)]
pub struct Xform {
    pub scale: f32,
    pub origin: (f32, f32),
}

impl Xform {
    pub const IDENTITY: Xform = Xform {
        scale: 1.0,
        origin: (0.0, 0.0),
    };

    fn is_identity(&self) -> bool {
        (self.scale - 1.0).abs() < 1e-4
    }

    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.origin.0 + (x - self.origin.0) * self.scale,
            self.origin.1 + (y - self.origin.1) * self.scale,
        )
    }

    fn rect(&self, r: Rect) -> Rect {
        let (x, y) = self.point(r.x, r.y);
        Rect::new(x, y, r.w * self.scale, r.h * self.scale)
    }

    /// Compose an additional scale about `origin`, given in the coordinate
    /// space this transform already produces.
    ///
    /// Two uniform scales about different origins compose to a single uniform
    /// scale, so the pair collapses into one transform instead of needing a
    /// matrix stack. With `T1(p) = o1 + (p - o1)·s1` and `T2(q) = o2 + (q - o2)·s2`:
    ///
    /// ```text
    /// T2(T1(p)) = o2 + (o1 - o2)·s2 + (p - o1)·s1·s2
    ///           = o·(1 - S) + p·S          where S = s1·s2
    ///   =>  o   = [o2 + (o1 - o2)·s2 - o1·S] / (1 - S)
    /// ```
    ///
    /// `S == 1` means the two scales cancel, leaving a pure translation that a
    /// scale-about-origin cannot express; the origin is then irrelevant.
    fn then_scale(&self, scale: f32, origin: (f32, f32)) -> Xform {
        if (scale - 1.0).abs() < 1e-4 {
            return *self;
        }
        let total = self.scale * scale;
        let denom = 1.0 - total;
        if denom.abs() < 1e-6 {
            return Xform {
                scale: total,
                origin: self.origin,
            };
        }
        let solve = |o1: f32, o2: f32| {
            (o2 + (o1 - o2) * scale - o1 * total) / denom
        };
        Xform {
            scale: total,
            origin: (
                solve(self.origin.0, origin.0),
                solve(self.origin.1, origin.1),
            ),
        }
    }
}

/// Paint a whole box tree.

fn paint_box(
    canvas: &mut Canvas,
    b: &Boxes,
    fonts: &Fonts,
    animator: &dyn Animator,
    inherited_alpha: f32,
    offset: (f32, f32),
    clip: Option<Rect>,
    xform: Xform,
) -> Result<(), String> {
    let ov = animator.for_node(b.node_index, b.id.as_deref());

    // A blur applies to the element and everything inside it, so the subtree is
    // drawn into its own transparent layer, blurred, and composited back. CSS
    // filters work on the rendered result, not on each shape: blurring shapes
    // individually would let their soft edges show through one another instead
    // of blurring the composed image.
    if let Some(radius) = ov.blur.filter(|r| *r > 0.5) {
        // Borrow a scratch layer instead of allocating one.
        //
        // A blurred element needs a full-frame transparent layer, and a naive
        // `Canvas::new` here costs 8.3 MB at 1080p for every blurred element on
        // every frame — multiplied by the number of render threads, since each
        // is doing this at the same time. Layers are pooled per thread and
        // returned when the subtree finishes, so the cost is paid once per
        // worker per nesting depth.
        let mut layer = take_layer(canvas.width, canvas.height);
        // The layer covers the whole canvas so coordinates stay absolute, but
        // only the element's own area plus the blur's reach can contain ink,
        // and blurring/compositing outside that is wasted work on a 1080p frame.
        let reach = radius * 3.0 + 2.0;
        let touched = Rect::new(
            b.rect.x + offset.0 - reach,
            b.rect.y + offset.1 - reach,
            b.rect.w + reach * 2.0,
            b.rect.h + reach * 2.0,
        );
        let blur_clip = Some(match clip {
            Some(c) => c.intersect(&touched),
            None => touched.intersect(&canvas.bounds()),
        });
        let mut inner = ov.clone();
        inner.blur = None;
        paint_box(
            &mut layer,
            b,
            fonts,
            &OverrideOnce {
                inner: animator,
                node: b.node_index,
                with: inner,
            },
            inherited_alpha,
            offset,
            clip,
            xform,
        )?;
        crate::raster::blur_region(&mut layer, radius, blur_clip);
        crate::raster::composite(canvas, &layer, 1.0, blur_clip);
        give_layer(layer);
        return Ok(());
    }

    // Opacity multiplies down the tree, as CSS group opacity does.
    // An override REPLACES the element's own opacity rather than scaling it.
    // A tween assigns `element.style.opacity`, which outranks the stylesheet —
    // so multiplying here would make it impossible to reveal anything the
    // stylesheet set to `opacity: 0`, which is how every reveal in a real
    // composition is written. Inherited (group) opacity still multiplies, as
    // CSS specifies.
    let own_opacity = ov.opacity.unwrap_or_else(|| {
        b.styles
            .get("opacity")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(1.0)
    });
    let alpha = inherited_alpha * own_opacity;
    if alpha <= 0.0005 {
        // Fully transparent subtrees are skipped entirely — a composition
        // hides three of its four scenes this way, so this is the common case,
        // not an optimisation for a rare one.
        return Ok(());
    }

    let offset = (offset.0 + ov.translate.0, offset.1 + ov.translate.1);
    let untransformed = Rect::new(
        b.rect.x + offset.0,
        b.rect.y + offset.1,
        b.rect.w,
        b.rect.h,
    );

    // A scale applies about the element's transform-origin and then carries
    // into the subtree, so it is accumulated rather than applied to this box
    // alone.
    let xform = match ov.scale {
        None => xform,
        Some(s) => {
            let base = xform.rect(untransformed);
            let origin = transform_origin(b, base);
            xform.then_scale(s, origin)
        }
    };
    let rect = xform.rect(untransformed);

    // Clipping accumulates: a child can only narrow what its parent allowed.
    let mut clip = clip;
    if b.styles.get("overflow") == Some("hidden") {
        clip = Some(match clip {
            Some(c) => c.intersect(&rect),
            None => rect,
        });
    }
    if let Some(ins) = ov.inset_clip.or_else(|| static_inset_clip(b)) {
        let (t, r, bo, l) = ins;
        let cut = Rect::new(
            rect.x + rect.w * l,
            rect.y + rect.h * t,
            (rect.w * (1.0 - l - r)).max(0.0),
            (rect.h * (1.0 - t - bo)).max(0.0),
        );
        clip = Some(match clip {
            Some(c) => c.intersect(&cut),
            None => cut,
        });
    }

    // Background.
    if let Some(bg) = b.styles.color("background-color") {
        if bg.a > 0 {
            let radius = b
                .styles
                .get("border-radius")
                .and_then(|v| length(v, b.font_size, b.rect.w))
                .unwrap_or(0.0);
            if radius > 0.0 {
                canvas.fill_round_rect(rect, radius, bg, alpha, clip);
            } else {
                canvas.fill_rect(rect, bg, alpha, clip);
            }
        }
    }

    paint_borders(canvas, b, rect, alpha, clip);

    // Text.
    if let Some(text) = &b.text {
        if !text.trim().is_empty() {
            paint_text(canvas, b, text, rect, fonts, alpha, clip, xform)?;
        }
    }

    for child in &b.children {
        paint_box(canvas, child, fonts, animator, alpha, offset, clip, xform)?;
    }
    Ok(())
}

/// Each border edge is painted separately, because a composition sets only
/// some of them (`border-top: 3px` on a card) and stroking the whole box would
/// draw three edges that were never asked for.
fn paint_borders(canvas: &mut Canvas, b: &Boxes, rect: Rect, alpha: f32, clip: Option<Rect>) {
    for (side, edge) in [
        ("top", Rect::new(rect.x, rect.y, rect.w, 0.0)),
        ("bottom", Rect::new(rect.x, rect.bottom(), rect.w, 0.0)),
        ("left", Rect::new(rect.x, rect.y, 0.0, rect.h)),
        ("right", Rect::new(rect.right(), rect.y, 0.0, rect.h)),
    ] {
        let Some(w) = b
            .styles
            .get(&format!("border-{side}-width"))
            .and_then(|v| length(v, b.font_size, rect.w))
        else {
            continue;
        };
        if w <= 0.0 {
            continue;
        }
        let Some(color) = b.styles.color(&format!("border-{side}-color")) else {
            continue;
        };
        let r = match side {
            "top" => Rect::new(edge.x, edge.y, edge.w, w),
            "bottom" => Rect::new(edge.x, edge.y - w, edge.w, w),
            "left" => Rect::new(edge.x, edge.y, w, edge.h),
            _ => Rect::new(edge.x - w, edge.y, w, edge.h),
        };
        canvas.fill_rect(r, color, alpha, clip);
    }
}

fn paint_text(
    canvas: &mut Canvas,
    b: &Boxes,
    text: &str,
    rect: Rect,
    fonts: &Fonts,
    alpha: f32,
    clip: Option<Rect>,
    xform: Xform,
) -> Result<(), String> {
    let family = b.styles.get("font-family").unwrap_or("sans-serif");
    let weight = weight_value(b.styles.get("font-weight"));
    let Some(font) = fonts.resolve(family, weight) else {
        // Drawing nothing is preferable to drawing the wrong typeface, and the
        // caller can detect the empty region.
        return Ok(());
    };
    let color = b.styles.color("color").unwrap_or(Rgba::BLACK);
    // Type scales with the element, which is the whole point of a push-in.
    let size = b.font_size * xform.scale;
    let letter_spacing = b
        .styles
        .get("letter-spacing")
        .and_then(|v| length(v, b.font_size, 0.0))
        .unwrap_or(0.0)
        * xform.scale;

    let line_height = b
        .styles
        .get("line-height")
        .and_then(|v| {
            v.trim()
                .parse::<f32>()
                .ok()
                .map(|m| m * b.font_size)
                .or_else(|| length(v, b.font_size, b.font_size))
        })
        .unwrap_or(b.font_size * 1.2)
        * xform.scale;

    // Baseline = top + half-leading + ascent.
    let ascent = font.ascent(size);
    let descent = font.descent(size);
    let half_leading = (line_height - (ascent + descent)) / 2.0;
    let content_top = rect.y + (b.content.y - b.rect.y) * xform.scale;
    let baseline = content_top + half_leading + ascent;

    let content_x = rect.x + (b.content.x - b.rect.x) * xform.scale;
    let content_w = b.content.w * xform.scale;

    // Wrap to exactly the width layout used. Recomputing it here would look
    // equivalent and is not — see Boxes::text_wrap.
    // Wrap at the layout width scaled to match, so a scaled box breaks its
    // text in the same places rather than re-flowing as it grows.
    let lines = crate::text::break_lines(
        font,
        size,
        letter_spacing,
        text,
        b.text_wrap.map(|w| w * xform.scale),
    )?;

    let align = b.styles.get("text-align").unwrap_or("left");
    for (i, (line, width)) in lines.iter().enumerate() {
        let x = match align {
            "center" => content_x + (content_w - width) / 2.0,
            "right" | "end" => content_x + content_w - width,
            _ => content_x,
        };
        let y = baseline + i as f32 * line_height;
        let mut pen = x;
        for ch in line.chars() {
            // Per-character fallback: a subsetted web font will be missing
            // symbols the composition uses, and drawing .notdef paints a tofu box.
            let f = fonts.resolve_for_char(family, weight, ch).unwrap_or(font);
            let gid = f.glyph_id(ch);
            crate::glyphcache::draw_glyph(canvas, f, gid, size, pen, y, color, alpha, clip)?;
            pen += f.advance(gid, size)? + letter_spacing;
        }
    }
    Ok(())
}

/// `transform-origin` in absolute pixels. Defaults to the box's centre, as CSS
/// does; compositions override it (`50% 100%`, `50% 0%`) to anchor growth.
fn transform_origin(b: &Boxes, rect: Rect) -> (f32, f32) {
    let Some(v) = b.styles.get("transform-origin") else {
        return (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
    };
    let parts: Vec<&str> = v.split_whitespace().collect();
    let resolve = |s: Option<&&str>, basis: f32, default: f32| -> f32 {
        match s {
            Some(t) => length(t, b.font_size, basis).unwrap_or(default),
            None => default,
        }
    };
    (
        rect.x + resolve(parts.first(), rect.w, rect.w / 2.0),
        rect.y + resolve(parts.get(1), rect.h, rect.h / 2.0),
    )
}

/// A `clip-path: inset(...)` written in the stylesheet, as fractions.
///
/// Only the percentage form is read. A pixel inset would need the box size to
/// convert, and returning None leaves the element unclipped rather than
/// clipped by a wrong amount.
fn static_inset_clip(b: &Boxes) -> Option<(f32, f32, f32, f32)> {
    let v = b.styles.get("clip-path")?;
    parse_inset(v)
}

/// Parse `inset(0 100% 0 0)` into (top, right, bottom, left) fractions.
pub fn parse_inset(v: &str) -> Option<(f32, f32, f32, f32)> {
    let inner = v.trim().strip_prefix("inset(")?.strip_suffix(')')?;
    let parts: Vec<f32> = inner
        .split_whitespace()
        .map(|t| {
            t.strip_suffix('%')
                .and_then(|n| n.parse::<f32>().ok())
                .map(|p| p / 100.0)
                .or_else(|| t.parse::<f32>().ok().filter(|n| *n == 0.0))
        })
        .collect::<Option<Vec<f32>>>()?;
    Some(match parts.len() {
        1 => (parts[0], parts[0], parts[0], parts[0]),
        2 => (parts[0], parts[1], parts[0], parts[1]),
        3 => (parts[0], parts[1], parts[2], parts[1]),
        4 => (parts[0], parts[1], parts[2], parts[3]),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{css, html, layout};

    const COMP: &str = "/home/ctown/projects/glossa/brag-output/composition";

    fn ink(c: &Canvas) -> usize {
        c.data.chunks_exact(4).filter(|p| p[3] > 0).count()
    }

    fn render_synth(html_src: &str, css_src: &str, w: u32, h: u32) -> Canvas {
        let root = html::parse(html_src).unwrap();
        let sheet = css::parse(css_src);
        let fonts = Fonts::default();
        let target = match &root.children[0] {
            html::Node::Element(e) => e.clone(),
            _ => panic!("expected element"),
        };
        let boxes = layout::Builder::new(&sheet, &fonts)
            .layout(&target, Rect::new(0.0, 0.0, w as f32, h as f32))
            .unwrap();
        let mut canvas = Canvas::new(w, h);
        paint(&mut canvas, &boxes, &fonts, &Static).unwrap();
        canvas
    }

    #[test]
    fn background_paints_the_border_box() {
        let c = render_synth(
            "<div class='a'></div>",
            ".a { width: 10px; height: 10px; background: #ff0000 }",
            20,
            20,
        );
        assert_eq!(c.pixel(5, 5), Rgba::rgb(255, 0, 0));
        assert_eq!(c.pixel(15, 15).a, 0);
        assert_eq!(ink(&c), 100);
    }

    #[test]
    fn only_declared_border_edges_are_drawn() {
        // .card sets border-top only; painting all four would add three edges
        // the stylesheet never asked for.
        let c = render_synth(
            "<div class='a'></div>",
            ".a { width: 20px; height: 20px; border-top: 4px solid #00ff00 }",
            30,
            30,
        );
        assert_eq!(c.pixel(10, 1), Rgba::rgb(0, 255, 0), "top edge drawn");
        assert_eq!(c.pixel(10, 18).a, 0, "bottom edge must be absent");
        assert_eq!(c.pixel(1, 10).a, 0, "left edge must be absent");
        assert_eq!(ink(&c), 20 * 4);
    }

    #[test]
    fn opacity_multiplies_down_the_tree() {
        let c = render_synth(
            "<div class='p'><div class='c'></div></div>",
            ".p { width: 10px; height: 10px; opacity: 0.5 } .c { width: 10px; height: 10px; background: #ffffff; opacity: 0.5 }",
            10,
            10,
        );
        let a = c.pixel(5, 5).a;
        assert!((55..=73).contains(&a), "0.5*0.5 should be ~25% alpha, got {a}");
    }

    #[test]
    fn a_transparent_subtree_is_skipped() {
        let c = render_synth(
            "<div class='p'><div class='c'></div></div>",
            ".p { width: 10px; height: 10px; opacity: 0 } .c { width: 10px; height: 10px; background: #ffffff }",
            10,
            10,
        );
        assert_eq!(ink(&c), 0, "opacity:0 hides the whole subtree");
    }

    #[test]
    fn overflow_hidden_clips_children() {
        let c = render_synth(
            "<div class='p'><div class='c'></div></div>",
            ".p { width: 10px; height: 20px; overflow: hidden } .c { width: 40px; height: 20px; background: #ffffff }",
            40,
            20,
        );
        assert_eq!(c.pixel(5, 5), Rgba::WHITE);
        assert_eq!(c.pixel(20, 5).a, 0, "child must be clipped to the parent");
    }

    #[test]
    fn inset_clip_parses_the_wipe_form() {
        assert_eq!(parse_inset("inset(0 100% 0 0)"), Some((0.0, 1.0, 0.0, 0.0)));
        assert_eq!(parse_inset("inset(0 0% 0 0)"), Some((0.0, 0.0, 0.0, 0.0)));
        assert_eq!(parse_inset("inset(10%)"), Some((0.1, 0.1, 0.1, 0.1)));
        assert_eq!(parse_inset("circle(50%)"), None);
    }

    #[test]
    fn a_full_inset_clip_hides_the_box() {
        let c = render_synth(
            "<div class='a'></div>",
            ".a { width: 20px; height: 20px; background: #fff; clip-path: inset(0 100% 0 0) }",
            20,
            20,
        );
        assert_eq!(ink(&c), 0, "a 100% right inset leaves nothing visible");
    }

    #[test]
    fn transform_origin_defaults_to_centre() {
        let root = html::parse("<div class='a'></div>").unwrap();
        let sheet = css::parse(".a { width: 20px; height: 10px }");
        let fonts = Fonts::default();
        let target = match &root.children[0] {
            html::Node::Element(e) => e.clone(),
            _ => unreachable!(),
        };
        let b = layout::Builder::new(&sheet, &fonts)
            .layout(&target, Rect::new(0.0, 0.0, 100.0, 100.0))
            .unwrap();
        assert_eq!(transform_origin(&b, b.rect), (10.0, 5.0));
    }

    // ── the real composition ────────────────────────────────────────────────

    fn real_frame() -> Option<(Canvas, layout::Boxes)> {
        let src = std::fs::read_to_string(format!("{COMP}/index.html")).ok()?;
        let root = html::parse(&src).ok()?;
        let sheet = css::parse(&root.collect_styles());
        let fonts = Fonts::load(&sheet.font_faces, std::path::Path::new(COMP)).ok()?;
        let comp = root.find_with_attr("data-composition-id")?;
        let boxes = layout::Builder::new(&sheet, &fonts)
            .layout(comp, Rect::new(0.0, 0.0, 1920.0, 1080.0))
            .ok()?;
        let mut canvas = Canvas::new(1920, 1080);
        paint(&mut canvas, &boxes, &fonts, &Static).ok()?;
        Some((canvas, boxes))
    }

    fn dark_pixels(c: &Canvas) -> usize {
        c.data
            .chunks_exact(4)
            .filter(|p| p[3] > 0 && p[0] < 100 && p[1] < 100 && p[2] < 100)
            .count()
    }

    #[test]
    fn the_real_composition_at_rest_is_background_only() {
        let Some((c, _)) = real_frame() else {
            eprintln!("skip: composition not present");
            return;
        };
        assert_eq!(
            c.pixel(10, 10),
            Rgba::rgb(248, 248, 248),
            "scene background should be #f8f8f8"
        );
        // Every word of the hook is `.w { opacity: 0 }` — the timeline fades
        // them in. So frame zero is correctly an empty light frame, and text
        // appearing here would mean the initial state was ignored.
        assert_eq!(
            dark_pixels(&c),
            0,
            "nothing should be visible before the timeline runs"
        );
    }

    /// Reveals every element, standing in for the timeline's opacity tweens.
    struct RevealAll;
    impl Animator for RevealAll {
        fn for_id(&self, _id: Option<&str>) -> Overrides {
            Overrides {
                opacity: Some(1.0),
                ..Default::default()
            }
        }
    }

    #[test]
    fn overrides_reveal_the_hook_text() {
        let src = match std::fs::read_to_string(format!("{COMP}/index.html")) {
            Ok(s) => s,
            Err(_) => return,
        };
        let root = html::parse(&src).unwrap();
        let sheet = css::parse(&root.collect_styles());
        let fonts = Fonts::load(&sheet.font_faces, std::path::Path::new(COMP)).unwrap();
        let comp = root.find_with_attr("data-composition-id").unwrap();
        let boxes = layout::Builder::new(&sheet, &fonts)
            .layout(comp, Rect::new(0.0, 0.0, 1920.0, 1080.0))
            .unwrap();

        let mut c = Canvas::new(1920, 1080);
        paint(&mut c, &boxes, &fonts, &RevealAll).unwrap();

        // With opacity overridden the hook renders in the composition's own
        // dark ink, proving the whole chain: cascade, layout, real Space
        // Grotesk outlines, rasterizer.
        let dark = dark_pixels(&c);
        assert!(dark > 5_000, "expected real hook text, got {dark} px");
    }

    #[test]
    fn hidden_scenes_do_not_paint() {
        let Some((_, boxes)) = real_frame() else { return };
        for id in ["s2", "s3", "s4"] {
            let s = boxes.find_id(id).unwrap();
            assert_eq!(
                s.styles.get("opacity"),
                Some("0"),
                "{id} should start hidden"
            );
        }
        // #s4's dark background must not appear anywhere in the frame.
        let (c, _) = real_frame().unwrap();
        let outro_bg = c
            .data
            .chunks_exact(4)
            .filter(|p| p[0] == 27 && p[1] == 27 && p[2] == 31)
            .count();
        assert_eq!(outro_bg, 0, "the hidden outro background leaked into frame 0");
    }
}

#[cfg(test)]
mod xform_tests {
    use super::*;

    fn approx(a: (f32, f32), b: (f32, f32)) {
        assert!(
            (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01,
            "{a:?} != {b:?}"
        );
    }

    #[test]
    fn identity_leaves_points_alone() {
        approx(Xform::IDENTITY.point(37.0, 91.0), (37.0, 91.0));
    }

    #[test]
    fn a_single_scale_fixes_its_origin() {
        let x = Xform::IDENTITY.then_scale(2.0, (100.0, 100.0));
        approx(x.point(100.0, 100.0), (100.0, 100.0));
        approx(x.point(110.0, 100.0), (120.0, 100.0));
    }

    #[test]
    fn nested_scales_compose_like_applying_them_in_turn() {
        // Compose, then compare against applying each in sequence by hand.
        let s1 = 1.035;
        let o1 = (300.0, 400.0);
        let s2 = 1.04;
        let o2 = (960.0, 540.0);

        let composed = Xform::IDENTITY.then_scale(s1, o1).then_scale(s2, o2);
        for p in [(0.0, 0.0), (500.0, 250.0), (1920.0, 1080.0)] {
            let a = (o1.0 + (p.0 - o1.0) * s1, o1.1 + (p.1 - o1.1) * s1);
            let b = (o2.0 + (a.0 - o2.0) * s2, o2.1 + (a.1 - o2.1) * s2);
            approx(composed.point(p.0, p.1), b);
        }
        assert!((composed.scale - s1 * s2).abs() < 1e-5);
    }

    #[test]
    fn cancelling_scales_do_not_divide_by_zero() {
        let x = Xform::IDENTITY.then_scale(2.0, (10.0, 10.0)).then_scale(0.5, (90.0, 90.0));
        assert!((x.scale - 1.0).abs() < 1e-4);
        assert!(x.point(5.0, 5.0).0.is_finite());
    }

    #[test]
    fn scaling_grows_a_rect_about_its_origin() {
        let x = Xform::IDENTITY.then_scale(2.0, (0.0, 0.0));
        let r = x.rect(Rect::new(10.0, 20.0, 30.0, 40.0));
        assert_eq!((r.x, r.y, r.w, r.h), (20.0, 40.0, 60.0, 80.0));
    }
}
