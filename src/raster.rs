//! Anti-aliased scanline rasterizer — geometry in, RGBA8 pixels out.
//!
//! This is the piece that did not exist anywhere in the workspace. `ruler`
//! measures text and hands back glyph outlines; `transcode` encodes finished
//! RGBA frames; between them sat a gap that was being filled by a headless
//! Chrome process. This module is that gap.
//!
//! # How the anti-aliasing works
//!
//! Each pixel row is sampled at [`SUB_SAMPLES`] evenly spaced sub-scanlines.
//! For every sub-scanline the rasterizer finds where the path's edges cross it,
//! sorts those crossings, and walks them applying the **nonzero winding rule**
//! to decide which stretches are inside the shape. Each inside stretch is added
//! to a float coverage row with *analytic* horizontal coverage: a span from
//! x=2.25 to x=5.5 contributes 0.75 to pixel 2, a full 1.0 to pixels 3 and 4,
//! and 0.5 to pixel 5.
//!
//! So vertical accuracy is sampled (4 steps) and horizontal accuracy is exact.
//! That asymmetry is deliberate: horizontal coverage is cheap to compute in
//! closed form, while exact vertical coverage requires trapezoid accumulation
//! that is far easier to get subtly wrong. Text quality is dominated by stem
//! edges, which are mostly vertical, so the exact axis is the one that matters.
//!
//! Work is bounded by the path's bounding box intersected with the clip and the
//! canvas, so filling a small glyph on a 1080p canvas touches only the glyph's
//! own rows — not the whole frame.

/// Sub-scanlines sampled per pixel row. Four is the usual quality/speed knob:
/// it resolves a near-horizontal edge into 5 distinct levels, which is enough
/// that banding is not visible at text sizes.
const SUB_SAMPLES: usize = 4;

/// Straight (non-premultiplied) 8-bit RGBA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);
    pub const BLACK: Rgba = Rgba::new(0, 0, 0, 255);
    pub const WHITE: Rgba = Rgba::new(255, 255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba::new(r, g, b, 255)
    }

    /// Parse `#rgb`, `#rrggbb`, or `#rrggbbaa`. Returns `None` on anything else
    /// rather than guessing — a silently mis-parsed colour is worse than a
    /// caller-visible failure.
    pub fn from_hex(s: &str) -> Option<Rgba> {
        let h = s.strip_prefix('#')?;
        let b = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
        match h.len() {
            3 => {
                let d = |i: usize| {
                    let v = u8::from_str_radix(h.get(i..i + 1)?, 16).ok()?;
                    Some(v * 17) // 0xF -> 0xFF
                };
                Some(Rgba::new(d(0)?, d(1)?, d(2)?, 255))
            }
            6 => Some(Rgba::new(b(0)?, b(2)?, b(4)?, 255)),
            8 => Some(Rgba::new(b(0)?, b(2)?, b(4)?, b(6)?)),
            _ => None,
        }
    }
}

/// An axis-aligned rectangle in pixel space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
    /// Overlap of two rectangles; empty (zero-sized) when they do not meet.
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect::new(x, y, (r - x).max(0.0), (b - y).max(0.0))
    }
}

/// A 2x3 affine transform, laid out as it applies:
/// `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub const fn translate(tx: f32, ty: f32) -> Transform {
        Transform {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    pub const fn scale(sx: f32, sy: f32) -> Transform {
        Transform {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: 0.0,
            f: 0.0,
        }
    }

    pub fn rotate(radians: f32) -> Transform {
        let (s, c) = radians.sin_cos();
        Transform {
            a: c,
            b: s,
            c: -s,
            d: c,
            e: 0.0,
            f: 0.0,
        }
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// `self` then `next` — the transform that applies `self` first.
    pub fn then(&self, next: &Transform) -> Transform {
        Transform {
            a: self.a * next.a + self.b * next.c,
            b: self.a * next.b + self.b * next.d,
            c: self.c * next.a + self.d * next.c,
            d: self.c * next.b + self.d * next.d,
            e: self.e * next.a + self.f * next.c + next.e,
            f: self.e * next.b + self.f * next.d + next.f,
        }
    }
}

/// A shape as one or more closed polylines. Curves are flattened on the way in,
/// so the rasterizer only ever deals with line segments.
#[derive(Clone, Debug, Default)]
pub struct Path {
    contours: Vec<Vec<(f32, f32)>>,
    current: Vec<(f32, f32)>,
}

impl Path {
    pub fn new() -> Path {
        Path::default()
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.close();
        self.current.push((x, y));
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        self.current.push((x, y));
    }

    /// Append a quadratic Bézier, subdividing by how far the curve deviates
    /// from the straight line between its ends. A nearly-flat curve costs one
    /// segment; a tight one costs as many as it needs.
    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (x0, y0) = *self.current.last().unwrap_or(&(cx, cy));
        let dx = x0 - 2.0 * cx + x;
        let dy = y0 - 2.0 * cy + y;
        let dev_sq = dx * dx + dy * dy;
        let n = if dev_sq < 0.1 {
            1
        } else {
            ((dev_sq.powf(0.25) * 2.0).ceil() as usize).clamp(1, 64)
        };
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            self.current.push((
                mt * mt * x0 + 2.0 * mt * t * cx + t * t * x,
                mt * mt * y0 + 2.0 * mt * t * cy + t * t * y,
            ));
        }
    }

    /// Finish the contour in progress. Contours are implicitly closed — the
    /// rasterizer joins last point to first — so an explicit closing point is
    /// never required.
    pub fn close(&mut self) {
        if self.current.len() >= 2 {
            let done = std::mem::take(&mut self.current);
            self.contours.push(done);
        } else {
            self.current.clear();
        }
    }

    fn finished(&self) -> Vec<Vec<(f32, f32)>> {
        let mut all = self.contours.clone();
        if self.current.len() >= 2 {
            all.push(self.current.clone());
        }
        all
    }

    pub fn is_empty(&self) -> bool {
        self.contours.is_empty() && self.current.len() < 2
    }

    /// Axis-aligned bounds, or `None` for an empty path.
    pub fn bounds(&self) -> Option<Rect> {
        let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
        let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        let mut any = false;
        for c in self.finished() {
            for (x, y) in c {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        any.then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    pub fn transform(&self, t: &Transform) -> Path {
        let map = |c: &Vec<(f32, f32)>| c.iter().map(|&(x, y)| t.apply(x, y)).collect();
        Path {
            contours: self.finished().iter().map(map).collect(),
            current: Vec::new(),
        }
    }

    pub fn rect(r: Rect) -> Path {
        let mut p = Path::new();
        p.move_to(r.x, r.y);
        p.line_to(r.right(), r.y);
        p.line_to(r.right(), r.bottom());
        p.line_to(r.x, r.bottom());
        p.close();
        p
    }

    /// A rectangle with circular corners, as used by every card and button in a
    /// composition. `radius` is clamped to half the shorter side, so an
    /// over-large radius degrades to a stadium rather than folding inside out.
    pub fn round_rect(r: Rect, radius: f32) -> Path {
        let rad = radius.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
        if rad <= 0.0 {
            return Path::rect(r);
        }
        // 4/3*(sqrt(2)-1) is the circle approximation constant for cubics; for
        // the quadratics used here the corner is drawn as two quads via the
        // corner point, which is accurate to well under a pixel at these radii.
        let mut p = Path::new();
        let (l, t, rt, b) = (r.x, r.y, r.right(), r.bottom());
        p.move_to(l + rad, t);
        p.line_to(rt - rad, t);
        p.quad_to(rt, t, rt, t + rad);
        p.line_to(rt, b - rad);
        p.quad_to(rt, b, rt - rad, b);
        p.line_to(l + rad, b);
        p.quad_to(l, b, l, b - rad);
        p.line_to(l, t + rad);
        p.quad_to(l, t, l + rad, t);
        p.close();
        p
    }

    /// Build a path from `ruler`'s glyph outlines.
    ///
    /// Font outlines are y-up with the origin on the baseline; a canvas is
    /// y-down with the origin top-left. `scale` converts font units to pixels
    /// and the y axis is flipped here, so callers pass a baseline position in
    /// ordinary screen coordinates and get the right result.
    pub fn from_glyph(
        contours: &[ruler::ttf::Contour],
        scale: f32,
        origin_x: f32,
        baseline_y: f32,
    ) -> Path {
        let mut p = Path::new();
        for c in contours {
            if c.len() < 2 {
                continue;
            }
            let px = |pt: &ruler::ttf::Point| (origin_x + pt.x * scale, baseline_y - pt.y * scale);

            // Start at an on-curve point so the walk below always has a
            // current position to draw from.
            let start = c.iter().position(|p| p.on_curve).unwrap_or(0);
            let (sx, sy) = px(&c[start]);
            p.move_to(sx, sy);

            let n = c.len();
            let mut i = 1;
            while i <= n {
                let pt = &c[(start + i) % n];
                if pt.on_curve {
                    let (x, y) = px(pt);
                    p.line_to(x, y);
                    i += 1;
                } else {
                    // ruler guarantees an on-curve point follows any off-curve
                    // one (it materialises the implied midpoints), so this
                    // lookahead is always a real endpoint.
                    let end = &c[(start + i + 1) % n];
                    let (cx, cy) = px(pt);
                    let (ex, ey) = px(end);
                    p.quad_to(cx, cy, ex, ey);
                    i += 2;
                }
            }
            p.close();
        }
        p
    }
}

/// An RGBA8 image that shapes can be drawn into.
#[derive(Clone)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// Row-major, 4 bytes per pixel, straight (non-premultiplied) alpha.
    pub data: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Canvas {
        Canvas {
            width,
            height,
            data: vec![0; width as usize * height as usize * 4],
        }
    }

    /// A canvas pre-filled with an opaque colour.
    pub fn filled(width: u32, height: u32, color: Rgba) -> Canvas {
        let mut c = Canvas::new(width, height);
        c.clear(color);
        c
    }

    pub fn clear(&mut self, color: Rgba) {
        for px in self.data.chunks_exact_mut(4) {
            px[0] = color.r;
            px[1] = color.g;
            px[2] = color.b;
            px[3] = color.a;
        }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    pub fn pixel(&self, x: u32, y: u32) -> Rgba {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        Rgba::new(self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3])
    }

    /// Fill a path with a solid colour, optionally clipped.
    ///
    /// `alpha` scales the colour's own alpha, which is how a composition's
    /// `opacity` is applied without having to rewrite the colour.
    pub fn fill_path(&mut self, path: &Path, color: Rgba, alpha: f32, clip: Option<Rect>) {
        if alpha <= 0.0 || color.a == 0 {
            return;
        }
        let Some(bounds) = path.bounds() else { return };
        let region = match clip {
            Some(c) => bounds.intersect(&c).intersect(&self.bounds()),
            None => bounds.intersect(&self.bounds()),
        };
        if region.is_empty() {
            return;
        }

        // Rows and columns actually touched — everything else is skipped, which
        // is what keeps a 20px glyph cheap on a 1080p canvas.
        let y0 = region.y.floor().max(0.0) as usize;
        let y1 = (region.bottom().ceil() as usize).min(self.height as usize);
        let x0 = region.x.floor().max(0.0) as usize;
        let x1 = (region.right().ceil() as usize).min(self.width as usize);
        if y0 >= y1 || x0 >= x1 {
            return;
        }

        // Edges as (x0, y0, x1, y1). Horizontal edges never produce a crossing,
        // so they are dropped here rather than special-cased in the inner loop.
        let mut edges: Vec<(f32, f32, f32, f32)> = Vec::new();
        for c in path.finished() {
            for i in 0..c.len() {
                let (ax, ay) = c[i];
                let (bx, by) = c[(i + 1) % c.len()];
                if ay != by {
                    edges.push((ax, ay, bx, by));
                }
            }
        }
        if edges.is_empty() {
            return;
        }

        let width = x1 - x0;
        let mut coverage = vec![0f32; width];
        let mut crossings: Vec<(f32, i32)> = Vec::new();
        let weight = 1.0 / SUB_SAMPLES as f32;

        for y in y0..y1 {
            coverage.iter_mut().for_each(|c| *c = 0.0);

            for s in 0..SUB_SAMPLES {
                let sy = y as f32 + (s as f32 + 0.5) * weight;
                crossings.clear();
                for &(ax, ay, bx, by) in &edges {
                    // Half-open in y: a vertex shared by two edges is counted
                    // once, not twice or zero times.
                    let hits = (ay <= sy && by > sy) || (by <= sy && ay > sy);
                    if !hits {
                        continue;
                    }
                    let t = (sy - ay) / (by - ay);
                    crossings.push((ax + t * (bx - ax), if by > ay { 1 } else { -1 }));
                }
                if crossings.len() < 2 {
                    continue;
                }
                crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

                // Nonzero winding: a run is inside while the accumulated
                // direction count is non-zero.
                let mut winding = 0i32;
                let mut span_start = 0f32;
                for &(x, dir) in &crossings {
                    if winding == 0 {
                        span_start = x;
                    }
                    winding += dir;
                    if winding == 0 {
                        add_span(&mut coverage, span_start, x, weight, x0, x1);
                    }
                }
            }

            // Composite this row.
            //
            // The opaque case is not a micro-optimisation here: a composition
            // fills a full-frame background every frame, and routing two
            // million already-opaque pixels through a float source-over blend
            // dominated the entire renderer. When coverage, element alpha and
            // colour alpha are all saturated the result is exactly the source,
            // so the blend is a four-byte store.
            let row = y * self.width as usize;
            let opaque_src = alpha >= 1.0 && color.a == 255;
            let src = [color.r, color.g, color.b, 255];
            for (i, &cov) in coverage.iter().enumerate() {
                if cov <= 0.0 {
                    continue;
                }
                let px = (row + x0 + i) * 4;
                if opaque_src && cov >= 0.999 {
                    self.data[px..px + 4].copy_from_slice(&src);
                    continue;
                }
                let a = (cov.min(1.0)) * alpha * (color.a as f32 / 255.0);
                if a <= 0.0 {
                    continue;
                }
                blend(&mut self.data[px..px + 4], color, a);
            }
        }
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Rgba, alpha: f32, clip: Option<Rect>) {
        // Axis-aligned opaque rectangles are the single most common fill in a
        // composition — every scene paints one the size of the frame — and they
        // need none of the scanline machinery. Interior rows are a straight
        // memcpy of a repeated pixel; only the fractional edges need coverage,
        // which the general path still handles.
        if alpha >= 1.0 && color.a == 255 {
            let r = match clip {
                Some(c) => rect.intersect(&c).intersect(&self.bounds()),
                None => rect.intersect(&self.bounds()),
            };
            // Only take the fast path when the rect lands on whole pixels;
            // otherwise the edges need real anti-aliasing.
            let whole = |v: f32| (v - v.round()).abs() < 0.001;
            if !r.is_empty() && whole(r.x) && whole(r.y) && whole(r.w) && whole(r.h) {
                let x0 = r.x.round() as usize;
                let y0 = r.y.round() as usize;
                let x1 = (r.right().round() as usize).min(self.width as usize);
                let y1 = (r.bottom().round() as usize).min(self.height as usize);
                if x0 < x1 && y0 < y1 {
                    let w = self.width as usize;
                    let row: Vec<u8> = [color.r, color.g, color.b, 255]
                        .iter()
                        .cycle()
                        .take((x1 - x0) * 4)
                        .copied()
                        .collect();
                    for y in y0..y1 {
                        let i = (y * w + x0) * 4;
                        self.data[i..i + row.len()].copy_from_slice(&row);
                    }
                    return;
                }
            }
        }
        self.fill_path(&Path::rect(rect), color, alpha, clip);
    }

    pub fn fill_round_rect(
        &mut self,
        rect: Rect,
        radius: f32,
        color: Rgba,
        alpha: f32,
        clip: Option<Rect>,
    ) {
        self.fill_path(&Path::round_rect(rect, radius), color, alpha, clip);
    }

    /// Stroke a rectangle's outline as four filled edge rectangles. Adequate
    /// for the hairline borders compositions use; a general path stroker
    /// (joins, caps, curves) is deliberately not attempted here.
    pub fn stroke_rect(
        &mut self,
        rect: Rect,
        width: f32,
        color: Rgba,
        alpha: f32,
        clip: Option<Rect>,
    ) {
        if width <= 0.0 || rect.is_empty() {
            return;
        }
        let w = width.min(rect.w).min(rect.h);
        let sides = [
            Rect::new(rect.x, rect.y, rect.w, w),                       // top
            Rect::new(rect.x, rect.bottom() - w, rect.w, w),            // bottom
            Rect::new(rect.x, rect.y + w, w, (rect.h - 2.0 * w).max(0.0)), // left
            Rect::new(rect.right() - w, rect.y + w, w, (rect.h - 2.0 * w).max(0.0)), // right
        ];
        for s in sides {
            if !s.is_empty() {
                self.fill_rect(s, color, alpha, clip);
            }
        }
    }

    /// Consume the canvas, yielding the RGBA8 buffer that `transcode`'s
    /// encoders take.
    pub fn into_rgba(self) -> Vec<u8> {
        self.data
    }
}

/// Add an inside span `[x0, x1)` to a coverage row with exact horizontal
/// coverage: whole pixels get `weight`, the two partial end pixels get the
/// fraction they actually contain.
fn add_span(cov: &mut [f32], x0: f32, x1: f32, weight: f32, offset: usize, limit: usize) {
    let lo = x0.max(offset as f32);
    let hi = x1.min(limit as f32);
    if hi <= lo {
        return;
    }
    let first = lo.floor() as usize;
    let last = (hi.ceil() as usize).saturating_sub(1);

    if first == last {
        cov[first - offset] += (hi - lo) * weight;
        return;
    }
    cov[first - offset] += ((first + 1) as f32 - lo) * weight;
    for i in (first + 1)..last {
        cov[i - offset] += weight;
    }
    if last >= offset && last - offset < cov.len() {
        cov[last - offset] += (hi - last as f32) * weight;
    }
}

/// Source-over composite, for callers outside this module (the glyph cache
/// blits masks and needs exactly this blend).
pub fn blend_pub(dst: &mut [u8], color: Rgba, a: f32) {
    blend(dst, color, a)
}

/// Source-over composite of `color` at coverage-scaled alpha `a` onto `dst`.
fn blend(dst: &mut [u8], color: Rgba, a: f32) {
    let a = a.clamp(0.0, 1.0);
    if a >= 1.0 {
        dst[0] = color.r;
        dst[1] = color.g;
        dst[2] = color.b;
        dst[3] = 255;
        return;
    }
    let inv = 1.0 - a;
    let da = dst[3] as f32 / 255.0;
    let out_a = a + da * inv;
    if out_a <= 0.0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        return;
    }
    // Composite in straight alpha: weight each source by its own coverage.
    let mix = |s: u8, d: u8| -> u8 {
        let s = s as f32;
        let d = d as f32;
        (((s * a) + (d * da * inv)) / out_a).round().clamp(0.0, 255.0) as u8
    };
    dst[0] = mix(color.r, dst[0]);
    dst[1] = mix(color.g, dst[1]);
    dst[2] = mix(color.b, dst[2]);
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink(c: &Canvas) -> usize {
        c.data.chunks_exact(4).filter(|p| p[3] > 0).count()
    }

    #[test]
    fn pixel_aligned_rect_is_exact() {
        let mut c = Canvas::new(20, 20);
        c.fill_rect(Rect::new(5.0, 5.0, 10.0, 10.0), Rgba::WHITE, 1.0, None);
        assert_eq!(ink(&c), 100, "a 10x10 rect must cover exactly 100 pixels");
        assert_eq!(c.pixel(5, 5), Rgba::WHITE);
        assert_eq!(c.pixel(14, 14), Rgba::WHITE);
        assert_eq!(c.pixel(15, 15).a, 0, "outside the rect must stay untouched");
        assert_eq!(c.pixel(4, 4).a, 0);
    }

    #[test]
    fn half_pixel_edge_is_antialiased() {
        let mut c = Canvas::new(10, 10);
        // Right edge at x=5.5 → column 5 is half covered.
        c.fill_rect(Rect::new(0.0, 0.0, 5.5, 10.0), Rgba::WHITE, 1.0, None);
        assert_eq!(c.pixel(4, 3).a, 255, "fully inside");
        let edge = c.pixel(5, 3).a;
        assert!(
            (120..=135).contains(&edge),
            "half-covered pixel should be ~50% alpha, got {edge}"
        );
        assert_eq!(c.pixel(6, 3).a, 0, "fully outside");
    }

    #[test]
    fn coverage_is_exact_for_a_fractional_rect() {
        let mut c = Canvas::new(8, 8);
        c.fill_rect(Rect::new(1.25, 1.0, 2.5, 1.0), Rgba::WHITE, 1.0, None);
        // x spans 1.25..3.75 → 0.75 of px1, 1.0 of px2, 0.75 of px3.
        let a = |x| c.pixel(x, 1).a as f32 / 255.0;
        assert!((a(1) - 0.75).abs() < 0.03, "px1 got {}", a(1));
        assert!((a(2) - 1.00).abs() < 0.03, "px2 got {}", a(2));
        assert!((a(3) - 0.75).abs() < 0.03, "px3 got {}", a(3));
        assert_eq!(c.pixel(4, 1).a, 0);
    }

    #[test]
    fn nonzero_winding_leaves_a_hole() {
        // Outer square clockwise, inner square counter-clockwise → a ring.
        let mut p = Path::new();
        p.move_to(2.0, 2.0);
        p.line_to(18.0, 2.0);
        p.line_to(18.0, 18.0);
        p.line_to(2.0, 18.0);
        p.close();
        p.move_to(7.0, 7.0);
        p.line_to(7.0, 13.0);
        p.line_to(13.0, 13.0);
        p.line_to(13.0, 7.0);
        p.close();

        let mut c = Canvas::new(20, 20);
        c.fill_path(&p, Rgba::WHITE, 1.0, None);
        assert_eq!(c.pixel(3, 10).a, 255, "ring body should be filled");
        assert_eq!(c.pixel(10, 10).a, 0, "the hole must be empty");
        assert_eq!(ink(&c), 16 * 16 - 6 * 6);
    }

    #[test]
    fn clip_limits_drawing() {
        let mut c = Canvas::new(20, 20);
        let clip = Rect::new(0.0, 0.0, 10.0, 20.0);
        c.fill_rect(Rect::new(0.0, 0.0, 20.0, 20.0), Rgba::WHITE, 1.0, Some(clip));
        assert_eq!(c.pixel(9, 10).a, 255);
        assert_eq!(c.pixel(10, 10).a, 0, "clip must cut the fill");
        assert_eq!(ink(&c), 10 * 20);
    }

    #[test]
    fn alpha_composites_source_over() {
        let mut c = Canvas::filled(4, 4, Rgba::BLACK);
        c.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), Rgba::WHITE, 0.5, None);
        let p = c.pixel(1, 1);
        assert_eq!(p.a, 255, "over an opaque backdrop the result stays opaque");
        assert!(
            (120..=135).contains(&p.r),
            "50% white over black should be mid grey, got {}",
            p.r
        );
    }

    #[test]
    fn opacity_zero_draws_nothing() {
        let mut c = Canvas::new(8, 8);
        c.fill_rect(Rect::new(0.0, 0.0, 8.0, 8.0), Rgba::WHITE, 0.0, None);
        assert_eq!(ink(&c), 0);
    }

    #[test]
    fn drawing_outside_the_canvas_is_safe() {
        let mut c = Canvas::new(8, 8);
        c.fill_rect(Rect::new(-50.0, -50.0, 20.0, 20.0), Rgba::WHITE, 1.0, None);
        c.fill_rect(Rect::new(100.0, 100.0, 20.0, 20.0), Rgba::WHITE, 1.0, None);
        c.fill_rect(Rect::new(-5.0, -5.0, 100.0, 100.0), Rgba::WHITE, 1.0, None);
        assert_eq!(ink(&c), 64, "the covering rect fills the whole canvas");
    }

    #[test]
    fn round_rect_corners_are_cut() {
        let mut c = Canvas::new(40, 40);
        c.fill_round_rect(Rect::new(0.0, 0.0, 40.0, 40.0), 12.0, Rgba::WHITE, 1.0, None);
        assert_eq!(c.pixel(20, 20).a, 255, "centre filled");
        assert_eq!(c.pixel(0, 0).a, 0, "corner cut away");
        assert_eq!(c.pixel(39, 0).a, 0);
        assert!(ink(&c) < 1600, "a rounded rect covers less than the full square");
        assert!(ink(&c) > 1300, "but not dramatically less");
    }

    #[test]
    fn radius_is_clamped_not_inverted() {
        let mut c = Canvas::new(20, 20);
        // Radius far larger than the rect — must degrade gracefully.
        c.fill_round_rect(Rect::new(0.0, 0.0, 20.0, 10.0), 999.0, Rgba::WHITE, 1.0, None);
        assert!(ink(&c) > 0, "an over-large radius must still draw something");
        assert_eq!(c.pixel(10, 5).a, 255, "centre still filled");
    }

    #[test]
    fn stroke_draws_a_border_not_a_fill() {
        let mut c = Canvas::new(20, 20);
        c.stroke_rect(Rect::new(2.0, 2.0, 16.0, 16.0), 1.0, Rgba::WHITE, 1.0, None);
        assert_eq!(c.pixel(2, 2).a, 255, "border drawn");
        assert_eq!(c.pixel(10, 10).a, 0, "interior left empty");
        assert_eq!(ink(&c), 16 * 16 - 14 * 14);
    }

    #[test]
    fn transform_translates_and_scales() {
        let p = Path::rect(Rect::new(0.0, 0.0, 2.0, 2.0));
        let moved = p.transform(&Transform::translate(3.0, 4.0));
        let b = moved.bounds().unwrap();
        assert_eq!((b.x, b.y, b.w, b.h), (3.0, 4.0, 2.0, 2.0));

        let scaled = p.transform(&Transform::scale(3.0, 2.0));
        let b = scaled.bounds().unwrap();
        assert_eq!((b.w, b.h), (6.0, 4.0));
    }

    #[test]
    fn transform_composition_order_is_self_then_next() {
        // Scale by 2 then translate by 10: the translation must not be scaled.
        let t = Transform::scale(2.0, 2.0).then(&Transform::translate(10.0, 0.0));
        assert_eq!(t.apply(1.0, 0.0), (12.0, 0.0));
        // Translate then scale: the translation is scaled.
        let t = Transform::translate(10.0, 0.0).then(&Transform::scale(2.0, 2.0));
        assert_eq!(t.apply(1.0, 0.0), (22.0, 0.0));
    }

    #[test]
    fn hex_colours_parse_and_reject() {
        assert_eq!(Rgba::from_hex("#fff"), Some(Rgba::WHITE));
        assert_eq!(Rgba::from_hex("#ff0000"), Some(Rgba::rgb(255, 0, 0)));
        assert_eq!(Rgba::from_hex("#00ff0080"), Some(Rgba::new(0, 255, 0, 128)));
        assert_eq!(Rgba::from_hex("fff"), None, "missing # must not parse");
        assert_eq!(Rgba::from_hex("#gg0000"), None);
        assert_eq!(Rgba::from_hex("#ff00"), None);
    }

    #[test]
    fn empty_and_degenerate_paths_are_no_ops() {
        let mut c = Canvas::new(8, 8);
        c.fill_path(&Path::new(), Rgba::WHITE, 1.0, None);
        let mut single = Path::new();
        single.move_to(1.0, 1.0);
        c.fill_path(&single, Rgba::WHITE, 1.0, None);
        c.fill_rect(Rect::new(0.0, 0.0, 0.0, 10.0), Rgba::WHITE, 1.0, None);
        assert_eq!(ink(&c), 0);
    }
}

/// Blur an RGBA8 buffer in place with a triple box blur.
///
/// Three successive box passes converge on a Gaussian (central limit theorem)
/// and cost O(n) per pass regardless of radius, where a true Gaussian kernel
/// costs O(n·r). At the radii compositions use for depth-of-field transitions —
/// 10 to 20 pixels — the difference is well under one 8-bit level while a
/// direct convolution is roughly 20x slower.
///
/// Alpha is blurred with colour, so a blurred element's edges soften rather
/// than staying hard against the backdrop.
pub fn blur(canvas: &mut Canvas, radius: f32) {
    let (w, h) = (canvas.width, canvas.height);
    blur_slice(&mut canvas.data, w, h, radius)
}

/// Blur only the rows a region touches.
///
/// A blurred element usually occupies a fraction of the frame, and blurring the
/// untouched remainder is pure cost. The band is blurred **in place** through a
/// mutable slice: copying it into its own canvas and back allocated a second
/// full-frame buffer for every blurred element of every frame, which on a
/// parallel render is one such buffer per worker simultaneously.
pub fn blur_region(canvas: &mut Canvas, radius: f32, region: Option<Rect>) {
    if radius <= 0.5 {
        return;
    }
    let (cw, ch) = (canvas.width, canvas.height);
    let Some(r) = region else {
        return blur_slice(&mut canvas.data, cw, ch, radius);
    };
    let r = r.intersect(&Rect::new(0.0, 0.0, cw as f32, ch as f32));
    if r.is_empty() {
        return;
    }
    let y0 = r.y.floor().max(0.0) as usize;
    let y1 = (r.bottom().ceil() as usize).min(ch as usize);
    if y0 >= y1 {
        return;
    }
    let w = cw as usize;
    blur_slice(
        &mut canvas.data[y0 * w * 4..y1 * w * 4],
        cw,
        (y1 - y0) as u32,
        radius,
    )
}

/// Blur an RGBA8 slice in place.
fn blur_slice(data: &mut [u8], width: u32, height: u32, radius: f32) {
    if radius <= 0.5 || width == 0 || height == 0 {
        return;
    }
    // Blur at reduced resolution for large radii.
    //
    // A blur is a low-pass filter: it destroys exactly the high-frequency
    // detail that full resolution preserves, so computing it at full scale
    // spends work producing information the operation then discards. Halving
    // the resolution quarters the pixel count and halves the required radius,
    // and at these radii the difference is invisible. Browser compositors do
    // the same.
    let scale = ((radius / 4.0).floor() as u32).clamp(1, 8);
    if scale > 1 && width > scale * 4 && height > scale * 4 {
        let sw = width / scale;
        let sh = height / scale;
        let mut small = vec![0u8; sw as usize * sh as usize * 4];
        downsample(data, width, height, &mut small, sw, sh);
        blur_passes(&mut small, sw, sh, radius / scale as f32);
        upsample(&small, sw, sh, data, width, height);
        return;
    }
    blur_passes(data, width, height, radius)
}

fn blur_passes(data: &mut [u8], width: u32, height: u32, radius: f32) {
    // Box width that best matches a Gaussian of this standard deviation.
    let w = ((radius * 3.0 * (2.0 * std::f32::consts::PI).sqrt() / 4.0) + 0.5)
        .floor()
        .max(1.0);
    let r = (w as usize).max(1);
    for _ in 0..3 {
        box_blur_h(data, width, height, r);
        box_blur_v(data, width, height, r);
    }
}

fn box_blur_h(data: &mut [u8], width: u32, height: u32, r: usize) {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return;
    }
    let mut row = vec![0u8; w * 4];
    for y in 0..h {
        let base = y * w * 4;
        row.copy_from_slice(&data[base..base + w * 4]);
        // Running sums, so each output pixel costs an add and a subtract
        // rather than a full kernel pass.
        let mut acc = [0u32; 4];
        for x in 0..=r.min(w - 1) {
            for k in 0..4 {
                acc[k] += row[x * 4 + k] as u32;
            }
        }
        let mut count = (r.min(w - 1) + 1) as u32;
        for x in 0..w {
            for k in 0..4 {
                // Round rather than truncate. Flooring loses up to half a level
                // per pixel per pass; over three iterations in two directions
                // that compounds to a visible ~30% alpha loss, so a blurred
                // element would fade as well as soften.
                data[base + x * 4 + k] = ((acc[k] + count / 2) / count) as u8;
            }
            let add = x + r + 1;
            if add < w {
                for k in 0..4 {
                    acc[k] += row[add * 4 + k] as u32;
                }
                count += 1;
            }
            if x >= r {
                let sub = x - r;
                for k in 0..4 {
                    acc[k] -= row[sub * 4 + k] as u32;
                }
                count -= 1;
            }
        }
    }
}

fn box_blur_v(data: &mut [u8], width: u32, height: u32, r: usize) {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return;
    }
    let mut col = vec![0u8; h * 4];
    for x in 0..w {
        for y in 0..h {
            let src = (y * w + x) * 4;
            col[y * 4..y * 4 + 4].copy_from_slice(&data[src..src + 4]);
        }
        let mut acc = [0u32; 4];
        for y in 0..=r.min(h - 1) {
            for k in 0..4 {
                acc[k] += col[y * 4 + k] as u32;
            }
        }
        let mut count = (r.min(h - 1) + 1) as u32;
        for y in 0..h {
            let dst = (y * w + x) * 4;
            for k in 0..4 {
                data[dst + k] = ((acc[k] + count / 2) / count) as u8;
            }
            let add = y + r + 1;
            if add < h {
                for k in 0..4 {
                    acc[k] += col[add * 4 + k] as u32;
                }
                count += 1;
            }
            if y >= r {
                let sub = y - r;
                for k in 0..4 {
                    acc[k] -= col[sub * 4 + k] as u32;
                }
                count -= 1;
            }
        }
    }
}

/// Box-average into a smaller buffer.
fn downsample(src: &[u8], sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32) {
    let s_w = sw as usize;
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for y in 0..dh as usize {
        let y0 = (y as f32 * fy) as usize;
        let y1 = (((y + 1) as f32 * fy) as usize).min(sh as usize).max(y0 + 1);
        for x in 0..dw as usize {
            let x0 = (x as f32 * fx) as usize;
            let x1 = (((x + 1) as f32 * fx) as usize).min(s_w).max(x0 + 1);
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let i = (yy * s_w + xx) * 4;
                    for k in 0..4 {
                        acc[k] += src[i + k] as u32;
                    }
                    n += 1;
                }
            }
            let o = (y * dw as usize + x) * 4;
            for k in 0..4 {
                dst[o + k] = (acc[k] / n.max(1)) as u8;
            }
        }
    }
}

/// Bilinear expansion back over a larger buffer.
fn upsample(src: &[u8], sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32) {
    let s_w = sw as usize;
    let s_h = sh as usize;
    let d_w = dw as usize;
    for y in 0..dh as usize {
        let fy = (y as f32 + 0.5) * s_h as f32 / dh as f32 - 0.5;
        let y0 = fy.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(s_h - 1);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..d_w {
            let fx = (x as f32 + 0.5) * s_w as f32 / dw as f32 - 0.5;
            let x0 = fx.floor().max(0.0) as usize;
            let x1 = (x0 + 1).min(s_w - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let o = (y * d_w + x) * 4;
            for k in 0..4 {
                let p = |yy: usize, xx: usize| src[(yy * s_w + xx) * 4 + k] as f32;
                let top = p(y0, x0) * (1.0 - tx) + p(y0, x1) * tx;
                let bot = p(y1, x0) * (1.0 - tx) + p(y1, x1) * tx;
                dst[o + k] = (top * (1.0 - ty) + bot * ty).round() as u8;
            }
        }
    }
}

/// Composite `src` over `dst` at full extent, honouring src alpha and a global
/// alpha, clipped to `clip`.
pub fn composite(dst: &mut Canvas, src: &Canvas, alpha: f32, clip: Option<Rect>) {
    let region = match clip {
        Some(c) => c.intersect(&dst.bounds()),
        None => dst.bounds(),
    };
    if region.is_empty() {
        return;
    }
    let x0 = region.x.floor().max(0.0) as usize;
    let y0 = region.y.floor().max(0.0) as usize;
    let x1 = (region.right().ceil() as usize).min(dst.width as usize);
    let y1 = (region.bottom().ceil() as usize).min(dst.height as usize);
    let w = dst.width as usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * w + x) * 4;
            let sa = src.data[i + 3] as f32 / 255.0 * alpha;
            if sa <= 0.0 {
                continue;
            }
            let color = Rgba::new(src.data[i], src.data[i + 1], src.data[i + 2], 255);
            blend(&mut dst.data[i..i + 4], color, sa);
        }
    }
}

#[cfg(test)]
mod blur_tests {
    use super::*;

    #[test]
    fn blur_spreads_ink_and_conserves_it_roughly() {
        let mut c = Canvas::new(64, 64);
        c.fill_rect(Rect::new(28.0, 28.0, 8.0, 8.0), Rgba::WHITE, 1.0, None);
        let before: u32 = c.data.chunks_exact(4).map(|p| p[3] as u32).sum();
        let lit_before = c.data.chunks_exact(4).filter(|p| p[3] > 0).count();

        blur(&mut c, 6.0);

        let after: u32 = c.data.chunks_exact(4).map(|p| p[3] as u32).sum();
        let lit_after = c.data.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(lit_after > lit_before * 3, "blur must spread the ink");
        // Total alpha is roughly preserved; box blur loses a little to rounding.
        let ratio = after as f32 / before as f32;
        assert!((0.85..=1.05).contains(&ratio), "alpha ratio {ratio}");
        // The centre stays brightest.
        assert!(c.pixel(32, 32).a > c.pixel(32, 50).a);
    }

    #[test]
    fn zero_radius_is_a_no_op() {
        let mut c = Canvas::new(16, 16);
        c.fill_rect(Rect::new(4.0, 4.0, 8.0, 8.0), Rgba::WHITE, 1.0, None);
        let before = c.data.clone();
        blur(&mut c, 0.0);
        assert_eq!(c.data, before);
    }

    #[test]
    fn composite_lays_one_canvas_over_another() {
        let mut dst = Canvas::filled(8, 8, Rgba::BLACK);
        let mut src = Canvas::new(8, 8);
        src.fill_rect(Rect::new(0.0, 0.0, 4.0, 8.0), Rgba::WHITE, 1.0, None);
        composite(&mut dst, &src, 1.0, None);
        assert_eq!(dst.pixel(1, 1), Rgba::WHITE);
        assert_eq!(dst.pixel(6, 1), Rgba::BLACK, "uncovered area untouched");
    }

    #[test]
    fn composite_respects_clip_and_alpha() {
        let mut dst = Canvas::filled(8, 8, Rgba::BLACK);
        let mut src = Canvas::new(8, 8);
        src.clear(Rgba::WHITE);
        composite(&mut dst, &src, 0.5, Some(Rect::new(0.0, 0.0, 4.0, 8.0)));
        let p = dst.pixel(1, 1).r;
        assert!((120..=135).contains(&p), "half white over black, got {p}");
        assert_eq!(dst.pixel(6, 1), Rgba::BLACK, "outside the clip");
    }
}
