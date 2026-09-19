//! Compare two renders of the same composition, frame by frame.
//!
//!     cargo run --release --example parity -- <a.mp4> <b.mp4> [--frames N]
//!
//! Reports SSIM per sampled frame and, for the worst ones, where the
//! differences actually sit — a scalar score cannot distinguish "text is
//! slightly soft everywhere" from "one card is 40px out of place", and those
//! call for completely different fixes.

use std::process::Command;

/// Accept either a video (frames are extracted) or a directory of PNGs.
///
/// Comparing two *encoded videos* measures the encoders as much as the
/// renderers: different H.264 implementations disagree slightly on nearly every
/// pixel, which floors SSIM around 0.96 no matter how identical the source
/// frames were. Uncompressed frames isolate the renderer.
fn frames_of(path: &str, dir: &str, fps: f32) -> Vec<String> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let mut v: Vec<String> = std::fs::read_dir(p)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "png").unwrap_or(false))
            .map(|p| p.display().to_string())
            .collect();
        v.sort();
        return v;
    }
    extract(path, dir, fps);
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path().display().to_string())
        .filter(|p| p.ends_with(".png"))
        .collect();
    v.sort();
    v
}

fn extract(video: &str, dir: &str, fps: f32) -> usize {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("mkdir");
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-i", video, "-vf", &format!("fps={fps}")])
        .arg(format!("{dir}/%04d.png"))
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to extract {video}");
    std::fs::read_dir(dir).unwrap().count()
}

fn load(path: &str) -> transcode::Image {
    let bytes = std::fs::read(path).expect("read png");
    transcode::png::decode(&bytes).expect("decode png")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (a, b) = match (args.first(), args.get(1)) {
        (Some(a), Some(b)) => (a.clone(), b.clone()),
        _ => {
            eprintln!("usage: parity <a.mp4> <b.mp4> [--fps N]");
            std::process::exit(2);
        }
    };
    let fps: f32 = args
        .iter()
        .position(|x| x == "--fps")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.0);

    let fa = frames_of(&a, "/var/tmp/parity-a", fps);
    let fb = frames_of(&b, "/var/tmp/parity-b", fps);
    let n = fa.len().min(fb.len());
    assert!(n > 0, "no frames to compare");
    // Sample evenly rather than comparing all 600; the interesting variation is
    // across the timeline, not between adjacent frames.
    let want = args
        .iter()
        .position(|x| x == "--samples")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(n.min(40));
    let step = (n / want.max(1)).max(1);
    eprintln!("comparing {} of {n} frame pairs\n", (n + step - 1) / step);

    let mut scores: Vec<(usize, f64)> = Vec::new();
    for i in (0..n).step_by(step) {
        let ia = load(&fa[i]);
        let ib = load(&fb[i]);
        match transcode::image::ssim::ssim(&ia, &ib) {
            Some(r) => scores.push((i, r.value)),
            None => eprintln!("frame {i}: incomparable (size mismatch)"),
        }
    }
    if scores.is_empty() {
        eprintln!("no comparable frames");
        std::process::exit(1);
    }

    let mean: f64 = scores.iter().map(|(_, s)| s).sum::<f64>() / scores.len() as f64;
    let mut sorted = scores.clone();
    sorted.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap());

    println!("frames compared : {}", scores.len());
    println!("mean SSIM       : {mean:.4}");
    println!("best  frame     : #{} {:.4}", sorted.last().unwrap().0, sorted.last().unwrap().1);
    println!("worst frame     : #{} {:.4}", sorted[0].0, sorted[0].1);
    println!("\nworst 8 frames (t = frame/{fps} seconds):");
    for (i, s) in sorted.iter().take(8) {
        println!("  #{i:<4} t={:5.2}s  SSIM {s:.4}", *i as f32 / 30.0);
    }

    // For the single worst frame, say where the disagreement lives.
    let (worst, _) = sorted[0];
    let ia = load(&fa[worst]);
    let ib = load(&fb[worst]);
    // transcode already ships the diff mask these clusters are built from.
    let mask = transcode::image::clusters::diff_mask(&ia, &ib).expect("diff mask");
    let differing = mask.iter().filter(|m| **m > 0).count();
    let clusters = transcode::image::clusters::clusters(&mask, ia.width, ia.height);
    println!(
        "\nworst frame #{worst}: {:.2}% of pixels differ, in {} regions",
        differing as f64 / mask.len() as f64 * 100.0,
        clusters.len()
    );
    for c in clusters.iter().take(10) {
        let b = &c.bbox;
        println!(
            "  region {:>4}x{:<4} at ({:>4},{:>4})",
            b.x_max - b.x_min + 1,
            b.y_max - b.y_min + 1,
            b.x_min,
            b.y_min
        );
    }
}
