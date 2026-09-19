//! Render one still frame of a real composition to a PNG.
//!
//!     cargo run --release --example still -- <composition-dir> <out.png> [--reveal]
//!
//! `--reveal` forces every element opaque, standing in for a timeline that has
//! played its entry tweens — useful before the timeline exists, because a
//! composition's frame zero is often deliberately blank.

use framefold::{css, fonts::Fonts, html, layout, paint, Canvas, Rect};

/// Reveals everything, optionally isolating one scene.
///
/// Scenes stack (`position: absolute; inset: 0`), so revealing all of them
/// paints the last one over the rest. Naming a scene hides its siblings, which
/// is the only way to inspect an individual one before a timeline exists.
struct RevealAll {
    only: Option<String>,
    scenes: Vec<String>,
}

impl paint::Animator for RevealAll {
    fn for_id(&self, id: Option<&str>) -> paint::Overrides {
        let opacity = match (&self.only, id) {
            (Some(want), Some(this)) if self.scenes.iter().any(|s| s == this) => {
                if this == want { 1.0 } else { 0.0 }
            }
            _ => 1.0,
        };
        paint::Overrides {
            opacity: Some(opacity),
            ..Default::default()
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().cloned().unwrap_or_else(|| {
        eprintln!("usage: still <composition-dir> <out.png> [--reveal]");
        std::process::exit(2);
    });
    let out = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "frame.png".to_string());
    let reveal = args.iter().any(|a| a == "--reveal");
    let only = args
        .iter()
        .position(|a| a == "--scene")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let at: Option<f32> = args
        .iter()
        .position(|a| a == "--at")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());

    let dir = std::path::Path::new(&dir);
    let src = std::fs::read_to_string(dir.join("index.html")).expect("read index.html");
    let root = html::parse(&src).expect("parse html");
    let sheet = css::parse(&root.collect_styles());
    let mut fonts = Fonts::load(&sheet.font_faces, dir).expect("load fonts");
    // A subsetted web font will not carry symbols like U+2713 CHECK MARK. A
    // browser falls back to an installed family per-glyph; give it one to fall
    // back to, or those characters paint as tofu boxes.
    let fallback = std::env::var("FRAMEFOLD_FALLBACK_FONT")
        .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".to_string());
    if let Ok(bytes) = std::fs::read(&fallback) {
        if let Ok(f) = ruler::ttf::Font::parse(bytes) {
            fonts = fonts.with_fallback(f);
        }
    }

    let comp = root
        .find_with_attr("data-composition-id")
        .expect("no data-composition-id in this composition");
    let w: u32 = comp.attr("data-width").and_then(|v| v.parse().ok()).unwrap_or(1920);
    let h: u32 = comp.attr("data-height").and_then(|v| v.parse().ok()).unwrap_or(1080);

    let t = std::time::Instant::now();
    let boxes = layout::Builder::new(&sheet, &fonts)
        .layout(comp, Rect::new(0.0, 0.0, w as f32, h as f32))
        .expect("layout");
    let laid = t.elapsed();

    let t = std::time::Instant::now();
    let mut canvas = Canvas::new(w, h);
    if let Some(t) = at {
        let script = root.collect_inline_scripts();
        // Extract against the same element layout used, or node indices disagree.
        let tl = framefold::timeline::extract(comp, &script).expect("timeline");
        eprintln!(
            "timeline: {} tweens over {:.2}s{}",
            tl.tweens.len(),
            tl.duration,
            if tl.uses_blur() { "  (uses blur — not yet rendered)" } else { "" }
        );
        let anim = framefold::timeline::FrameAnimator { timeline: &tl, time: t };
        paint::paint(&mut canvas, &boxes, &fonts, &anim).expect("paint");
    } else if reveal || only.is_some() {
        let scenes: Vec<String> = comp
            .children
            .iter()
            .filter_map(|c| match c {
                html::Node::Element(e) => e.id().map(|s| s.to_string()),
                _ => None,
            })
            .collect();
        paint::paint(&mut canvas, &boxes, &fonts, &RevealAll { only, scenes }).expect("paint");
    } else {
        paint::paint(&mut canvas, &boxes, &fonts, &paint::Static).expect("paint");
    }
    let painted = t.elapsed();

    let png = transcode::png::encode(w, h, &canvas.data, true).expect("encode png");
    std::fs::write(&out, &png).expect("write png");

    eprintln!(
        "{w}x{h}  layout {:.1}ms  paint {:.1}ms  -> {out} ({:.0} KB)",
        laid.as_secs_f64() * 1000.0,
        painted.as_secs_f64() * 1000.0,
        png.len() as f64 / 1024.0
    );
}
