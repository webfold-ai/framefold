//! Render a short animated card to an .mp4 — no browser, no ffmpeg.
//!
//!     cargo run --release --example card -- /tmp/out.mp4
//!
//! Everything here is a pure function of time, so any frame can be rendered
//! independently and in any order.

use framefold::{Align, Canvas, Rect, Rgba, TextStyle};

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "framefold-card.mp4".to_string());

    let font_path = std::env::var("FRAMEFOLD_FONT")
        .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".to_string());
    let font = match std::fs::read(&font_path) {
        Ok(d) => ruler::ttf::Font::parse(d).expect("parse font"),
        Err(e) => {
            eprintln!("cannot read {font_path}: {e}\nset FRAMEFOLD_FONT to a .ttf");
            std::process::exit(1);
        }
    };

    let (w, h, fps, seconds) = (1280u32, 720u32, 30u32, 4u32);
    let total = fps * seconds;

    let t_render = std::time::Instant::now();
    let frames: Vec<Vec<u8>> = (0..total)
        .map(|i| {
            let t = i as f32 / (total - 1) as f32;
            let mut c = Canvas::filled(w, h, Rgba::rgb(11, 11, 15));

            // Two staggered reveals, each eased independently — the shape a
            // real composition's timeline produces.
            let stage = |start: f32, len: f32| -> f32 {
                (((t - start) / len).clamp(0.0, 1.0)).powf(0.4)
            };
            let card_t = stage(0.00, 0.45);
            let text_t = stage(0.20, 0.45);

            let card = Rect::new(
                140.0,
                130.0 + (1.0 - card_t) * 50.0,
                w as f32 - 280.0,
                h as f32 - 260.0,
            );
            c.fill_round_rect(card, 18.0, Rgba::rgb(22, 22, 30), card_t, None);
            c.stroke_rect(card, 1.0, Rgba::rgb(64, 64, 86), card_t * 0.9, None);

            // An accent bar that wipes across as the card settles.
            let bar = Rect::new(card.x + 40.0, card.y + 40.0, 220.0 * card_t, 4.0);
            c.fill_rect(bar, Rgba::rgb(120, 200, 255), card_t, Some(card));

            let title = TextStyle::new(&font, 56.0, Rgba::WHITE).align(Align::Center);
            let sub = TextStyle::new(&font, 22.0, Rgba::rgb(150, 150, 175))
                .align(Align::Center)
                .letter_spacing(1.5);

            framefold::draw_text(
                &mut c,
                &title,
                "framefold",
                Rect::new(card.x, card.y + 120.0, card.w, 80.0),
                Some(card),
                text_t,
            )
            .unwrap();
            framefold::draw_text(
                &mut c,
                &sub,
                "native compositing — no browser in the loop",
                Rect::new(card.x, card.y + 200.0, card.w, 50.0),
                Some(card),
                text_t,
            )
            .unwrap();

            c.into_rgba()
        })
        .collect();

    let render = t_render.elapsed();
    eprintln!(
        "render: {total} frames at {w}x{h} in {:.2}s ({:.1} fps)",
        render.as_secs_f64(),
        total as f64 / render.as_secs_f64()
    );

    let t_encode = std::time::Instant::now();
    let mp4 = transcode::mp4::encode_rgba_h264_qp(&frames, w, h, fps, transcode::h264::DEFAULT_QP)
        .expect("encode");
    let encode = t_encode.elapsed();

    std::fs::write(&out, &mp4).expect("write");
    eprintln!(
        "encode: {:.2}s · total {:.2}s · wrote {out} ({:.1} MB)",
        encode.as_secs_f64(),
        (render + encode).as_secs_f64(),
        mp4.len() as f64 / 1_048_576.0
    );
}
