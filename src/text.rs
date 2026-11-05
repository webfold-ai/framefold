//! Drawing text — the join between `ruler` (where glyphs go) and
//! [`crate::raster`] (what pixels they become).
//!
//! ruler owns measurement and line breaking; this module owns placement and
//! painting. Nothing here re-measures: advances come from the font's `hmtx`
//! through `ruler`, and line breaks come from `Ruler::lines`, so laying the
//! same text out twice is guaranteed to agree with itself.

use crate::raster::{Canvas, Path, Rect, Rgba};
use ruler::ttf::Font;
use ruler::Ruler;

/// Where a run of text sits horizontally within its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A text style: which font, how big, what colour, and how it is aligned.
pub struct TextStyle<'a> {
    pub font: &'a Font,
    /// Em size in pixels.
    pub size: f32,
    pub color: Rgba,
    pub align: Align,
    /// Baseline-to-baseline distance. Defaults to 1.2x the em size, the usual
    /// typographic default, when `None`.
    pub line_height: Option<f32>,
    /// Extra advance inserted after every glyph — CSS `letter-spacing`.
    pub letter_spacing: f32,
}

impl<'a> TextStyle<'a> {
    pub fn new(font: &'a Font, size: f32, color: Rgba) -> TextStyle<'a> {
        TextStyle {
            font,
            size,
            color,
            align: Align::Left,
            line_height: None,
            letter_spacing: 0.0,
        }
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn line_height(mut self, lh: f32) -> Self {
        self.line_height = Some(lh);
        self
    }

    pub fn letter_spacing(mut self, ls: f32) -> Self {
        self.letter_spacing = ls;
        self
    }

    pub fn resolved_line_height(&self) -> f32 {
        self.line_height.unwrap_or(self.size * 1.2)
    }
}

/// One laid-out line: its text and how wide it came out.
#[derive(Clone, Debug)]
pub struct LaidLine {
    pub text: String,
    pub width: f32,
}

/// Break `text` into lines that fit `max_width`, honouring existing newlines.
///
/// `max_width` of `None` means "never wrap" — only explicit newlines break.
pub fn layout(
    style: &TextStyle,
    text: &str,
    max_width: Option<f32>,
) -> Result<Vec<LaidLine>, String> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(LaidLine {
                text: String::new(),
                width: 0.0,
            });
            continue;
        }
        let chars: Vec<char> = paragraph.chars().collect();
        let mut boxes = style.font.boxes(paragraph, style.size)?;
        if style.letter_spacing != 0.0 {
            for b in &mut boxes {
                b.advance += style.letter_spacing;
            }
        }

        match max_width {
            None => out.push(LaidLine {
                text: paragraph.to_string(),
                width: boxes.iter().map(|b| b.advance).sum(),
            }),
            Some(w) => {
                let ruler = Ruler::prepare(&boxes);
                for line in ruler.lines(w) {
                    let lo = line.start.min(chars.len());
                    let hi = line.end.min(chars.len());
                    // Trailing whitespace at a break is not visible ink. Counting
                    // it would push centred text left and make a right-aligned
                    // line hang past its box, so drop it from both the string and
                    // the measured width. Counted in chars, not bytes — ruler
                    // indexes by character and a byte count would desynchronise
                    // the two on any multi-byte text.
                    let mut end = hi;
                    while end > lo && chars[end - 1].is_whitespace() {
                        end -= 1;
                    }
                    let width: f32 = boxes[lo..end].iter().map(|b| b.advance).sum();
                    out.push(LaidLine {
                        text: chars[lo..end].iter().collect(),
                        width: width.max(0.0),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Draw a single line of text with its baseline at `baseline_y`.
///
/// Returns the x advance consumed. Glyphs with no outline (spaces) still
/// advance, which is the whole reason advance and outline are separate queries.
pub fn draw_line(
    canvas: &mut Canvas,
    style: &TextStyle,
    text: &str,
    x: f32,
    baseline_y: f32,
    clip: Option<Rect>,
    alpha: f32,
) -> Result<f32, String> {
    let scale = style.size / style.font.units_per_em() as f32;
    let mut pen = x;
    for ch in text.chars() {
        let gid = style.font.glyph_id(ch);
        let contours = style.font.outline(gid)?;
        if !contours.is_empty() {
            let path = Path::from_glyph(&contours, scale, pen, baseline_y);
            canvas.fill_path(&path, style.color, alpha, clip);
        }
        pen += style.font.advance(gid, style.size)? + style.letter_spacing;
    }
    Ok(pen - x)
}

/// Draw wrapped, aligned text inside `area`, starting from its top edge.
///
/// Returns the total height consumed, so a caller stacking blocks can advance
/// without re-running layout.
pub fn draw_text(
    canvas: &mut Canvas,
    style: &TextStyle,
    text: &str,
    area: Rect,
    clip: Option<Rect>,
    alpha: f32,
) -> Result<f32, String> {
    let lines = layout(style, text, Some(area.w))?;
    let lh = style.resolved_line_height();
    // First baseline sits one ascent below the top. Approximating the ascent as
    // 0.8em avoids a dependency on `OS/2`, and is close enough that a caller
    // positioning by box rather than baseline sees text where they expect.
    let mut baseline = area.y + style.size * 0.8;
    for line in &lines {
        let x = match style.align {
            Align::Left => area.x,
            Align::Center => area.x + (area.w - line.width) / 2.0,
            Align::Right => area.right() - line.width,
        };
        draw_line(canvas, style, &line.text, x, baseline, clip, alpha)?;
        baseline += lh;
    }
    Ok(lines.len() as f32 * lh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> Option<Font> {
        let data = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf").ok()?;
        Some(Font::parse(data).expect("parse DejaVuSans"))
    }

    fn ink(c: &Canvas) -> usize {
        c.data.chunks_exact(4).filter(|p| p[3] > 0).count()
    }

    #[test]
    fn draws_visible_glyphs() {
        let Some(f) = font() else {
            eprintln!("skip: DejaVuSans not installed");
            return;
        };
        let style = TextStyle::new(&f, 48.0, Rgba::WHITE);
        let mut c = Canvas::new(200, 80);
        draw_line(&mut c, &style, "Hi", 10.0, 60.0, None, 1.0).unwrap();
        assert!(ink(&c) > 100, "text should put real ink on the canvas");
    }

    #[test]
    fn a_space_advances_but_draws_nothing() {
        let Some(f) = font() else { return };
        let style = TextStyle::new(&f, 32.0, Rgba::WHITE);
        let mut c = Canvas::new(120, 60);
        let adv = draw_line(&mut c, &style, "   ", 5.0, 40.0, None, 1.0).unwrap();
        assert!(adv > 0.0, "spaces must still advance the pen");
        assert_eq!(ink(&c), 0, "spaces must not paint");
    }

    #[test]
    fn wider_text_consumes_more_advance() {
        let Some(f) = font() else { return };
        let style = TextStyle::new(&f, 24.0, Rgba::WHITE);
        let mut c = Canvas::new(400, 60);
        let short = draw_line(&mut c, &style, "i", 0.0, 40.0, None, 1.0).unwrap();
        let long = draw_line(&mut c, &style, "William", 0.0, 40.0, None, 1.0).unwrap();
        assert!(long > short * 3.0);
    }

    #[test]
    fn letter_spacing_widens_the_run() {
        let Some(f) = font() else { return };
        let mut c = Canvas::new(400, 60);
        let plain = TextStyle::new(&f, 24.0, Rgba::WHITE);
        let spaced = TextStyle::new(&f, 24.0, Rgba::WHITE).letter_spacing(5.0);
        let a = draw_line(&mut c, &plain, "abcd", 0.0, 40.0, None, 1.0).unwrap();
        let b = draw_line(&mut c, &spaced, "abcd", 0.0, 40.0, None, 1.0).unwrap();
        assert!((b - a - 20.0).abs() < 0.01, "4 glyphs x 5px spacing");
    }

    #[test]
    fn text_wraps_at_the_given_width() {
        let Some(f) = font() else { return };
        let style = TextStyle::new(&f, 16.0, Rgba::WHITE);
        let one = layout(&style, "hello world hello world", Some(10_000.0)).unwrap();
        assert_eq!(one.len(), 1, "a huge width must not wrap");
        let many = layout(&style, "hello world hello world", Some(80.0)).unwrap();
        assert!(many.len() > 1, "a narrow width must wrap");
        for l in &many {
            assert!(l.width <= 80.0 + 0.5, "line overflows: {l:?}");
        }
    }

    #[test]
    fn explicit_newlines_always_break() {
        let Some(f) = font() else { return };
        let style = TextStyle::new(&f, 16.0, Rgba::WHITE);
        let lines = layout(&style, "a\nb\nc", None).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].text, "b");
    }

    #[test]
    fn alignment_shifts_the_ink() {
        let Some(f) = font() else { return };
        let area = Rect::new(0.0, 0.0, 300.0, 50.0);

        let centroid = |align: Align| {
            let style = TextStyle::new(&f, 28.0, Rgba::WHITE).align(align);
            let mut c = Canvas::new(300, 50);
            draw_text(&mut c, &style, "hi", area, None, 1.0).unwrap();
            let (mut sum, mut n) = (0usize, 0usize);
            for y in 0..c.height {
                for x in 0..c.width {
                    if c.pixel(x, y).a > 0 {
                        sum += x as usize;
                        n += 1;
                    }
                }
            }
            assert!(n > 0, "no ink drawn for {align:?}");
            sum as f32 / n as f32
        };

        let (l, c, r) = (
            centroid(Align::Left),
            centroid(Align::Center),
            centroid(Align::Right),
        );
        assert!(l < c && c < r, "left {l} center {c} right {r}");
    }

    #[test]
    fn clip_cuts_text() {
        let Some(f) = font() else { return };
        let style = TextStyle::new(&f, 40.0, Rgba::WHITE);
        let mut full = Canvas::new(200, 60);
        draw_line(&mut full, &style, "MMMM", 0.0, 45.0, None, 1.0).unwrap();
        let mut clipped = Canvas::new(200, 60);
        draw_line(
            &mut clipped,
            &style,
            "MMMM",
            0.0,
            45.0,
            Some(Rect::new(0.0, 0.0, 50.0, 60.0)),
            1.0,
        )
        .unwrap();
        assert!(ink(&clipped) > 0);
        assert!(ink(&clipped) < ink(&full), "clip must remove ink");
    }

    #[test]
    fn alpha_fades_text() {
        let Some(f) = font() else { return };
        let style = TextStyle::new(&f, 40.0, Rgba::WHITE);
        let mut c = Canvas::filled(100, 60, Rgba::BLACK);
        draw_line(&mut c, &style, "M", 10.0, 45.0, None, 0.5).unwrap();
        let brightest = (0..c.height)
            .flat_map(|y| (0..c.width).map(move |x| (x, y)))
            .map(|(x, y)| c.pixel(x, y).r)
            .max()
            .unwrap();
        assert!(
            (100..=160).contains(&brightest),
            "half-alpha white on black should be mid grey, got {brightest}"
        );
    }
}

/// Break a run into lines that fit `max_width`, returning each line and its width.
///
/// Shared by layout (which needs the height a wrapped run occupies) and paint
/// (which needs the same lines in the same places). Measuring one way and
/// drawing another is how text ends up overflowing the box that was sized for
/// it, so both go through here.
///
/// `max_width` of `None` means never wrap — `white-space: nowrap`.
pub fn break_lines(
    font: &Font,
    size: f32,
    letter_spacing: f32,
    text: &str,
    max_width: Option<f32>,
) -> Result<Vec<(String, f32)>, String> {
    let chars: Vec<char> = text.chars().collect();
    let width_of = |s: &str| -> f32 {
        s.chars()
            .map(|c| font.advance(font.glyph_id(c), size).unwrap_or(0.0) + letter_spacing)
            .sum()
    };

    let Some(max) = max_width.filter(|w| *w > 0.0) else {
        return Ok(vec![(text.to_string(), width_of(text))]);
    };

    let mut boxes = font.boxes(text, size)?;
    if letter_spacing != 0.0 {
        for b in &mut boxes {
            b.advance += letter_spacing;
        }
    }
    let ruler = Ruler::prepare(&boxes);
    let mut out = Vec::new();
    for line in ruler.lines(max) {
        let lo = line.start.min(chars.len());
        let hi = line.end.min(chars.len());
        // Trailing whitespace at a break is not visible ink; counting it would
        // push centred text left and make right-aligned text hang.
        let mut end = hi;
        while end > lo && chars[end - 1].is_whitespace() {
            end -= 1;
        }
        let s: String = chars[lo..end].iter().collect();
        let w = width_of(&s);
        out.push((s, w));
    }
    if out.is_empty() {
        out.push((String::new(), 0.0));
    }
    Ok(out)
}
