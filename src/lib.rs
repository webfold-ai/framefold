//! framefold — native compositing for video frames, with no browser in the loop.
//!
//! # Why this exists
//!
//! Rendering an HTML-style composition to video currently means driving a
//! headless Chrome: seek a timeline, screenshot, repeat, pipe to ffmpeg. The
//! workspace already owned both ends of that pipeline — `ruler` measures text
//! and yields glyph outlines, `transcode` encodes RGBA frames to H.264/AAC —
//! and nothing in between turned geometry into pixels.
//!
//! framefold is that middle. Its output is exactly what
//! `transcode::mp4::encode_rgba_h264_aac` consumes, so a composition can become
//! an `.mp4` without a browser process ever starting.
//!
//! ```no_run
//! use framefold::{Canvas, Rect, Rgba};
//!
//! let mut frame = Canvas::filled(1920, 1080, Rgba::rgb(10, 10, 12));
//! frame.fill_round_rect(Rect::new(80.0, 80.0, 600.0, 320.0), 16.0, Rgba::WHITE, 1.0, None);
//! let rgba: Vec<u8> = frame.into_rgba(); // ready for transcode
//! ```
//!
//! # What is here, and what is not
//!
//! Present: an anti-aliased scanline rasterizer (nonzero winding, exact
//! horizontal coverage), affine transforms, rectangular clipping, rounded
//! rectangles, source-over alpha, and text drawn from real TrueType outlines
//! via `ruler`.
//!
//! Implemented: a tolerant HTML parser for the subset compositions use, CSS parsing
//! and cascade for the subset real stylesheets specify, block and flex layout,
//! text drawn from TrueType outlines via `ruler` (WOFF2 via `woff2`), and timeline
//! extraction by running the composition's script under the `js` feature.
//!
//! Not implemented: grid, floats, tables, images, gradients, blurs, and general path
//! stroking (only rectangle borders are stroked). See `layout.rs` for the layout gaps.

pub mod css;
pub mod fonts;
pub mod glyphcache;
pub mod html;
pub mod layout;
pub mod paint;
pub mod raster;
pub mod render;
pub mod text;
pub mod timeline;
pub mod woff2;

pub use raster::{Canvas, Path, Rect, Rgba, Transform};
pub use render::{default_threads, render_frames, render_frames_parallel};
pub use text::{draw_line, draw_text, Align, TextStyle};
