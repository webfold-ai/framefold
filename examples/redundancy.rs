//! How much of a composition is the same frame twice?
//!
//! A renderer that draws every frame from scratch is running a loop whose
//! result is often identical to the previous iteration. The timeline knows this
//! without comparing a single pixel: if no element's overrides changed between
//! two times, the frames are bit-identical by construction.

use framefold::{css, fonts::Fonts, html, layout, timeline, Rect};
use std::collections::HashMap;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: redundancy <dir>");
    let dir = std::path::Path::new(&dir);
    let src = std::fs::read_to_string(dir.join("index.html")).expect("read");
    let root = html::parse(&src).expect("parse");
    let sheet = css::parse(&root.collect_styles());
    let fonts = Fonts::load(&sheet.font_faces, dir).expect("fonts");
    let comp = root.find_with_attr("data-composition-id").expect("comp");
    let w: u32 = comp
        .attr("data-width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1920);
    let h: u32 = comp
        .attr("data-height")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1080);
    let secs: f32 = comp
        .attr("data-duration")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    let fps = 30.0f32;

    let boxes = layout::Builder::new(&sheet, &fonts)
        .layout(comp, Rect::new(0.0, 0.0, w as f32, h as f32))
        .expect("layout");
    let tl = timeline::extract(comp, &root.collect_inline_scripts()).expect("timeline");

    let nodes: Vec<usize> = {
        let mut v: Vec<usize> = tl.tweens.iter().map(|t| t.node).collect();
        v.sort_unstable();
        v.dedup();
        v
    };

    // Quantise to the precision a renderer can actually express: sub-pixel
    // translation and sub-1/255 opacity produce the same pixels.
    let signature = |t: f32| -> Vec<u8> {
        let mut sig = Vec::with_capacity(nodes.len() * 10);
        for &n in &nodes {
            let o = tl.overrides_at(n, t);
            let q = |v: f32, step: f32| ((v / step).round() as i32).to_le_bytes();
            sig.extend_from_slice(&q(o.opacity.unwrap_or(1.0), 1.0 / 255.0));
            sig.extend_from_slice(&q(o.translate.0, 0.25));
            sig.extend_from_slice(&q(o.translate.1, 0.25));
            sig.extend_from_slice(&q(o.scale.unwrap_or(1.0), 0.0005));
            sig.extend_from_slice(&q(o.inset_clip.map(|c| c.1).unwrap_or(0.0), 0.0005));
            sig.extend_from_slice(&q(o.blur.unwrap_or(0.0), 0.25));
        }
        sig
    };

    let total = (secs * fps).round() as usize;
    let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut first_of: Vec<usize> = Vec::with_capacity(total);
    let mut runs = 0usize;
    let mut prev: Option<Vec<u8>> = None;
    for i in 0..total {
        let s = signature(i as f32 / fps);
        if prev.as_ref() != Some(&s) {
            runs += 1;
        }
        prev = Some(s.clone());
        let n = seen.len();
        let idx = *seen.entry(s).or_insert(n);
        first_of.push(idx);
    }

    let distinct = seen.len();
    println!("frames            : {total}");
    println!(
        "distinct frames   : {distinct}  ({:.1}% of total)",
        distinct as f64 / total as f64 * 100.0
    );
    println!(
        "identical repeats : {}  ({:.1}% skippable)",
        total - distinct,
        (total - distinct) as f64 / total as f64 * 100.0
    );
    println!("runs of change    : {runs}");
    println!();
    println!(
        "A renderer that draws only distinct frames does {:.1}x less work;",
        total as f64 / distinct.max(1) as f64
    );
    println!(
        "an encoder told which frames repeat spends ~0 bits on {} of them.",
        total - distinct
    );
}
