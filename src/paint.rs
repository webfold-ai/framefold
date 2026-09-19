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
use crate::raster::{Canvas, Path, Rect, Rgba};
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
}

impl Overrides {
    pub fn is_identity(&self) -> bool {
        self.opacity.is_none()
            && self.translate == (0.0, 0.0)
            && self.scale.is_none()
            && self.inset_clip.is_none()
    }
}

/// Resolves per-element overrides for the frame being painted.
pub trait Animator {
    /// Overrides for the element with this id, at the current time.
    fn for_id(&self, id: Option<&str>) -> Overrides;
}

/// An animator that changes nothing — paints the composition at rest.
pub struct Static;

impl Animator for Static {
    fn for_id(&self, _id: Option<&str>) -> Overrides {
        Overrides::default()
    }
}

/// Paint a whole box tree.
pub fn paint(
    canvas: &mut Canvas,
    root: &Boxes,
    fonts: &Fonts,
    animator: &dyn Animator,
) -> Result<(), String> {
    paint_box(canvas, root, fonts, animator, 1.0, (0.0, 0.0), None)
}

fn paint_box(
    canvas: &mut Canvas,
    b: &Boxes,
    fonts: &Fonts,
    animator: &dyn Animator,
    inherited_alpha: f32,
    offset: (f32, f32),
    clip: Option<Rect>,
) -> Result<(), String> {
    let ov = animator.for_id(b.id.as_deref());

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
    let rect = Rect::new(
        b.rect.x + offset.0,
        b.rect.y + offset.1,
        b.rect.w,
        b.rect.h,
    );

    // A scale is applied about the element's transform-origin, so a card that
    // grows does so from its own anchor rather than the frame's corner.
    let rect = match ov.scale {
        None => rect,
        Some(s) => {
            let (ox, oy) = transform_origin(b, rect);
            Rect::new(
                ox + (rect.x - ox) * s,
                oy + (rect.y - oy) * s,
                rect.w * s,
                rect.h * s,
            )
        }
    };

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
            paint_text(canvas, b, text, rect, fonts, alpha, clip)?;
        }
    }

    for child in &b.children {
        paint_box(canvas, child, fonts, animator, alpha, offset, clip)?;
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
) -> Result<(), String> {
    let family = b.styles.get("font-family").unwrap_or("sans-serif");
    let weight = weight_value(b.styles.get("font-weight"));
    let Some(font) = fonts.resolve(family, weight) else {
        // Drawing nothing is preferable to drawing the wrong typeface, and the
        // caller can detect the empty region.
        return Ok(());
    };
    let color = b.styles.color("color").unwrap_or(Rgba::BLACK);
    let size = b.font_size;
    let letter_spacing = b
        .styles
        .get("letter-spacing")
        .and_then(|v| length(v, size, 0.0))
        .unwrap_or(0.0);

    let line_height = b
        .styles
        .get("line-height")
        .and_then(|v| {
            v.trim()
                .parse::<f32>()
                .ok()
                .map(|m| m * size)
                .or_else(|| length(v, size, size))
        })
        .unwrap_or(size * 1.2);

    // Baseline = top + half-leading + ascent.
    let ascent = font.ascent(size);
    let descent = font.descent(size);
    let half_leading = (line_height - (ascent + descent)) / 2.0;
    let content_top = rect.y + (b.content.y - b.rect.y);
    let baseline = content_top + half_leading + ascent;

    let content_x = rect.x + (b.content.x - b.rect.x);
    let content_w = b.content.w;

    // Wrap to exactly the width layout used. Recomputing it here would look
    // equivalent and is not — see Boxes::text_wrap.
    let lines = crate::text::break_lines(font, size, letter_spacing, text, b.text_wrap)?;

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
            let contours = f.outline(gid)?;
            if !contours.is_empty() {
                let scale = size / f.units_per_em() as f32;
                let path = Path::from_glyph(&contours, scale, pen, y);
                canvas.fill_path(&path, color, alpha, clip);
            }
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
