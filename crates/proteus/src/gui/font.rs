//! Fonts: fontconfig discovery and per-character fallback, swash metrics and
//! rasterization.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_int};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;

use fontconfig_sys as fc;
use fontconfig_sys::constants::{
    FC_CHARSET, FC_FAMILY, FC_FILE, FC_INDEX, FC_SLANT, FC_SLANT_ITALIC, FC_SLANT_ROMAN, FC_SPACING, FC_WEIGHT,
    FC_WEIGHT_BOLD, FC_WEIGHT_REGULAR, FC_WEIGHT_SEMIBOLD, FC_MONO, FC_COLOR,
};
use swash::scale::image::Content;
use std::rc::Rc;

use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::text::{Codepoint, Script};
use swash::zeno::{Angle, Format, Transform};
use swash::{CacheKey, FontRef};

/// Regular, bold, italic, bold italic.
pub const STYLES: usize = 4;
pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;

/// Cell geometry in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub cell_w: u32,
    pub cell_h: u32,
    /// Baseline, from the top of the cell.
    pub baseline: i32,
    pub underline_y: i32,
    pub underline_h: u32,
    pub strike_y: i32,
}

struct Face {
    data: Arc<Vec<u8>>,
    offset: u32,
    key: CacheKey,
    synth_bold: bool,
    synth_italic: bool,
}

impl Face {
    fn font(&self) -> FontRef<'_> {
        FontRef { data: &self.data, offset: self.offset, key: self.key }
    }
}

/// A rasterized glyph. `left`/`top` are relative to the pen position on the
/// baseline (`top` is upwards).
pub struct Raster {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// RGBA if true, else 8-bit coverage.
    pub color: bool,
    pub data: Vec<u8>,
}

/// Identifies a glyph of a loaded face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphRef {
    pub face: u16,
    pub glyph: u16,
    /// Style the glyph was requested in (synthesis depends on it).
    pub style: u8,
}

/// One glyph of a shaped run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    /// Byte offset (within the shaped text) of the cluster the glyph
    /// belongs to; the caller maps it to a cell.
    pub src: u32,
    pub glyph: GlyphRef,
    /// Offset from the left edge of that cluster's cell and from the
    /// baseline, in pixels (`y` is up).
    pub x: f32,
    pub y: f32,
}

/// Bound on cached shaped runs before the cache is reset.
const SHAPE_CACHE: usize = 8192;

struct FcConfig(*mut fc::FcConfig);

/// Result of a fontconfig match.
struct Match {
    family: String,
    file: PathBuf,
    index: u32,
    weight: c_int,
    slant: c_int,
}

impl FcConfig {
    fn new() -> Option<Self> {
        // SAFETY: plain library initialization; returns null on failure.
        let cfg = unsafe { fc::FcInitLoadConfigAndFonts() };
        (!cfg.is_null()).then_some(Self(cfg))
    }

    /// Best match for `family` with the given weight/slant, optionally
    /// requiring a character.
    fn find(&self, family: &str, weight: c_int, slant: c_int, chars: &[char], color: bool) -> Option<Match> {
        let family = CString::new(family).ok()?;
        // SAFETY: every pointer passed to fontconfig is valid for the call;
        // objects we create are destroyed before returning.
        unsafe {
            let pat = fc::FcPatternCreate();
            if pat.is_null() {
                return None;
            }
            fc::FcPatternAddString(pat, FC_FAMILY.as_ptr(), family.as_ptr().cast());
            fc::FcPatternAddInteger(pat, FC_WEIGHT.as_ptr(), weight);
            fc::FcPatternAddInteger(pat, FC_SLANT.as_ptr(), slant);
            if chars.is_empty() && !color {
                fc::FcPatternAddInteger(pat, FC_SPACING.as_ptr(), FC_MONO);
            }
            if color {
                fc::FcPatternAddBool(pat, FC_COLOR.as_ptr(), 1);
            }
            if !chars.is_empty() {
                let cs = fc::FcCharSetCreate();
                for &c in chars {
                    fc::FcCharSetAddChar(cs, c as u32);
                }
                fc::FcPatternAddCharSet(pat, FC_CHARSET.as_ptr(), cs);
                fc::FcCharSetDestroy(cs);
            }
            fc::FcConfigSubstitute(self.0, pat, fc::FcMatchPattern);
            fc::FcDefaultSubstitute(pat);
            let mut res: fc::FcResult = 0;
            let m = fc::FcFontMatch(self.0, pat, &mut res);
            fc::FcPatternDestroy(pat);
            if m.is_null() {
                return None;
            }
            let mut file: *mut fc::FcChar8 = std::ptr::null_mut();
            let mut fam: *mut fc::FcChar8 = std::ptr::null_mut();
            let family = if fc::FcPatternGetString(m, FC_FAMILY.as_ptr(), 0, &mut fam) == fc::FcResultMatch {
                CStr::from_ptr(fam.cast()).to_string_lossy().into_owned()
            } else {
                String::new()
            };
            let mut index: c_int = 0;
            let (mut w, mut s) = (FC_WEIGHT_REGULAR, FC_SLANT_ROMAN);
            let ok = fc::FcPatternGetString(m, FC_FILE.as_ptr(), 0, &mut file) == fc::FcResultMatch;
            fc::FcPatternGetInteger(m, FC_INDEX.as_ptr(), 0, &mut index);
            fc::FcPatternGetInteger(m, FC_WEIGHT.as_ptr(), 0, &mut w);
            fc::FcPatternGetInteger(m, FC_SLANT.as_ptr(), 0, &mut s);
            let result = ok.then(|| Match {
                family,
                file: PathBuf::from(std::ffi::OsStr::from_bytes(CStr::from_ptr(file.cast()).to_bytes())),
                index: index.max(0) as u32,
                weight: w,
                slant: s,
            });
            fc::FcPatternDestroy(m);
            result
        }
    }
}

impl Drop for FcConfig {
    fn drop(&mut self) {
        // SAFETY: created by FcInitLoadConfigAndFonts and not shared.
        unsafe { fc::FcConfigDestroy(self.0) }
    }
}

pub struct Fonts {
    fc: Option<FcConfig>,
    family: String,
    /// Font size in points; pixels = points * 96/72 * scale.
    pub size_pt: f32,
    scale: f64,
    px: f32,
    faces: Vec<Face>,
    /// Face index per style for the primary family.
    primary: [usize; STYLES],
    files: HashMap<(PathBuf, u32), Arc<Vec<u8>>>,
    /// Fallback face per (char, style).
    fallback: HashMap<(char, u8), Option<usize>>,
    glyphs: HashMap<(char, u8), Option<GlyphRef>>,
    ctx: ScaleContext,
    shaper: ShapeContext,
    /// Shaped runs per style.
    shaped: [HashMap<String, Rc<[ShapedGlyph]>>; STYLES],
    /// Shaped grapheme clusters (combining sequences, emoji) per style.
    clusters: [HashMap<String, Rc<[ShapedGlyph]>>; STYLES],
    /// Color emoji face, looked up on first use.
    emoji: Option<Option<usize>>,
    metrics: Metrics,
}

/// Characters that never get a glyph of their own.
fn invisible(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{fe00}'..='\u{fe0f}' | '\u{2060}'..='\u{2064}')
}

/// Whether a cluster should use a color emoji font: explicit emoji
/// presentation or sequences, or a wide pictograph without VS15.
pub fn emoji_presentation(text: &str, wide: bool) -> bool {
    if text.contains('\u{fe0e}') {
        return false;
    }
    let mut chars = text.chars();
    let base = chars.next().unwrap_or(' ');
    let marker = |c: char| {
        matches!(c, '\u{fe0f}' | '\u{200d}' | '\u{20e3}' | '\u{1f3fb}'..='\u{1f3ff}' | '\u{e0020}'..='\u{e007f}')
    };
    matches!(base, '\u{1f1e6}'..='\u{1f1ff}')
        || chars.any(marker)
        || (wide && base.is_extended_pictographic())
}

/// Script for shaping: the first one that is not Common/Inherited.
fn script_of(text: &str) -> Script {
    text.chars()
        .map(Codepoint::script)
        .find(|s| !matches!(s, Script::Common | Script::Inherited | Script::Unknown))
        .unwrap_or(Script::Latin)
}

/// Opaque id of a text's script (see [`script_of`]), `None` if it only has
/// Common/Inherited characters. Runs break where this changes.
pub fn script_tag(text: &str) -> Option<u32> {
    text.chars()
        .map(Codepoint::script)
        .find(|s| !matches!(s, Script::Common | Script::Inherited | Script::Unknown))
        .map(|s| s as u32)
}

impl Fonts {
    pub fn new(family: &str, size_pt: f32, scale: f64) -> Result<Fonts, String> {
        let mut f = Fonts {
            fc: FcConfig::new(),
            family: family.to_string(),
            size_pt,
            scale,
            px: 0.0,
            faces: Vec::new(),
            primary: [0; STYLES],
            files: HashMap::new(),
            fallback: HashMap::new(),
            glyphs: HashMap::new(),
            ctx: ScaleContext::new(),
            shaper: ShapeContext::new(),
            shaped: Default::default(),
            clusters: Default::default(),
            emoji: None,
            metrics: Metrics { cell_w: 1, cell_h: 1, baseline: 0, underline_y: 0, underline_h: 1, strike_y: 0 },
        };
        f.load_primary()?;
        f.update_metrics();
        Ok(f)
    }

    fn load_face(&mut self, m: &Match, synth_bold: bool, synth_italic: bool) -> Option<usize> {
        let key = (m.file.clone(), m.index);
        let data = match self.files.get(&key) {
            Some(d) => d.clone(),
            None => {
                let d = Arc::new(std::fs::read(&m.file).ok()?);
                self.files.insert(key, d.clone());
                d
            }
        };
        let font = FontRef::from_index(&data, m.index as usize)?;
        let (offset, key) = (font.offset, font.key);
        self.faces.push(Face { data, offset, key, synth_bold, synth_italic });
        Some(self.faces.len() - 1)
    }

    fn load_primary(&mut self) -> Result<(), String> {
        let fc = self.fc.as_ref().ok_or("fontconfig initialization failed")?;
        let mut matches = Vec::new();
        for style in 0..STYLES as u8 {
            let w = if style & BOLD != 0 { FC_WEIGHT_BOLD } else { FC_WEIGHT_REGULAR };
            let s = if style & ITALIC != 0 { FC_SLANT_ITALIC } else { FC_SLANT_ROMAN };
            matches.push(fc.find(&self.family, w, s, &[], false));
        }
        let regular = matches[0].take().ok_or_else(|| format!("no font found for `{}`", self.family))?;
        self.primary[0] = self.load_face(&regular, false, false).ok_or_else(|| {
            format!("cannot load font {}", regular.file.display())
        })?;
        for (style, m) in matches.into_iter().enumerate().skip(1) {
            let want_bold = style as u8 & BOLD != 0;
            let want_italic = style as u8 & ITALIC != 0;
            // Only accept a style variant of the same family; else synthesize.
            let m = m.filter(|m| m.family == regular.family);
            let m = m.unwrap_or(Match {
                family: regular.family.clone(),
                file: regular.file.clone(),
                index: regular.index,
                weight: regular.weight,
                slant: regular.slant,
            });
            let synth_bold = want_bold && m.weight < FC_WEIGHT_SEMIBOLD;
            let synth_italic = want_italic && m.slant == FC_SLANT_ROMAN;
            self.primary[style] = self.load_face(&m, synth_bold, synth_italic).unwrap_or(self.primary[0]);
        }
        Ok(())
    }

    fn update_metrics(&mut self) {
        self.px = (self.size_pt as f64 * 96.0 / 72.0 * self.scale) as f32;
        let face = &self.faces[self.primary[0]];
        let font = face.font();
        let m = font.metrics(&[]).scale(self.px);
        let gm = font.glyph_metrics(&[]).scale(self.px);
        let adv = gm.advance_width(font.charmap().map('M'));
        let adv = if adv > 0.0 { adv } else { m.average_width.max(self.px / 2.0) };
        let cell_w = adv.round().max(1.0) as u32;
        let cell_h = (m.ascent + m.descent + m.leading).ceil().max(1.0) as u32;
        let baseline = (m.ascent + m.leading / 2.0).round() as i32;
        let underline_h = m.stroke_size.round().max(1.0) as u32;
        let underline_y = (baseline as f32 - m.underline_offset).round() as i32;
        let strike = if m.strikeout_offset > 0.0 { m.strikeout_offset } else { m.x_height / 2.0 };
        let strike_y = (baseline as f32 - strike).round() as i32;
        self.metrics = Metrics {
            cell_w,
            cell_h,
            baseline,
            underline_y: underline_y.min(cell_h as i32 - underline_h as i32),
            underline_h,
            strike_y,
        };
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// Change scale factor or size; returns true if metrics changed.
    pub fn set_size(&mut self, size_pt: f32, scale: f64) -> bool {
        if (size_pt - self.size_pt).abs() < f32::EPSILON && (scale - self.scale).abs() < 1e-9 {
            return false;
        }
        self.size_pt = size_pt;
        self.scale = scale;
        self.update_metrics();
        self.shaped.iter_mut().chain(&mut self.clusters).for_each(HashMap::clear);
        true
    }

    /// Whether the primary face for `style` covers every visible char of a
    /// cell's text, i.e. whether the cell can take part in a shaped run.
    pub fn in_primary(&self, text: &str, style: u8) -> bool {
        let font = self.faces[self.primary[style as usize]].font();
        let cmap = font.charmap();
        let mut any = false;
        for c in text.chars().filter(|&c| !invisible(c)) {
            if cmap.map(c) == 0 {
                return false;
            }
            any = true;
        }
        any
    }

    /// Shape `text` with one face; glyph positions are relative to the
    /// start of their (shaper) cluster.
    fn shape_with_face(&mut self, face_idx: usize, text: &str, style: u8) -> Vec<ShapedGlyph> {
        let face = &self.faces[face_idx];
        let font = face.font();
        // Variation selectors the face does not map would shape as .notdef
        // and block sequences (keycaps, VS16 emoji): drop them, remembering
        // each kept byte's original offset.
        let cmap = font.charmap();
        let mut stripped = String::with_capacity(text.len());
        let mut orig = Vec::with_capacity(text.len() + 1);
        for (b, c) in text.char_indices() {
            if matches!(c, '\u{fe00}'..='\u{fe0f}') && cmap.map(c) == 0 {
                continue;
            }
            stripped.push(c);
            orig.extend((0..c.len_utf8() as u32).map(|i| b as u32 + i));
        }
        orig.push(text.len() as u32);
        let mut shaper = self.shaper.builder(font).script(script_of(text)).size(self.px).build();
        shaper.add_str(&stripped);
        let mut out = Vec::with_capacity(text.len());
        let mut pen = 0.0f32;
        shaper.shape_with(|cluster| {
            let start = pen;
            let src = orig[(cluster.source.start as usize).min(orig.len() - 1)];
            for g in cluster.glyphs {
                out.push(ShapedGlyph {
                    src,
                    glyph: GlyphRef { face: face_idx as u16, glyph: g.id, style },
                    x: pen - start + g.x,
                    y: g.y,
                });
                pen += g.advance;
            }
        });
        out
    }

    /// Shape a run of single-cell texts (each covered by the primary face,
    /// see [`Fonts::in_primary`]) so the font's ligatures, contextual
    /// alternates and mark positioning apply. Glyphs are positioned relative
    /// to the cell their cluster starts at, so the grid never drifts.
    pub fn shape(&mut self, text: &str, style: u8) -> Rc<[ShapedGlyph]> {
        if let Some(r) = self.shaped[style as usize].get(text) {
            return r.clone();
        }
        let mut out = self.shape_with_face(self.primary[style as usize], text, style);
        out.retain(|g| g.glyph.glyph != 0);
        let out: Rc<[ShapedGlyph]> = out.into();
        let cache = &mut self.shaped[style as usize];
        if cache.len() >= SHAPE_CACHE {
            cache.clear();
        }
        cache.insert(text.to_string(), out.clone());
        out
    }

    /// Shape one grapheme cluster that cannot join a run: combining
    /// sequences outside the primary face, and emoji (ZWJ sequences, flags,
    /// skin tones, keycaps, VS16), which prefer a color emoji font. All
    /// glyphs have `src == 0`.
    pub fn shape_cluster(&mut self, text: &str, style: u8, wide: bool) -> Rc<[ShapedGlyph]> {
        if let Some(r) = self.clusters[style as usize].get(text) {
            return r.clone();
        }
        let out: Rc<[ShapedGlyph]> = self.shape_cluster_uncached(text, style, wide).into();
        let cache = &mut self.clusters[style as usize];
        if cache.len() >= SHAPE_CACHE {
            cache.clear();
        }
        cache.insert(text.to_string(), out.clone());
        out
    }

    fn shape_cluster_uncached(&mut self, text: &str, style: u8, wide: bool) -> Vec<ShapedGlyph> {
        let visible: Vec<char> = text.chars().filter(|&c| !invisible(c)).collect();
        let Some(&base) = visible.first() else { return Vec::new() };
        let covers = |f: &Fonts, face: usize| {
            let font = f.faces[face].font();
            let cmap = font.charmap();
            visible.iter().all(|&c| cmap.map(c) != 0)
        };
        let mut face = None;
        if emoji_presentation(text, wide)
            && let Some(e) = self.emoji_face()
            && self.faces[e].font().charmap().map(base) != 0
        {
            face = Some(e);
        }
        if face.is_none() {
            face = [self.primary[style as usize], self.primary[0]].into_iter().find(|&f| covers(self, f));
        }
        if face.is_none() {
            face = self.fallback_for(&visible, style).filter(|&f| covers(self, f));
        }
        if let Some(f) = face {
            let out = self.shape_with_face(f, text, style);
            if !out.is_empty() && out.iter().all(|g| g.glyph.glyph != 0) {
                // A cluster is one cell: everything is relative to its start.
                return out.into_iter().map(|g| ShapedGlyph { src: 0, ..g }).collect();
            }
        }
        // No single face covers it: overlay each char's own glyph.
        visible
            .iter()
            .filter_map(|&c| self.glyph(c, style))
            .map(|glyph| ShapedGlyph { src: 0, glyph, x: 0.0, y: 0.0 })
            .collect()
    }

    fn emoji_face(&mut self) -> Option<usize> {
        if let Some(f) = self.emoji {
            return f;
        }
        let m = self.fc.as_ref().and_then(|fc| fc.find("emoji", FC_WEIGHT_REGULAR, FC_SLANT_ROMAN, &['\u{1f600}'], true));
        let f = m.and_then(|m| self.face_for(&m, false, false));
        self.emoji = Some(f);
        f
    }

    /// A face of the configured family's fallback chain covering all `chars`.
    fn fallback_for(&mut self, chars: &[char], style: u8) -> Option<usize> {
        let w = if style & BOLD != 0 { FC_WEIGHT_BOLD } else { FC_WEIGHT_REGULAR };
        let s = if style & ITALIC != 0 { FC_SLANT_ITALIC } else { FC_SLANT_ROMAN };
        let m = self.fc.as_ref()?.find(&self.family, w, s, chars, false)?;
        let synth_bold = style & BOLD != 0 && m.weight < FC_WEIGHT_SEMIBOLD;
        let synth_italic = style & ITALIC != 0 && m.slant == FC_SLANT_ROMAN;
        self.face_for(&m, synth_bold, synth_italic)
    }

    /// Reuse an already loaded face for the same file and synthesis, or load it.
    fn face_for(&mut self, m: &Match, synth_bold: bool, synth_italic: bool) -> Option<usize> {
        if let Some(data) = self.files.get(&(m.file.clone(), m.index))
            && let Some(i) = self.faces.iter().position(|f| {
                Arc::ptr_eq(&f.data, data) && f.synth_bold == synth_bold && f.synth_italic == synth_italic
            })
        {
            return Some(i);
        }
        self.load_face(m, synth_bold, synth_italic)
    }

    /// Glyph for a character in a style, falling back through fontconfig.
    pub fn glyph(&mut self, c: char, style: u8) -> Option<GlyphRef> {
        if invisible(c) {
            return None;
        }
        if let Some(&g) = self.glyphs.get(&(c, style)) {
            return g;
        }
        let g = self.find_glyph(c, style);
        self.glyphs.insert((c, style), g);
        g
    }

    fn find_glyph(&mut self, c: char, style: u8) -> Option<GlyphRef> {
        let mk = |face: usize, glyph: u16| GlyphRef { face: face as u16, glyph, style };
        for face in [self.primary[style as usize], self.primary[0]] {
            let gid = self.faces[face].font().charmap().map(c);
            if gid != 0 {
                return Some(mk(face, gid));
            }
        }
        let face = match self.fallback.get(&(c, style)) {
            Some(&f) => f,
            None => {
                let f = self.find_fallback(c, style);
                self.fallback.insert((c, style), f);
                f
            }
        }?;
        let gid = self.faces[face].font().charmap().map(c);
        (gid != 0).then(|| mk(face, gid))
    }

    fn find_fallback(&mut self, c: char, style: u8) -> Option<usize> {
        let i = self.fallback_for(&[c], style)?;
        (self.faces[i].font().charmap().map(c) != 0).then_some(i)
    }

    /// Rasterize a glyph at the current size.
    pub fn rasterize(&mut self, g: GlyphRef) -> Option<Raster> {
        let face = &self.faces[g.face as usize];
        let font = face.font();
        let mut scaler = self.ctx.builder(font).size(self.px).hint(true).build();
        let sources = [Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline];
        let mut render = Render::new(&sources);
        render.format(Format::Alpha);
        if face.synth_bold {
            render.embolden((self.px / 28.0).max(0.5));
        }
        if face.synth_italic {
            render.transform(Some(Transform::skew(Angle::from_degrees(12.0), Angle::from_degrees(0.0))));
        }
        let img = render.render(&mut scaler, g.glyph)?;
        if img.placement.width == 0 || img.placement.height == 0 {
            return None;
        }
        Some(Raster {
            left: img.placement.left,
            top: img.placement.top,
            width: img.placement.width,
            height: img.placement.height,
            color: img.content == Content::Color,
            data: img.data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monospace_metrics_and_fallback() {
        let Ok(mut f) = Fonts::new("monospace", 11.0, 1.0) else {
            eprintln!("skipping: no fontconfig monospace font");
            return;
        };
        let m = f.metrics();
        assert!(m.cell_w >= 4 && m.cell_h > m.cell_w, "{m:?}");
        assert!(m.baseline > 0 && m.baseline < m.cell_h as i32);
        let a = f.glyph('a', 0).expect("glyph for a");
        let r = f.rasterize(a).expect("raster for a");
        assert!(!r.color && r.data.len() == (r.width * r.height) as usize);
        assert!(f.glyph('\u{fe0f}', 0).is_none());
        // Bold always resolves (real or synthesized).
        assert!(f.glyph('a', BOLD).is_some());
        assert!(f.set_size(22.0, 1.0));
        assert!(f.metrics().cell_h > m.cell_h);
    }

    #[test]
    fn shaping_keeps_cells() {
        let Ok(mut f) = Fonts::new("monospace", 11.0, 1.0) else {
            eprintln!("skipping: no fontconfig monospace font");
            return;
        };
        let g = f.shape("abc", 0);
        assert_eq!(g.iter().map(|g| g.src).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(g.iter().all(|g| g.x.abs() < 1.0 && g.y.abs() < 1.0), "{g:?}");
        assert_eq!(g[0].glyph, f.glyph('a', 0).unwrap());
        // Byte offsets survive non-ASCII text.
        assert!(f.in_primary("é", 0));
        assert_eq!(f.shape("éa", 0).last().unwrap().src, 2);
    }

    #[test]
    fn combining_marks_are_positioned() {
        let Ok(mut f) = Fonts::new("DejaVu Sans Mono", 11.0, 1.0) else { return };
        if !f.in_primary("a\u{0323}\u{0301}", 0) {
            eprintln!("skipping: font lacks combining marks");
            return;
        }
        // Base + below + above mark, then `b` in the next cell.
        let text = "a\u{0323}\u{0301}b";
        let g = f.shape(text, 0);
        let b = g.iter().filter(|g| g.src == 5).count();
        assert_eq!(b, 1, "{g:?}");
        let marks: Vec<_> = g.iter().filter(|g| g.src == 0).collect();
        assert!(!marks.is_empty() && marks.len() <= 3, "{g:?}");
        // Marks stay within their cell.
        let cw = f.metrics().cell_w as f32;
        assert!(marks.iter().all(|g| g.x > -cw && g.x < cw), "{g:?}");
    }

    #[test]
    fn emoji_sequences_compose() {
        let Ok(mut f) = Fonts::new("monospace", 11.0, 1.0) else { return };
        let Some(e) = f.emoji_face() else {
            eprintln!("skipping: no color emoji font");
            return;
        };
        for seq in [
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", // family (ZWJ)
            "\u{1f1e8}\u{1f1f3}",                         // flag
            "\u{1f44d}\u{1f3fd}",                         // skin tone
            "1\u{fe0f}\u{20e3}",                          // keycap
            "\u{2764}\u{fe0f}",                           // VS16
            "\u{1f600}",
        ] {
            assert!(emoji_presentation(seq, true), "{seq}");
            let g = f.shape_cluster(seq, 0, true);
            assert_eq!(g.len(), 1, "{seq}: {g:?}");
            assert_eq!(g[0].glyph.face as usize, e, "{seq}");
            assert!(f.rasterize(g[0].glyph).is_some_and(|r| r.color), "{seq}");
        }
        // Text presentation and plain digits stay with the text font.
        assert!(!emoji_presentation("\u{2764}\u{fe0e}", true));
        assert!(!emoji_presentation("1", false));
        assert!(!emoji_presentation("\u{2764}", false));
    }

    #[test]
    fn ligature_font_substitutes() {
        let Ok(mut f) = Fonts::new("FiraCode Nerd Font Mono", 11.0, 1.0) else { return };
        if !f.faces[f.primary[0]].font().localized_strings().any(|s| s.to_string().contains("Fira")) {
            eprintln!("skipping: Fira Code not installed");
            return;
        }
        let plain = [f.glyph('-', 0).unwrap(), f.glyph('>', 0).unwrap()];
        let shaped: Vec<_> = f.shape("->", 0).iter().map(|g| g.glyph).collect();
        assert_ne!(shaped, plain, "calt should replace `->`");
        assert!(f.shape("->", 0).iter().all(|g| g.src < 2));
    }
}
