//! End-to-end: compose frames natively, encode them, and have a real decoder
//! confirm the result.
//!
//! This is the claim the crate exists to make — a composition becomes an .mp4
//! with no browser process anywhere in the pipeline. Unit tests prove the
//! rasterizer agrees with itself; this proves the output is a video that
//! something other than us will play.

use framefold::{Align, Canvas, Rect, Rgba, TextStyle};
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn font() -> Option<ruler::ttf::Font> {
    let data = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf").ok()?;
    Some(ruler::ttf::Font::parse(data).expect("parse DejaVuSans"))
}

/// Render one frame of a small "stat card" scene at time `t` in [0,1].
///
/// Deliberately shaped like what a promo composition actually does: a dark
/// backdrop, a rounded card that slides up and fades in, a hairline border, and
/// two lines of text. Every value is a pure function of `t`, which is the same
/// determinism contract a seekable timeline requires.
fn render_frame(font: &ruler::ttf::Font, w: u32, h: u32, t: f32) -> Vec<u8> {
    let mut c = Canvas::filled(w, h, Rgba::rgb(12, 12, 16));

    // Ease-out cubic, so the motion decelerates the way a real tween would.
    let e = 1.0 - (1.0 - t).powi(3);
    let slide = (1.0 - e) * 40.0;
    let card = Rect::new(60.0, 60.0 + slide, w as f32 - 120.0, h as f32 - 140.0);

    c.fill_round_rect(card, 14.0, Rgba::rgb(24, 24, 32), e, None);
    c.stroke_rect(card, 1.0, Rgba::rgb(70, 70, 90), e, None);

    let title = TextStyle::new(font, 34.0, Rgba::WHITE).align(Align::Center);
    let sub = TextStyle::new(font, 18.0, Rgba::rgb(150, 150, 170)).align(Align::Center);

    framefold::draw_text(
        &mut c,
        &title,
        "framefold",
        Rect::new(card.x, card.y + 40.0, card.w, 60.0),
        Some(card),
        e,
    )
    .expect("title");
    framefold::draw_text(
        &mut c,
        &sub,
        "rendered without a browser",
        Rect::new(card.x, card.y + 92.0, card.w, 40.0),
        Some(card),
        e,
    )
    .expect("subtitle");

    c.into_rgba()
}

#[test]
fn frames_are_deterministic_for_the_same_time() {
    let Some(f) = font() else {
        eprintln!("skip: DejaVuSans not installed");
        return;
    };
    // Seek-safety: asking for the same instant twice must produce identical
    // pixels, or parallel or out-of-order rendering would not be sound.
    let a = render_frame(&f, 320, 180, 0.37);
    let b = render_frame(&f, 320, 180, 0.37);
    assert_eq!(a, b, "the same time must render identically");

    let c = render_frame(&f, 320, 180, 0.90);
    assert_ne!(a, c, "different times must differ");
}

#[test]
fn animation_actually_progresses() {
    let Some(f) = font() else { return };
    // The card fades in, so later frames carry strictly more ink than early ones.
    let ink = |t: f32| {
        render_frame(&f, 320, 180, t)
            .chunks_exact(4)
            .filter(|p| p[0] > 20) // brighter than the backdrop
            .count()
    };
    let (early, late) = (ink(0.05), ink(1.0));
    assert!(
        late > early,
        "fade-in should add ink over time: {early} -> {late}"
    );
}

#[test]
fn rendered_frames_encode_to_a_playable_mp4() {
    if !ffmpeg_available() {
        eprintln!("skip: ffmpeg/ffprobe not installed");
        return;
    }
    let Some(f) = font() else {
        eprintln!("skip: DejaVuSans not installed");
        return;
    };

    let (w, h, fps) = (320u32, 180u32, 12u32);
    let n = 24;
    let frames: Vec<Vec<u8>> = (0..n)
        .map(|i| render_frame(&f, w, h, i as f32 / (n - 1) as f32))
        .collect();

    // A quiet tone, so the audio track carries something a decoder must
    // actually parse rather than a degenerate all-zero signal.
    let rate = 44100u32;
    let seconds = n as f32 / fps as f32;
    let pcm: Vec<i16> = (0..(rate as f32 * seconds) as usize)
        .map(|i| {
            let x = i as f32 / rate as f32;
            ((x * 220.0 * std::f32::consts::TAU).sin() * 6000.0) as i16
        })
        .collect();

    let mp4 = transcode::mp4::encode_rgba_h264_aac(
        &frames,
        w,
        h,
        fps,
        transcode::h264::DEFAULT_QP,
        &pcm,
        rate,
        1,
        96,
    )
    .expect("encode");

    assert!(mp4.len() > 1000, "suspiciously small output");
    let path = std::env::temp_dir().join("framefold_pipeline_test.mp4");
    std::fs::write(&path, &mp4).expect("write mp4");

    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe rejected the file: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info = String::from_utf8_lossy(&out.stdout);
    assert!(info.contains("h264"), "no h264 stream: {info}");
    assert!(info.contains("aac"), "no aac stream: {info}");
    assert!(
        info.contains("320") && info.contains("180"),
        "wrong size: {info}"
    );

    // Decode every frame back out — a container can advertise streams it cannot
    // deliver, so listing them is not proof.
    let decoded = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(&path)
        .args(["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("-y")
        .arg("/dev/null")
        .status()
        .expect("spawn ffmpeg");
    assert!(decoded.success(), "ffmpeg could not decode the video track");

    let _ = std::fs::remove_file(&path);
}
