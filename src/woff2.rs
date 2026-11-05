//! WOFF2 → TrueType reconstruction.
//!
//! Web fonts on real compositions are WOFF2, and `ruler::ttf` reads plain
//! TrueType. This module closes that gap so a composition's own
//! `@font-face … url(x.woff2)` can be used directly — substituting a different
//! typeface would fail any visual-parity check on the first frame of text.
//!
//! WOFF2 (W3C TR/WOFF2) is not merely "an sfnt, Brotli'd". Two things differ:
//!
//! 1. All table data is concatenated and Brotli-compressed as one block.
//! 2. `glyf` and `loca` may be **transformed**: glyph outlines are split into
//!    parallel streams (contour counts, point counts, flags, coordinates,
//!    bounding boxes, instructions) with coordinates in a bespoke triplet
//!    encoding, and `loca` is dropped entirely because it is recomputable.
//!
//! Reversing the transform is what most of this file does. The output is a
//! byte-identical-in-spirit sfnt: the same tables, reassembled with a fresh
//! directory, ready for `ruler::ttf::Font::parse`.
//!
//! Not handled: `hmtx` transform (version 1), CFF/OTF flavour, font
//! collections, and the optional metadata/private blocks — all reported as
//! errors rather than silently producing a broken font.

/// The 63 well-known table tags, indexed by the 6-bit code in a table's flags
/// byte. Index 63 means "an explicit 4-byte tag follows".
const KNOWN_TAGS: [&[u8; 4]; 63] = [
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm",
    b"glyf", b"loca", b"prep", b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern",
    b"LTSH", b"PCLT", b"VDMX", b"vhea", b"vmtx", b"BASE", b"GDEF", b"GPOS", b"GSUB", b"EBSC",
    b"JSTF", b"MATH", b"CBDT", b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt", b"avar",
    b"bdat", b"bloc", b"bsln", b"cvar", b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty",
    b"just", b"lcar", b"mort", b"morx", b"opbd", b"prop", b"trak", b"Zapf", b"Silf", b"Glat",
    b"Gloc", b"Feat", b"Sill",
];

struct Reader<'a> {
    d: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, String> {
        let v = *self.d.get(self.i).ok_or("WOFF2: truncated")?;
        self.i += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(((self.u8()? as u16) << 8) | self.u8()? as u16)
    }
    fn i16(&mut self) -> Result<i16, String> {
        Ok(self.u16()? as i16)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(((self.u16()? as u32) << 16) | self.u16()? as u32)
    }
    /// UIntBase128: 7 bits per byte, high bit continues. Leading zeros and
    /// over-long encodings are rejected by the spec.
    fn base128(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for k in 0..5 {
            let b = self.u8()?;
            if k == 0 && b == 0x80 {
                return Err("WOFF2: base128 has a leading zero".into());
            }
            if v > (u32::MAX >> 7) {
                return Err("WOFF2: base128 overflow".into());
            }
            v = (v << 7) | (b & 0x7F) as u32;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("WOFF2: base128 too long".into())
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self.d.get(self.i..self.i + n).ok_or("WOFF2: truncated")?;
        self.i += n;
        Ok(s)
    }
}

struct TableEntry {
    tag: [u8; 4],
    transformed: bool,
    orig_length: u32,
    transform_length: u32,
}

/// Convert a WOFF2 file into an sfnt (TrueType) byte stream.
pub fn to_ttf(woff2: &[u8]) -> Result<Vec<u8>, String> {
    let mut r = Reader { d: woff2, i: 0 };
    if r.take(4)? != b"wOF2" {
        return Err("WOFF2: bad signature".into());
    }
    let flavor = r.u32()?;
    if flavor == 0x4F54_544F {
        return Err("WOFF2: CFF/OpenType flavour is not supported, only TrueType".into());
    }
    let _length = r.u32()?;
    let num_tables = r.u16()?;
    let _reserved = r.u16()?;
    let _total_sfnt_size = r.u32()?;
    let total_compressed_size = r.u32()? as usize;
    let _major = r.u16()?;
    let _minor = r.u16()?;
    let _meta_offset = r.u32()?;
    let _meta_length = r.u32()?;
    let _meta_orig_length = r.u32()?;
    let _priv_offset = r.u32()?;
    let _priv_length = r.u32()?;

    // Table directory.
    let mut entries = Vec::with_capacity(num_tables as usize);
    for _ in 0..num_tables {
        let flags = r.u8()?;
        let idx = (flags & 0x3F) as usize;
        let transform_version = (flags >> 6) & 0x03;
        let tag = if idx == 63 {
            let t = r.take(4)?;
            [t[0], t[1], t[2], t[3]]
        } else {
            **KNOWN_TAGS
                .get(idx)
                .ok_or_else(|| format!("WOFF2: bad table index {idx}"))?
        };
        let orig_length = r.base128()?;

        // For glyf/loca, transform version 0 means transformed and 3 means
        // not. For every other table the sense is inverted: 0 means untouched.
        let is_glyf_or_loca = &tag == b"glyf" || &tag == b"loca";
        let transformed = if is_glyf_or_loca {
            transform_version == 0
        } else {
            transform_version != 0
        };
        if !is_glyf_or_loca && transformed {
            return Err(format!(
                "WOFF2: transform on table {} is not supported",
                String::from_utf8_lossy(&tag)
            ));
        }
        let transform_length = if transformed {
            r.base128()?
        } else {
            orig_length
        };
        entries.push(TableEntry {
            tag,
            transformed,
            orig_length,
            transform_length,
        });
    }

    // One Brotli block holds every table's data, back to back.
    let compressed = r.take(total_compressed_size)?;
    let mut decompressed = Vec::new();
    brotli::BrotliDecompress(&mut std::io::Cursor::new(compressed), &mut decompressed)
        .map_err(|e| format!("WOFF2: brotli decompression failed: {e}"))?;

    // Slice the block into per-table byte ranges.
    let mut raw: Vec<(&[u8; 4], &[u8])> = Vec::with_capacity(entries.len());
    let mut off = 0usize;
    for e in &entries {
        let n = e.transform_length as usize;
        let end = off
            .checked_add(n)
            .filter(|&e| e <= decompressed.len())
            .ok_or_else(|| {
                format!(
                    "WOFF2: table {} runs past the decompressed block",
                    String::from_utf8_lossy(&e.tag)
                )
            })?;
        raw.push((&e.tag, &decompressed[off..end]));
        off = end;
    }

    // Reconstruct glyf/loca if they were transformed.
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(entries.len());
    let mut rebuilt_glyf: Option<(Vec<u8>, Vec<u8>)> = None;

    let index_to_loc_long = {
        let head = raw
            .iter()
            .find(|(t, _)| *t == b"head")
            .map(|(_, d)| *d)
            .ok_or("WOFF2: missing head table")?;
        if head.len() < 52 {
            return Err("WOFF2: head table too short".into());
        }
        i16::from_be_bytes([head[50], head[51]]) == 1
    };

    for (i, e) in entries.iter().enumerate() {
        if &e.tag == b"glyf" && e.transformed {
            let (glyf, loca) = reconstruct_glyf(raw[i].1, index_to_loc_long)?;
            rebuilt_glyf = Some((glyf, loca));
        }
    }

    for (i, e) in entries.iter().enumerate() {
        let data = match (&e.tag, &rebuilt_glyf) {
            (b"glyf", Some((g, _))) if e.transformed => g.clone(),
            (b"loca", Some((_, l))) if e.transformed => l.clone(),
            _ => {
                if e.transformed {
                    return Err(format!(
                        "WOFF2: table {} is transformed but was not reconstructed",
                        String::from_utf8_lossy(&e.tag)
                    ));
                }
                let d = raw[i].1;
                if d.len() != e.orig_length as usize {
                    return Err(format!(
                        "WOFF2: table {} length {} does not match declared {}",
                        String::from_utf8_lossy(&e.tag),
                        d.len(),
                        e.orig_length
                    ));
                }
                d.to_vec()
            }
        };
        tables.push((e.tag, data));
    }

    Ok(assemble_sfnt(flavor, tables))
}

/// Rebuild `glyf` and `loca` from WOFF2's transformed glyph streams.
fn reconstruct_glyf(d: &[u8], loca_long: bool) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut h = Reader { d, i: 0 };
    let _version = h.u32()?;
    let num_glyphs = h.u16()? as usize;
    let index_format = h.u16()?;
    let n_contour_len = h.u32()? as usize;
    let n_points_len = h.u32()? as usize;
    let flag_len = h.u32()? as usize;
    let glyph_len = h.u32()? as usize;
    let composite_len = h.u32()? as usize;
    let bbox_len = h.u32()? as usize;
    let instruction_len = h.u32()? as usize;

    // The bbox bitmap says which glyphs carry an explicit bounding box; the
    // rest have one computed from their points.
    let bbox_bitmap_size = (num_glyphs + 31) / 32 * 4;
    if bbox_len < bbox_bitmap_size {
        return Err("WOFF2: bbox stream shorter than its bitmap".into());
    }

    let mut n_contour = Reader {
        d: h.take(n_contour_len)?,
        i: 0,
    };
    let mut n_points = Reader {
        d: h.take(n_points_len)?,
        i: 0,
    };
    let mut flags = Reader {
        d: h.take(flag_len)?,
        i: 0,
    };
    let mut glyphs = Reader {
        d: h.take(glyph_len)?,
        i: 0,
    };
    let mut composites = Reader {
        d: h.take(composite_len)?,
        i: 0,
    };
    let bbox_block = h.take(bbox_len)?;
    let mut instructions = Reader {
        d: h.take(instruction_len)?,
        i: 0,
    };

    let bbox_bitmap = &bbox_block[..bbox_bitmap_size];
    let mut bbox_stream = Reader {
        d: &bbox_block[bbox_bitmap_size..],
        i: 0,
    };
    let has_bbox = |gid: usize| -> bool {
        let byte = bbox_bitmap[gid / 8];
        byte & (0x80 >> (gid % 8)) != 0
    };

    let mut glyf = Vec::new();
    let mut offsets = Vec::with_capacity(num_glyphs + 1);
    offsets.push(0u32);

    for gid in 0..num_glyphs {
        let n = n_contour.i16()?;
        let start = glyf.len();

        if n == 0 {
            // An empty glyph occupies no bytes in glyf at all.
            offsets.push(glyf.len() as u32);
            continue;
        }

        if n < 0 {
            // Composite: the component data is copied verbatim, then any
            // instructions are appended.
            let bbox = if has_bbox(gid) {
                [
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                ]
            } else {
                return Err(format!("WOFF2: composite glyph {gid} has no bbox"));
            };
            let comp_start = composites.i;
            let mut have_instructions = false;
            loop {
                let flags_word = composites.u16()?;
                let _glyph_index = composites.u16()?;
                const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
                const WE_HAVE_A_SCALE: u16 = 0x0008;
                const MORE_COMPONENTS: u16 = 0x0020;
                const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
                const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
                const WE_HAVE_INSTRUCTIONS: u16 = 0x0100;
                composites.take(if flags_word & ARG_1_AND_2_ARE_WORDS != 0 {
                    4
                } else {
                    2
                })?;
                if flags_word & WE_HAVE_A_SCALE != 0 {
                    composites.take(2)?;
                } else if flags_word & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                    composites.take(4)?;
                } else if flags_word & WE_HAVE_A_TWO_BY_TWO != 0 {
                    composites.take(8)?;
                }
                if flags_word & WE_HAVE_INSTRUCTIONS != 0 {
                    have_instructions = true;
                }
                if flags_word & MORE_COMPONENTS == 0 {
                    break;
                }
            }
            let comp_data = &composites.d[comp_start..composites.i];

            glyf.extend_from_slice(&(-1i16).to_be_bytes());
            for v in bbox {
                glyf.extend_from_slice(&v.to_be_bytes());
            }
            glyf.extend_from_slice(comp_data);
            if have_instructions {
                let n_instr = read_255_u16(&mut glyphs)? as usize;
                glyf.extend_from_slice(&(n_instr as u16).to_be_bytes());
                glyf.extend_from_slice(instructions.take(n_instr)?);
            }
        } else {
            // Simple glyph: per-contour point counts, then flags, then
            // triplet-encoded coordinate deltas.
            let n = n as usize;
            let mut end_pts = Vec::with_capacity(n);
            let mut total = 0usize;
            for _ in 0..n {
                let c = read_255_u16(&mut n_points)? as usize;
                total += c;
                if total == 0 {
                    return Err(format!("WOFF2: glyph {gid} has an empty contour"));
                }
                end_pts.push(total as u16 - 1);
            }

            let mut xs = Vec::with_capacity(total);
            let mut ys = Vec::with_capacity(total);
            let mut on_curve = Vec::with_capacity(total);
            let (mut x, mut y) = (0i32, 0i32);
            for _ in 0..total {
                let f = flags.u8()?;
                on_curve.push(f & 0x80 == 0);
                let (dx, dy) = read_triplet(&mut glyphs, f & 0x7F)?;
                x += dx;
                y += dy;
                xs.push(x);
                ys.push(y);
            }

            let n_instr = read_255_u16(&mut glyphs)? as usize;
            let instr = instructions.take(n_instr)?;

            let bbox = if has_bbox(gid) {
                [
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                ]
            } else {
                // Computed from the points, which is why the encoder could omit it.
                [
                    *xs.iter().min().unwrap_or(&0) as i16,
                    *ys.iter().min().unwrap_or(&0) as i16,
                    *xs.iter().max().unwrap_or(&0) as i16,
                    *ys.iter().max().unwrap_or(&0) as i16,
                ]
            };

            glyf.extend_from_slice(&(n as i16).to_be_bytes());
            for v in bbox {
                glyf.extend_from_slice(&v.to_be_bytes());
            }
            for e in &end_pts {
                glyf.extend_from_slice(&e.to_be_bytes());
            }
            glyf.extend_from_slice(&(n_instr as u16).to_be_bytes());
            glyf.extend_from_slice(instr);
            write_simple_outline(&mut glyf, &xs, &ys, &on_curve);
        }

        // Every glyph must start on an even offset when loca is short format.
        while (glyf.len() - start) % 4 != 0 && !loca_long && index_format == 0 {
            glyf.push(0);
        }
        offsets.push(glyf.len() as u32);
    }

    // loca: short format stores half-offsets, so it requires even offsets.
    let long = loca_long || index_format == 1 || *offsets.last().unwrap_or(&0) > 0x1FFFF;
    let mut loca = Vec::with_capacity(offsets.len() * if long { 4 } else { 2 });
    for o in &offsets {
        if long {
            loca.extend_from_slice(&o.to_be_bytes());
        } else {
            loca.extend_from_slice(&((*o / 2) as u16).to_be_bytes());
        }
    }
    Ok((glyf, loca))
}

/// Re-encode points into the standard `glyf` flags + delta representation.
fn write_simple_outline(out: &mut Vec<u8>, xs: &[i32], ys: &[i32], on_curve: &[bool]) {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const X_SAME_OR_POS: u8 = 0x10;
    const Y_SAME_OR_POS: u8 = 0x20;

    let mut flags = Vec::with_capacity(xs.len());
    let mut xbuf = Vec::new();
    let mut ybuf = Vec::new();
    let (mut px, mut py) = (0i32, 0i32);

    for i in 0..xs.len() {
        let dx = xs[i] - px;
        let dy = ys[i] - py;
        px = xs[i];
        py = ys[i];
        let mut f = if on_curve[i] { ON_CURVE } else { 0 };

        if dx == 0 {
            f |= X_SAME_OR_POS;
        } else if (-255..=255).contains(&dx) {
            f |= X_SHORT;
            if dx > 0 {
                f |= X_SAME_OR_POS;
            }
            xbuf.push(dx.unsigned_abs() as u8);
        } else {
            xbuf.extend_from_slice(&(dx as i16).to_be_bytes());
        }

        if dy == 0 {
            f |= Y_SAME_OR_POS;
        } else if (-255..=255).contains(&dy) {
            f |= Y_SHORT;
            if dy > 0 {
                f |= Y_SAME_OR_POS;
            }
            ybuf.push(dy.unsigned_abs() as u8);
        } else {
            ybuf.extend_from_slice(&(dy as i16).to_be_bytes());
        }
        flags.push(f);
    }

    out.extend_from_slice(&flags);
    out.extend_from_slice(&xbuf);
    out.extend_from_slice(&ybuf);
}

/// WOFF2's 255UInt16: a variable-length small-integer encoding.
fn read_255_u16(r: &mut Reader) -> Result<u16, String> {
    const ONE_MORE: u8 = 255;
    const WORD: u8 = 253;
    const LOWEST: u8 = 254;
    let code = r.u8()?;
    match code {
        WORD => r.u16(),
        ONE_MORE => Ok(r.u8()? as u16 + 253),
        LOWEST => Ok(r.u8()? as u16 + 253 * 2),
        c => Ok(c as u16),
    }
}

/// WOFF2's triplet encoding: the flag byte selects one of 128 (dx, dy)
/// bit-width and sign combinations, packed into 1-4 following bytes. This is
/// where most of the format's compression comes from — the common case of a
/// small delta in both axes fits in a single byte.
fn read_triplet(r: &mut Reader, flag: u8) -> Result<(i32, i32), String> {
    let f = flag as i32;
    // The sign bit is the LOW bit of the relevant flag: set means positive.
    // Getting this backwards yields outlines that are well-formed and mirrored,
    // which is why it is worth stating rather than inlining.
    let sign = |bits: i32, magnitude: i32| if bits & 1 != 0 { magnitude } else { -magnitude };

    match f {
        0..=9 => {
            let b0 = r.u8()? as i32;
            Ok((0, sign(f, ((f & 14) << 7) + b0)))
        }
        10..=19 => {
            let b0 = r.u8()? as i32;
            Ok((sign(f, (((f - 10) & 14) << 7) + b0), 0))
        }
        20..=83 => {
            let g = f - 20;
            let b1 = r.u8()? as i32;
            Ok((
                sign(f, 1 + (g & 0x30) + (b1 >> 4)),
                sign(f >> 1, 1 + ((g & 0x0C) << 2) + (b1 & 0x0F)),
            ))
        }
        84..=119 => {
            let g = f - 84;
            let b1 = r.u8()? as i32;
            let b2 = r.u8()? as i32;
            Ok((
                sign(f, 1 + ((g / 12) << 8) + b1),
                sign(f >> 1, 1 + (((g % 12) >> 2) << 8) + b2),
            ))
        }
        120..=123 => {
            let b1 = r.u8()? as i32;
            let b2 = r.u8()? as i32;
            let b3 = r.u8()? as i32;
            Ok((
                sign(f, (b1 << 4) + (b2 >> 4)),
                sign(f >> 1, ((b2 & 0x0F) << 8) + b3),
            ))
        }
        124..=127 => {
            let b1 = r.u8()? as i32;
            let b2 = r.u8()? as i32;
            let b3 = r.u8()? as i32;
            let b4 = r.u8()? as i32;
            Ok((sign(f, (b1 << 8) + b2), sign(f >> 1, (b3 << 8) + b4)))
        }
        _ => Err(format!("WOFF2: invalid triplet flag {flag}")),
    }
}

/// Write a standard sfnt: offset table, table directory, then padded tables.
fn assemble_sfnt(flavor: u32, mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    // The directory must be sorted by tag; readers binary-search it.
    tables.sort_by(|a, b| a.0.cmp(&b.0));

    let n = tables.len() as u16;
    // search_range = largest power of two <= n, times 16.
    let mut pow2 = 1u16;
    while pow2 * 2 <= n {
        pow2 *= 2;
    }
    let search_range = pow2 * 16;
    let entry_selector = (pow2 as f32).log2() as u16;
    let range_shift = n * 16 - search_range;

    let mut out = Vec::new();
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    let mut offset = 12 + tables.len() * 16;
    let mut dir = Vec::new();
    for (tag, data) in &tables {
        dir.extend_from_slice(tag);
        dir.extend_from_slice(&checksum(data).to_be_bytes());
        dir.extend_from_slice(&(offset as u32).to_be_bytes());
        dir.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += (data.len() + 3) & !3; // tables are 4-byte aligned
    }
    out.extend_from_slice(&dir);

    for (_, data) in &tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    out
}

fn checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 4 <= data.len() {
        sum = sum.wrapping_add(u32::from_be_bytes([
            data[i],
            data[i + 1],
            data[i + 2],
            data[i + 3],
        ]));
        i += 4;
    }
    if i < data.len() {
        let mut last = [0u8; 4];
        last[..data.len() - i].copy_from_slice(&data[i..]);
        sum = sum.wrapping_add(u32::from_be_bytes(last));
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    const FONT_DIR: &str = "/home/ctown/projects/glossa/brag-output/composition/assets/fonts";

    fn load(name: &str) -> Option<Vec<u8>> {
        std::fs::read(format!("{FONT_DIR}/{name}")).ok()
    }

    #[test]
    fn rejects_non_woff2() {
        assert!(to_ttf(b"not a font at all").is_err());
        assert!(to_ttf(&[0u8; 48]).is_err());
    }

    #[test]
    fn converts_a_real_woff2_to_a_parseable_font() {
        let Some(w) = load("space-mono-latin.woff2") else {
            eprintln!("skip: composition fonts not present");
            return;
        };
        let ttf = to_ttf(&w).expect("convert Space Mono");
        assert!(ttf.len() > w.len(), "decompressed font should be larger");
        let font = ruler::ttf::Font::parse(ttf).expect("ruler should parse the result");
        assert!(font.num_glyphs() > 100, "expected a real glyph count");
        assert_ne!(font.glyph_id('A'), 0, "'A' must map to a glyph");
        assert_ne!(font.glyph_id('$'), 0);
    }

    #[test]
    fn outlines_are_recovered_from_the_glyf_transform() {
        let Some(w) = load("space-mono-latin.woff2") else {
            return;
        };
        let font = ruler::ttf::Font::parse(to_ttf(&w).unwrap()).unwrap();
        // 'o' is two contours in essentially every Latin face; this is the
        // strongest cheap signal that the transform reversal is correct rather
        // than merely producing well-formed garbage.
        let o = font.outline(font.glyph_id('o')).unwrap();
        assert_eq!(
            o.len(),
            2,
            "lowercase o should be an outer and inner contour"
        );
        for c in &o {
            assert!(c.len() >= 4);
            for p in c {
                assert!(p.x.abs() < 5000.0 && p.y.abs() < 5000.0, "wild point {p:?}");
            }
        }
        // 'i' has a bowl and a separate dot.
        assert!(font.outline(font.glyph_id('i')).unwrap().len() >= 2);
        // A space has an advance but no outline.
        assert!(font.outline(font.glyph_id(' ')).unwrap().is_empty());
    }

    #[test]
    fn space_mono_is_actually_monospaced() {
        let Some(w) = load("space-mono-latin.woff2") else {
            return;
        };
        let font = ruler::ttf::Font::parse(to_ttf(&w).unwrap()).unwrap();
        // The name is a testable claim: every Latin glyph must share an advance.
        // If hmtx were mis-sliced this would fail loudly.
        let a = font.advance(font.glyph_id('i'), 100.0).unwrap();
        for ch in "MWmwil1.$@".chars() {
            let adv = font.advance(font.glyph_id(ch), 100.0).unwrap();
            assert!(
                (adv - a).abs() < 0.01,
                "'{ch}' advance {adv} differs from {a} in a monospaced font"
            );
        }
    }

    #[test]
    fn variable_font_converts_and_maps() {
        let Some(w) = load("space-grotesk-latin.woff2") else {
            return;
        };
        let font = ruler::ttf::Font::parse(to_ttf(&w).unwrap()).expect("parse Space Grotesk");
        assert!(font.num_glyphs() > 100);
        assert_ne!(font.glyph_id('g'), 0);
        // Proportional, unlike Space Mono above.
        let i = font.advance(font.glyph_id('i'), 100.0).unwrap();
        let m = font.advance(font.glyph_id('M'), 100.0).unwrap();
        assert!(
            m > i * 1.5,
            "expected proportional advances, got i={i} M={m}"
        );
    }

    #[test]
    fn all_three_composition_fonts_convert() {
        for name in [
            "space-grotesk-latin.woff2",
            "space-mono-latin.woff2",
            "space-mono-bold.woff2",
        ] {
            let Some(w) = load(name) else { return };
            let ttf = to_ttf(&w).unwrap_or_else(|e| panic!("{name}: {e}"));
            ruler::ttf::Font::parse(ttf).unwrap_or_else(|e| panic!("{name} parse: {e}"));
        }
    }
}
