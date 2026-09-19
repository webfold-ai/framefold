//! Resolving CSS font families to loaded font files.
//!
//! A composition names fonts the way CSS does — a family stack like
//! `"Space Mono", monospace` plus a weight — and supplies the actual files via
//! `@font-face`. This module turns the former into the latter, loading WOFF2
//! through [`crate::woff2`] so a composition's own assets are used rather than
//! a substitute, which would fail visual parity immediately.

use crate::css::FontFace;
use ruler::ttf::Font;
use std::path::Path;

struct Loaded {
    family: String,
    /// The `@font-face` weight as written: a single value like "400", or a
    /// variable range like "300 700".
    weight: String,
    font: Font,
}

/// Every font a composition declared, indexed for CSS-style lookup.
#[derive(Default)]
pub struct Fonts {
    loaded: Vec<Loaded>,
    fallback: Option<Font>,
}

impl Fonts {
    /// Load every `@font-face` in a stylesheet, resolving `src` relative to
    /// `base_dir`.
    ///
    /// A face that fails to load is reported rather than skipped: rendering
    /// with a silently substituted typeface is the exact failure this crate
    /// exists to avoid.
    pub fn load(faces: &[FontFace], base_dir: &Path) -> Result<Fonts, String> {
        let mut loaded = Vec::new();
        for f in faces {
            let path = base_dir.join(&f.src);
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("font {}: cannot read {}: {e}", f.family, path.display()))?;
            let ttf = if bytes.starts_with(b"wOF2") {
                crate::woff2::to_ttf(&bytes)
                    .map_err(|e| format!("font {}: {e}", f.family))?
            } else {
                bytes
            };
            let font = Font::parse(ttf).map_err(|e| format!("font {}: {e}", f.family))?;
            loaded.push(Loaded {
                family: f.family.clone(),
                weight: f.weight.clone(),
                font,
            });
        }
        Ok(Fonts {
            loaded,
            fallback: None,
        })
    }

    /// A font used when a family stack names nothing that was declared.
    pub fn with_fallback(mut self, font: Font) -> Self {
        self.fallback = Some(font);
        self
    }

    /// Resolve a CSS `font-family` stack and numeric weight.
    ///
    /// The stack is tried in order, as CSS specifies: the first declared family
    /// wins, and generic names (`sans-serif`, `monospace`) only reach the
    /// fallback. Within a family, an exact weight match beats a variable range
    /// that merely contains the weight, which in turn beats any other face.
    pub fn resolve(&self, family_stack: &str, weight: f32) -> Option<&Font> {
        for name in family_stack.split(',') {
            let name = name.trim().trim_matches(|c| c == '"' || c == '\'');
            if name.is_empty() {
                continue;
            }
            let candidates: Vec<&Loaded> = self
                .loaded
                .iter()
                .filter(|l| l.family.eq_ignore_ascii_case(name))
                .collect();
            if candidates.is_empty() {
                continue;
            }
            // Exact single-weight match.
            if let Some(hit) = candidates.iter().find(|c| {
                c.weight
                    .trim()
                    .parse::<f32>()
                    .map(|w| (w - weight).abs() < 0.5)
                    .unwrap_or(false)
            }) {
                return Some(&hit.font);
            }
            // A variable range that covers the requested weight.
            if let Some(hit) = candidates.iter().find(|c| covers(&c.weight, weight)) {
                return Some(&hit.font);
            }
            // Otherwise the nearest declared weight, so "500" picks 400 over 700.
            if let Some(hit) = candidates.iter().min_by(|a, b| {
                let d = |c: &Loaded| {
                    c.weight
                        .split_whitespace()
                        .filter_map(|t| t.parse::<f32>().ok())
                        .map(|w| (w - weight).abs())
                        .fold(f32::INFINITY, f32::min)
                };
                d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
            }) {
                return Some(&hit.font);
            }
        }
        self.fallback.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty() && self.fallback.is_none()
    }

    /// Resolve the font that should actually draw `ch`.
    ///
    /// Web fonts are usually subsetted — glossa's Space Mono carries Latin and
    /// nothing else — so a character like U+2713 CHECK MARK maps to glyph 0
    /// (.notdef) and would paint as a tofu box. A browser silently falls back
    /// to another installed family for exactly these characters, per-glyph
    /// rather than per-run, so matching its output requires doing the same.
    ///
    /// Returns the primary font when it has the glyph, otherwise the first
    /// font that does, otherwise the primary so the caller still draws
    /// something rather than nothing.
    pub fn resolve_for_char(&self, family_stack: &str, weight: f32, ch: char) -> Option<&Font> {
        let primary = self.resolve(family_stack, weight)?;
        if primary.glyph_id(ch) != 0 || ch == '\u{0}' {
            return Some(primary);
        }
        // Whitespace has no outline by design; never treat it as missing.
        if ch.is_whitespace() {
            return Some(primary);
        }
        for l in &self.loaded {
            if l.font.glyph_id(ch) != 0 {
                return Some(&l.font);
            }
        }
        if let Some(f) = &self.fallback {
            if f.glyph_id(ch) != 0 {
                return Some(f);
            }
        }
        Some(primary)
    }
}

/// Does a `font-weight` descriptor like "300 700" cover `weight`?
fn covers(descriptor: &str, weight: f32) -> bool {
    let nums: Vec<f32> = descriptor
        .split_whitespace()
        .filter_map(|t| t.parse::<f32>().ok())
        .collect();
    match nums.len() {
        2 => weight >= nums[0] - 0.5 && weight <= nums[1] + 0.5,
        _ => false,
    }
}

/// Parse a CSS `font-weight` into a number. Keywords map to their CSS values;
/// anything unrecognised is normal weight.
pub fn weight_value(v: Option<&str>) -> f32 {
    match v.map(|s| s.trim()) {
        Some("bold") => 700.0,
        Some("bolder") => 700.0,
        Some("lighter") => 300.0,
        Some("normal") | None => 400.0,
        Some(n) => n.parse().unwrap_or(400.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMP: &str = "/home/ctown/projects/glossa/brag-output/composition";

    fn real_faces() -> Option<(Vec<FontFace>, std::path::PathBuf)> {
        let src = std::fs::read_to_string(format!("{COMP}/index.html")).ok()?;
        let root = crate::html::parse(&src).ok()?;
        let sheet = crate::css::parse(&root.collect_styles());
        Some((sheet.font_faces, std::path::PathBuf::from(COMP)))
    }

    #[test]
    fn weights_parse() {
        assert_eq!(weight_value(Some("500")), 500.0);
        assert_eq!(weight_value(Some("bold")), 700.0);
        assert_eq!(weight_value(None), 400.0);
        assert_eq!(weight_value(Some("nonsense")), 400.0);
    }

    #[test]
    fn variable_ranges_cover_their_span() {
        assert!(covers("300 700", 400.0));
        assert!(covers("300 700", 300.0));
        assert!(covers("300 700", 700.0));
        assert!(!covers("300 700", 800.0));
        assert!(!covers("400", 400.0), "a single weight is not a range");
    }

    #[test]
    fn loads_the_compositions_own_fonts() {
        let Some((faces, dir)) = real_faces() else {
            eprintln!("skip: composition not present");
            return;
        };
        let fonts = Fonts::load(&faces, &dir).expect("all faces should load");
        assert!(!fonts.is_empty());

        // The two families the stylesheet actually names.
        let grotesk = fonts
            .resolve("\"Space Grotesk\", sans-serif", 400.0)
            .expect("Space Grotesk should resolve");
        let mono = fonts
            .resolve("\"Space Mono\", monospace", 400.0)
            .expect("Space Mono should resolve");

        // They must be different files — resolving both to one font would
        // silently render the terminal in the wrong face.
        let g_adv = grotesk.advance(grotesk.glyph_id('i'), 100.0).unwrap();
        let g_m = grotesk.advance(grotesk.glyph_id('M'), 100.0).unwrap();
        let m_adv = mono.advance(mono.glyph_id('i'), 100.0).unwrap();
        let m_m = mono.advance(mono.glyph_id('M'), 100.0).unwrap();
        assert!(g_m > g_adv * 1.5, "Space Grotesk is proportional");
        assert!((m_m - m_adv).abs() < 0.01, "Space Mono is monospaced");
    }

    #[test]
    fn bold_and_regular_mono_are_distinct_faces() {
        let Some((faces, dir)) = real_faces() else { return };
        let fonts = Fonts::load(&faces, &dir).unwrap();
        let regular = fonts.resolve("\"Space Mono\"", 400.0).unwrap();
        let bold = fonts.resolve("\"Space Mono\"", 700.0).unwrap();
        // Same metrics (monospaced), but different outlines.
        let r = regular.outline(regular.glyph_id('M')).unwrap();
        let b = bold.outline(bold.glyph_id('M')).unwrap();
        let count = |cs: &[ruler::ttf::Contour]| cs.iter().map(|c| c.len()).sum::<usize>();
        assert_ne!(
            count(&r),
            count(&b),
            "400 and 700 must resolve to different files"
        );
    }

    #[test]
    fn unknown_family_falls_through_the_stack() {
        let Some((faces, dir)) = real_faces() else { return };
        let fonts = Fonts::load(&faces, &dir).unwrap();
        // First name is undeclared, second is real — CSS says the second wins.
        assert!(fonts.resolve("\"Nonexistent\", \"Space Mono\"", 400.0).is_some());
        // Nothing declared and no fallback installed.
        assert!(fonts.resolve("\"Nonexistent\", sans-serif", 400.0).is_none());
    }

    #[test]
    fn a_missing_font_file_is_an_error_not_a_substitution() {
        let faces = vec![FontFace {
            family: "Ghost".into(),
            src: "does/not/exist.woff2".into(),
            weight: "400".into(),
        }];
        let err = match Fonts::load(&faces, Path::new("/tmp")) {
            Err(e) => e,
            Ok(_) => panic!("a missing font file must not load"),
        };
        assert!(err.contains("Ghost"), "error should name the family: {err}");
    }
}
