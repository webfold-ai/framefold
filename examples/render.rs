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
    // FRAMEFOLD_MEMORY_MB picks the worker count from a footprint budget;
    // FRAMEFOLD_THREADS overrides it outright.
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

    // Fuse render and encode per frame.
    //
    // Each worker renders one frame into its own reused canvas, encodes it
    // immediately, and keeps only the ~25 KB sample. The 8.3 MB of pixels dies
    // before the next frame is drawn, so the live set is threads x frame and
    // does not grow with duration. Batching render-then-encode would also bound
    // memory, but serialises the two halves; folding keeps every worker busy
    // doing both.
    let live = framefold::render::live_frame_bytes(threads, w, h);
    eprintln!(
        "{w}x{h} {fps}fps {seconds}s · {} tweens · {threads} threads · live frame budget {:.0} MB",
        tl.tweens.len(),
        live as f64 / 1_048_576.0
    );

    let t0 = std::time::Instant::now();
    let samples: Vec<Vec<u8>> =
        framefold::render::fold_frames(total, threads, w, h, |i, canvas| {
            let t = i as f32 / fps;
            let anim = timeline::FrameAnimator { timeline: &tl, time: t };
            paint::paint(canvas, &boxes, &fonts, &anim).expect("paint");
            // Encode here, while this frame's pixels are still the only ones live.
            let (_, _, mut nal) = transcode::h264::encode_mp4_track_qp_threads(
                std::slice::from_ref(&canvas.data),
                w,
                h,
                transcode::h264::DEFAULT_QP,
                1,
            )
            .expect("encode");
            nal.pop().expect("one sample per frame")
        });
    let elapsed = t0.elapsed();

    // Parameter sets depend only on dimensions, so deriving them once from a
    // single frame is exact rather than an approximation.
    let probe = Canvas::new(w, h);
    let (sps, pps, _) = transcode::h264::encode_mp4_track_qp_threads(
        std::slice::from_ref(&probe.data),
        w,
        h,
        transcode::h264::DEFAULT_QP,
        1,
    )
    .expect("parameter sets");
    drop(probe);

    // Stream straight to the file. Building the .mp4 in memory would hold the
    // encoded stream a second time, which is the only term left that grows with
    // duration.
    let file = std::fs::File::create(&out).expect("create output");
    let mut w_out = std::io::BufWriter::new(file);
    let written = transcode::mp4::write_h264_mp4(
        &mut w_out,
        &sps,
        &pps,
        &samples,
        w,
        h,
        fps.round() as u32,
    )
    .expect("write mp4");
    use std::io::Write as _;
    w_out.flush().expect("flush");

    let encoded: usize = samples.iter().map(|s| s.len()).sum();
    let _ = written;
    eprintln!(
        "{total} frames in {:.2}s ({:.1} fps) · encoded stream {:.1} MB · {out}",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64(),
        encoded as f64 / 1_048_576.0
    );
}
