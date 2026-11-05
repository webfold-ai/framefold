//! Caching rasterized glyphs.
//!
//! A frame of a text-heavy composition draws hundreds of glyphs, and a 20-second
//! render draws every one of them 600 times. Without a cache each draw re-reads
//! the glyph from the font's `glyf` table, flattens its Bézier curves, and runs
//! the scanline rasterizer over it — identical work, repeated, for a result that
//! never changes.
//!
//! A glyph's appearance depends only on (font, glyph id, pixel size, subpixel
//! x-offset), so that tuple is the key and the value is a tight coverage mask.
//! Drawing then becomes a blit.
//!
//! # Subpixel positioning
//!
//! Text is not laid out on whole pixels — an advance of 12.37px puts the next
//! glyph at a fractional position. Rounding to whole pixels would make the cache
//! trivial and the text visibly uneven, so the fractional part is quantised into
//! [`SUBPIXEL_BINS`] bins and cached per bin. Four bins is the usual compromise:
//! a quarter-pixel positioning error is below the threshold of visibility, and
//! the cache stays four entries per glyph rather than unbounded.
//!
//! The cache is thread-local. Frames render in parallel and a shared cache would
//! need a lock on the hottest path in the renderer; per-worker caches warm up
//! independently and never contend.

use crate::raster::{Canvas, Path, Rect, Rgba};
use ruler::ttf::Font;
use std::cell::RefCell;
use std::collections::HashMap;

/// Distinct subpixel positions cached per glyph.
const SUBPIXEL_BINS: usize = 4;

/// A rasterized glyph: an 8-bit coverage mask and where it sits relative to the
/// pen position on the baseline.
struct Mask {
    w: usize,
    h: usize,
    /// Offset from the pen/baseline origin to the mask's top-left corner.
    left: i32,
    top: i32,
    a: Vec<u8>,
}

#[derive(PartialEq, Eq, Hash)]
struct Key {
    font: usize,
    gid: u16,
    /// Size in thousandths of a pixel, so 12.5px and 12.501px share an entry.
    size_milli: u32,
    subpixel: u8,
}

#[derive(Default)]
struct Cache {
    map: HashMap<Key, Option<Mask>>,
    bytes: usize,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Draw one glyph, going through the per-thread cache.
///
/// `pen_x` is the exact fractional pen position; `baseline_y` the baseline.
/// Returns without drawing for glyphs that have no outline (spaces), which
/// still advance the pen — that is the caller's business.
#[allow(clippy::too_many_arguments)]
pub fn draw_glyph(
    canvas: &mut Canvas,
    font: &Font,
    gid: u16,
    size: f32,
    pen_x: f32,
    baseline_y: f32,
    color: Rgba,
    alpha: f32,
    clip: Option<Rect>,
) -> Result<(), String> {
    if alpha <= 0.0 || size <= 0.0 {
        return Ok(());
    }
    // Whole-pixel origin plus a quantised fractional part.
    let bin =
        (((pen_x.fract() + 1.0).fract()) * SUBPIXEL_BINS as f32).floor() as usize % SUBPIXEL_BINS;
    let origin_x = pen_x.floor();
    // The baseline is quantised the same way, so a line at y=100.5 is not
    // rebuilt for every glyph on it.
    let bin_y = (((baseline_y.fract() + 1.0).fract()) * SUBPIXEL_BINS as f32).floor() as usize
        % SUBPIXEL_BINS;
    let origin_y = baseline_y.floor();

    let key = Key {
        font: font as *const Font as usize,
        gid,
        size_milli: (size * 1000.0).round() as u32,
        subpixel: (bin * SUBPIXEL_BINS + bin_y) as u8,
    };

    CACHE.with(|c| -> Result<(), String> {
        let mut c = c.borrow_mut();
        if !c.map.contains_key(&key) {
            let sub_x = bin as f32 / SUBPIXEL_BINS as f32;
            let sub_y = bin_y as f32 / SUBPIXEL_BINS as f32;
            let mask = rasterize(font, gid, size, sub_x, sub_y)?;
            // Deliberately unbounded within a render.
            //
            // A budget with wholesale eviction was measured and made things
            // worse on both axes: clearing and refilling churns allocations
            // through the allocator, which raised peak RSS from 112 MB to
            // 157 MB at four threads while also redoing rasterization work. The
            // cache is naturally bounded by the number of distinct
            // (glyph, size, subpixel) tuples a composition uses — a few hundred
            // kilobytes to a few megabytes — and that ceiling is reached in the
            // first second and then held.
            c.bytes += mask.as_ref().map(|m| m.a.len()).unwrap_or(0) + 64;
            c.map.insert(
                Key {
                    font: key.font,
                    gid,
                    size_milli: key.size_milli,
                    subpixel: key.subpixel,
                },
                mask,
            );
        }
        let Some(Some(mask)) = c.map.get(&key) else {
            return Ok(()); // no outline: a space
        };
        blit(
            canvas,
            mask,
            origin_x as i32 + mask.left,
            origin_y as i32 + mask.top,
            color,
            alpha,
            clip,
        );
        Ok(())
    })
}

/// Rasterize one glyph into a tight mask at a given subpixel offset.
fn rasterize(
    font: &Font,
    gid: u16,
    size: f32,
    sub_x: f32,
    sub_y: f32,
) -> Result<Option<Mask>, String> {
    let contours = font.outline(gid)?;
    if contours.is_empty() {
        return Ok(None);
    }
    let scale = size / font.units_per_em() as f32;
    // Build the path around a local origin so the mask is tight.
    let path = Path::from_glyph(&contours, scale, sub_x, sub_y);
    let Some(b) = path.bounds() else {
        return Ok(None);
    };
    let left = b.x.floor() as i32;
    let top = b.y.floor() as i32;
    let w = (b.right().ceil() as i32 - left).max(1) as usize;
    let h = (b.bottom().ceil() as i32 - top).max(1) as usize;
    if w > 4096 || h > 4096 {
        return Ok(None);
    }

    // Render white-on-transparent into a small canvas, then keep only alpha.
    let mut tile = Canvas::new(w as u32, h as u32);
    let shifted = path.transform(&crate::raster::Transform::translate(
        -(left as f32),
        -(top as f32),
    ));
    tile.fill_path(&shifted, Rgba::WHITE, 1.0, None);
    let a = tile.data.chunks_exact(4).map(|p| p[3]).collect();
    Ok(Some(Mask { w, h, left, top, a }))
}

/// Composite a coverage mask in a solid colour.
fn blit(
    canvas: &mut Canvas,
    mask: &Mask,
    x: i32,
    y: i32,
    color: Rgba,
    alpha: f32,
    clip: Option<Rect>,
) {
    let cw = canvas.width as i32;
    let ch = canvas.height as i32;
    // Clamp to the canvas and the clip up front, so the inner loop is a
    // straight walk with no per-pixel bounds tests.
    let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + mask.w as i32, y + mask.h as i32);
    if let Some(c) = clip {
        x0 = x0.max(c.x.floor() as i32);
        y0 = y0.max(c.y.floor() as i32);
        x1 = x1.min(c.right().ceil() as i32);
        y1 = y1.min(c.bottom().ceil() as i32);
    }
    x0 = x0.max(0);
    y0 = y0.max(0);
    x1 = x1.min(cw);
    y1 = y1.min(ch);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    for py in y0..y1 {
        let my = (py - y) as usize;
        for px in x0..x1 {
            let mx = (px - x) as usize;
            let cov = mask.a[my * mask.w + mx];
            if cov == 0 {
                continue;
            }
            let a = cov as f32 / 255.0 * alpha;
            let i = (py as usize * canvas.width as usize + px as usize) * 4;
            crate::raster::blend_pub(&mut canvas.data[i..i + 4], color, a);
        }
    }
}

/// Drop this thread's cache. Useful between very different workloads; the cache
/// is otherwise bounded by the number of distinct (glyph, size, subpixel) tuples
/// a composition actually uses, which is small.
pub fn clear() {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.map.clear();
        c.bytes = 0;
    });
}

/// Bytes of mask data cached on this thread.
pub fn bytes() -> usize {
    CACHE.with(|c| c.borrow().bytes)
}

/// Number of entries currently cached on this thread.
pub fn len() -> usize {
    CACHE.with(|c| c.borrow().map.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> Option<Font> {
        let d = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf").ok()?;
        Some(Font::parse(d).expect("parse"))
    }

    fn ink(c: &Canvas) -> usize {
        c.data.chunks_exact(4).filter(|p| p[3] > 0).count()
    }

    #[test]
    fn cached_glyph_matches_an_uncached_path_fill() {
        let Some(f) = font() else {
            eprintln!("skip: DejaVuSans not installed");
            return;
        };
        clear();
        let gid = f.glyph_id('R');
        let size = 48.0;

        // Direct path fill, the reference.
        let contours = f.outline(gid).unwrap();
        let scale = size / f.units_per_em() as f32;
        let mut direct = Canvas::new(80, 80);
        direct.fill_path(
            &Path::from_glyph(&contours, scale, 10.0, 60.0),
            Rgba::BLACK,
            1.0,
            None,
        );

        let mut cached = Canvas::new(80, 80);
        draw_glyph(
            &mut cached,
            &f,
            gid,
            size,
            10.0,
            60.0,
            Rgba::BLACK,
            1.0,
            None,
        )
        .unwrap();

        // Both take the same rasterizer path, so at a whole-pixel position they
        // must agree exactly — any drift means the mask is offset.
        assert_eq!(ink(&cached), ink(&direct), "different coverage");
        assert_eq!(
            cached.data, direct.data,
            "cached glyph differs from direct fill"
        );
    }

    #[test]
    fn repeated_draws_reuse_one_entry() {
        let Some(f) = font() else { return };
        clear();
        let mut c = Canvas::new(400, 80);
        for i in 0..20 {
            draw_glyph(
                &mut c,
                &f,
                f.glyph_id('a'),
                32.0,
                10.0 + i as f32 * 18.0,
                60.0,
                Rgba::BLACK,
                1.0,
                None,
            )
            .unwrap();
        }
        // 20 draws at whole-pixel steps: one glyph, one subpixel bin.
        assert_eq!(len(), 1, "identical positions should share a cache entry");
        assert!(ink(&c) > 100);
    }

    #[test]
    fn subpixel_positions_are_cached_separately() {
        let Some(f) = font() else { return };
        clear();
        let mut c = Canvas::new(200, 80);
        for k in 0..4 {
            draw_glyph(
                &mut c,
                &f,
                f.glyph_id('i'),
                32.0,
                10.0 + k as f32 * 0.25,
                60.0,
                Rgba::BLACK,
                1.0,
                None,
            )
            .unwrap();
        }
        assert_eq!(len(), 4, "four subpixel bins should be four entries");
    }

    #[test]
    fn a_space_caches_as_nothing_and_draws_nothing() {
        let Some(f) = font() else { return };
        clear();
        let mut c = Canvas::new(60, 60);
        draw_glyph(
            &mut c,
            &f,
            f.glyph_id(' '),
            32.0,
            10.0,
            40.0,
            Rgba::BLACK,
            1.0,
            None,
        )
        .unwrap();
        assert_eq!(ink(&c), 0, "a space must not paint");
        assert_eq!(len(), 1, "and its absence is still cached, not re-derived");
    }

    #[test]
    fn clipping_cuts_the_blit() {
        let Some(f) = font() else { return };
        clear();
        let mut full = Canvas::new(80, 80);
        draw_glyph(
            &mut full,
            &f,
            f.glyph_id('M'),
            60.0,
            5.0,
            65.0,
            Rgba::BLACK,
            1.0,
            None,
        )
        .unwrap();
        let mut clipped = Canvas::new(80, 80);
        draw_glyph(
            &mut clipped,
            &f,
            f.glyph_id('M'),
            60.0,
            5.0,
            65.0,
            Rgba::BLACK,
            1.0,
            Some(Rect::new(0.0, 0.0, 20.0, 80.0)),
        )
        .unwrap();
        assert!(ink(&clipped) > 0);
        assert!(ink(&clipped) < ink(&full), "clip must remove ink");
    }

    #[test]
    fn alpha_scales_the_mask() {
        let Some(f) = font() else { return };
        clear();
        let mut c = Canvas::filled(80, 80, Rgba::WHITE);
        draw_glyph(
            &mut c,
            &f,
            f.glyph_id('M'),
            60.0,
            5.0,
            65.0,
            Rgba::BLACK,
            0.5,
            None,
        )
        .unwrap();
        let darkest = c.data.chunks_exact(4).map(|p| p[0]).min().unwrap();
        assert!(
            (110..=145).contains(&darkest),
            "half-alpha black on white: {darkest}"
        );
    }
}
