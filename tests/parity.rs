//! Visual-regression gate against hyperframes.
//!
//! # Why uncompressed frames
//!
//! Comparing two encoded videos measures the *encoders* as much as the
//! renderers: two H.264 implementations disagree slightly on nearly every
//! pixel, which floors any similarity score around 0.96 no matter how identical
//! the source frames were. That is a ceiling, not a result, and a regression
//! would hide under it. This renders in-process and compares against
//! hyperframes' PNG sequence, so the only difference measured is the one that
//! matters.
//!
//! # Generating the reference
//!
//! ```sh
//! node <hyperframes>/bin/hyperframes.mjs render <composition> \
//!     --format png-sequence -o /var/tmp/hf-seq
//! ```
//!
//! The test skips when the reference is absent, so a checkout without it stays
//! green rather than failing for an unrelated reason.

use framefold::{css, fonts::Fonts, html, layout, paint, timeline, Canvas, Rect};

const COMP: &str = "/home/ctown/projects/glossa/brag-output/composition";
const REFERENCE: &str = "/var/tmp/hf-seq";
const FPS: f32 = 30.0;

/// Frames sampled across the timeline. Spread rather than contiguous: the
/// interesting variation is between scenes and through transitions, not
/// between neighbours.
const SAMPLES: &[usize] = &[1, 30, 60, 105, 150, 210, 240, 300, 360, 420, 480, 510, 540, 570, 599];

/// Mean SSIM the renderer must hold across the sampled frames.
///
/// Not 1.0, and it should not be: matching a browser's glyph hinting and
/// subpixel positioning exactly is not a goal this renderer pursues. The number
/// sits just under what the renderer currently achieves (0.971), so it catches
/// a regression without failing on the known, accepted difference.
///
/// The first frame does reach 1.0000 exactly — a frame with no text is a frame
/// with nothing to disagree about — which is why a static frame scoring below
/// 1.0 is worth investigating rather than accepting. That is how the
/// pre-timeline state bug was found: the hook was on screen in frame one.
const MEAN_FLOOR: f64 = 0.965;

/// No single frame may fall below this, so an average cannot hide one bad frame.
const WORST_FLOOR: f64 = 0.92;

struct Fixture {
    boxes: layout::Boxes,
    fonts: Fonts,
    timeline: timeline::Timeline,
    width: u32,
    height: u32,
}

fn build() -> Option<Fixture> {
    let src = std::fs::read_to_string(format!("{COMP}/index.html")).ok()?;
    let root = html::parse(&src).ok()?;
    let sheet = css::parse(&root.collect_styles());
    let mut fonts = Fonts::load(&sheet.font_faces, std::path::Path::new(COMP)).ok()?;
    if let Ok(bytes) = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf") {
        if let Ok(f) = ruler::ttf::Font::parse(bytes) {
            fonts = fonts.with_fallback(f);
        }
    }
    let comp = root.find_with_attr("data-composition-id")?;
    let width: u32 = comp.attr("data-width").and_then(|v| v.parse().ok())?;
    let height: u32 = comp.attr("data-height").and_then(|v| v.parse().ok())?;
    let boxes = layout::Builder::new(&sheet, &fonts)
        .layout(comp, Rect::new(0.0, 0.0, width as f32, height as f32))
        .ok()?;
    let timeline = timeline::extract(comp, &root.collect_inline_scripts()).ok()?;
    Some(Fixture { boxes, fonts, timeline, width, height })
}

fn render_at(f: &Fixture, frame: usize) -> Canvas {
    let anim = timeline::FrameAnimator {
        timeline: &f.timeline,
        // hyperframes numbers its sequence from 1, and frame N shows time
        // (N-1)/fps. Getting this off by one compares adjacent frames and
        // looks like a small uniform regression.
        time: (frame - 1) as f32 / FPS,
    };
    let mut c = Canvas::new(f.width, f.height);
    paint::paint(&mut c, &f.boxes, &f.fonts, &anim).expect("paint");
    c
}

fn reference(frame: usize) -> Option<transcode::Image> {
    let path = format!("{REFERENCE}/frame_{frame:06}.png");
    let bytes = std::fs::read(path).ok()?;
    transcode::png::decode(&bytes).ok()
}

fn as_image(c: Canvas) -> transcode::Image {
    transcode::Image { width: c.width, height: c.height, pixels: c.data }
}

#[test]
fn no_visual_regression_against_hyperframes() {
    let Some(f) = build() else {
        eprintln!("skip: composition not present");
        return;
    };
    if reference(1).is_none() {
        eprintln!("skip: no reference sequence at {REFERENCE} — see module docs");
        return;
    }

    let mut scores = Vec::new();
    for &frame in SAMPLES {
        let Some(want) = reference(frame) else { continue };
        let got = as_image(render_at(&f, frame));
        assert_eq!(
            (got.width, got.height),
            (want.width, want.height),
            "frame {frame} rendered at the wrong size"
        );
        let r = transcode::image::ssim::ssim(&want, &got)
            .unwrap_or_else(|| panic!("frame {frame} incomparable"));
        scores.push((frame, r.value));
    }
    assert!(!scores.is_empty(), "no frames compared");

    let mean: f64 = scores.iter().map(|(_, s)| s).sum::<f64>() / scores.len() as f64;
    let (worst_frame, worst) = scores
        .iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .copied()
        .unwrap();

    eprintln!("parity over {} frames: mean {mean:.4}, worst {worst:.4} (frame {worst_frame})", scores.len());
    for (frame, s) in &scores {
        eprintln!("  frame {frame:>4}  t={:5.2}s  SSIM {s:.4}", (*frame - 1) as f32 / FPS);
    }

    // On failure, say *where* the worst frame disagrees. A scalar cannot
    // distinguish "text is slightly soft everywhere" from "one card is 40px
    // out of place", and those want completely different fixes.
    if mean < MEAN_FLOOR || worst < WORST_FLOOR {
        let want = reference(worst_frame).unwrap();
        let got = as_image(render_at(&f, worst_frame));
        if let Some(mask) = transcode::image::clusters::diff_mask(&want, &got) {
            let regions = transcode::image::clusters::clusters(&mask, want.width, want.height);
            let differing = mask.iter().filter(|m| **m > 0).count();
            eprintln!(
                "worst frame {worst_frame}: {:.2}% of pixels differ in {} regions",
                differing as f64 / mask.len() as f64 * 100.0,
                regions.len()
            );
            for c in regions.iter().take(8) {
                let b = &c.bbox;
                eprintln!(
                    "  {:>5}x{:<5} at ({:>4},{:>4})",
                    b.x_max - b.x_min + 1,
                    b.y_max - b.y_min + 1,
                    b.x_min,
                    b.y_min
                );
            }
        }
    }

    assert!(mean >= MEAN_FLOOR, "mean SSIM {mean:.4} below floor {MEAN_FLOOR}");
    assert!(
        worst >= WORST_FLOOR,
        "frame {worst_frame} SSIM {worst:.4} below floor {WORST_FLOOR}"
    );
}

#[test]
fn collapsing_repeat_frames_changes_nothing() {
    // The render plan skips frames it believes cannot differ. If that belief is
    // ever wrong the output is silently incorrect, so it is checked directly:
    // every frame the plan calls a repeat must render bit-identically to the
    // frame it repeats.
    let Some(f) = build() else {
        eprintln!("skip: composition not present");
        return;
    };
    let total = 600;
    let plan = f.timeline.plan(total, FPS);
    assert!(plan.distinct() < total, "nothing collapsed — plan is inert");

    let mut checked = 0;
    for i in 1..total {
        if plan.shows[i] != plan.shows[i - 1] {
            continue;
        }
        // Claimed identical. Verify a sample of them rather than all 324,
        // which would take as long as the render this exists to justify.
        if checked >= 12 {
            break;
        }
        let a = render_at(&f, i); // frame numbers are 1-based -> time (i-1)/fps
        let b = render_at(&f, i + 1);
        assert_eq!(
            a.data, b.data,
            "plan claims frames {} and {} are identical, but they render differently",
            i, i + 1
        );
        checked += 1;
    }
    assert!(checked > 0, "no repeats were sampled");
    eprintln!("verified {checked} claimed-identical frame pairs are bit-identical");
}
