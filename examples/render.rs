//! Render a whole composition to an .mp4 — the end-to-end job.
//!
//!     cargo run --release --example render -- <composition-dir> <out.mp4> [--fps N]
//!
//! No browser, no Node, no ffmpeg: parse, cascade, lay out, run the
//! composition's own timeline script, rasterize every frame in parallel, and
//! encode H.264 through transcode.

use framefold::{css, fonts::Fonts, html, layout, paint, timeline, Canvas, Rect};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first().cloned() else {
        eprintln!("usage: render <composition-dir> <out.mp4> [--fps N] [--seconds N]");
        std::process::exit(2);
    };
    let out = args.get(1).cloned().unwrap_or_else(|| "out.mp4".into());
    let flag = |name: &str| -> Option<f32> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
    };

    let dir = std::path::Path::new(&dir);
    let src = std::fs::read_to_string(dir.join("index.html")).expect("read index.html");
    let root = html::parse(&src).expect("parse html");
    let sheet = css::parse(&root.collect_styles());

    let mut fonts = Fonts::load(&sheet.font_faces, dir).expect("load fonts");
    let fallback = std::env::var("FRAMEFOLD_FALLBACK_FONT")
        .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into());
    if let Ok(bytes) = std::fs::read(&fallback) {
        if let Ok(f) = ruler::ttf::Font::parse(bytes) {
            fonts = fonts.with_fallback(f);
        }
    }

    let comp = root
        .find_with_attr("data-composition-id")
        .expect("no data-composition-id");
    let w: u32 = comp.attr("data-width").and_then(|v| v.parse().ok()).unwrap_or(1920);
    let h: u32 = comp.attr("data-height").and_then(|v| v.parse().ok()).unwrap_or(1080);
    let declared: f32 = comp
        .attr("data-duration")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0);

    let fps = flag("--fps").unwrap_or_else(|| {
        comp.attr("data-fps").and_then(|v| v.parse().ok()).unwrap_or(30.0)
    });
    let seconds = flag("--seconds").unwrap_or(declared);

    // Layout once. Geometry does not change over time — only the overrides a
    // frame is painted with — which is what makes frames independent.
    let boxes = layout::Builder::new(&sheet, &fonts)
        .layout(comp, Rect::new(0.0, 0.0, w as f32, h as f32))
        .expect("layout");

    let tl = timeline::extract(comp, &root.collect_inline_scripts()).expect("timeline");
    eprintln!(
        "{w}x{h} {fps}fps {seconds}s · {} tweens over {:.2}s",
        tl.tweens.len(),
        tl.duration
    );

    let total = (seconds * fps).round() as usize;
    let threads = std::env::var("FRAMEFOLD_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| {
            std::env::var("FRAMEFOLD_MEMORY_MB")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .map(|mb| framefold::render::threads_for_budget(mb, w, h))
        })
        .unwrap_or_else(framefold::default_threads);

    // Ask the timeline which frames are actually distinct before drawing any.
    //
    // A frame is a pure function of its overrides, so two times with the same
    // overrides are the same image — knowable without producing either. The
    // repeats are neither rendered nor encoded: they become all-skip P-frames,
    // which cost a handful of bytes and no pixels.
    let plan = tl.plan(total, fps);
    eprintln!(
        "{w}x{h} {fps}fps {seconds}s · {} tweens · {threads} threads",
        tl.tweens.len()
    );
    eprintln!(
        "plan: {} distinct of {total} frames ({:.0}% repeat, skipped entirely)",
        plan.distinct(),
        plan.repeat_fraction() * 100.0
    );

    let t0 = std::time::Instant::now();
    // Encode inside the fold, so a frame's 8.3 MB of pixels dies before the
    // next is drawn and only its ~25 KB sample survives.
    let idr: Vec<Vec<u8>> =
        framefold::render::fold_frames(plan.distinct(), threads, w, h, |k, canvas| {
            let anim = timeline::FrameAnimator {
                timeline: &tl,
                time: plan.distinct_times[k],
            };
            paint::paint(canvas, &boxes, &fonts, &anim).expect("paint");
            transcode::h264_inter::encode_idr_sample(
                &canvas.data,
                w,
                h,
                transcode::h264::DEFAULT_QP,
                k as u32,
            )
            .expect("encode")
        });
    let render_time = t0.elapsed();

    // Skips carry no pixels, so they are synthesised from the plan alone.
    let t1 = std::time::Instant::now();
    let (samples, sync) = transcode::h264_inter::assemble_planned(
        &idr,
        &plan.shows,
        w,
        h,
        transcode::h264::DEFAULT_QP,
    )
    .expect("assemble");
    let encode_time = t1.elapsed();
    let sps = transcode::h264::nal_unit_pub(3, 7, &transcode::h264bits::build_sps(w, h));
    let pps = transcode::h264::nal_unit_pub(3, 8, &transcode::h264bits::build_pps());
    drop(idr);

    let file = std::fs::File::create(&out).expect("create output");
    let mut w_out = std::io::BufWriter::new(file);
    transcode::mp4::write_h264_mp4_sync(
        &mut w_out,
        &sps,
        &pps,
        &samples,
        Some(&sync),
        w,
        h,
        fps.round() as u32,
    )
    .expect("write mp4");
    use std::io::Write as _;
    w_out.flush().expect("flush");

    let encoded: usize = samples.iter().map(|s| s.len()).sum();
    eprintln!(
        "render {:.2}s · encode {:.2}s · total {:.2}s · {:.1} MB · {out}",
        render_time.as_secs_f64(),
        encode_time.as_secs_f64(),
        (render_time + encode_time).as_secs_f64(),
        encoded as f64 / 1_048_576.0
    );
}
