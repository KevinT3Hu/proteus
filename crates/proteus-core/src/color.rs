//! Colors and the 256-color palette (with OSC 4/10/11/12 overrides).

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// X11 `rgb:rrrr/gggg/bbbb` form used in OSC color query replies.
    pub fn to_x11(self) -> String {
        format!("rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}", self.r, self.r, self.g, self.g, self.b, self.b)
    }

    /// Parse `#rgb`, `#rrggbb`, `#rrrgggbbb`, `#rrrrggggbbbb` or `rgb:r/g/b` (1–4 hex digits per channel).
    pub fn parse(spec: &str) -> Option<Rgb> {
        let spec = spec.trim();
        if let Some(rest) = spec.strip_prefix("rgb:") {
            let mut parts = rest.split('/');
            let r = parse_scaled(parts.next()?)?;
            let g = parse_scaled(parts.next()?)?;
            let b = parse_scaled(parts.next()?)?;
            if parts.next().is_some() {
                return None;
            }
            return Some(Rgb::new(r, g, b));
        }
        let hex = spec.strip_prefix('#')?;
        if hex.is_empty() || hex.len() % 3 != 0 || hex.len() > 12 {
            return None;
        }
        let n = hex.len() / 3;
        let r = parse_scaled(&hex[..n])?;
        let g = parse_scaled(&hex[n..2 * n])?;
        let b = parse_scaled(&hex[2 * n..])?;
        Some(Rgb::new(r, g, b))
    }
}

/// Parse 1–4 hex digits and scale to 8 bits.
fn parse_scaled(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    let max = (1u32 << (4 * s.len())) - 1;
    Some(((v * 255 + max / 2) / max) as u8)
}

/// A color as stored in a cell style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    /// The default foreground or background (depending on where it is used).
    #[default]
    Default,
    /// Palette index 0–255 (0–15 are the named ANSI colors).
    Indexed(u8),
    /// Direct true color.
    Rgb(Rgb),
}

/// Terminal palette: 256 indexed colors plus special colors. The base is the
/// theme; OSC sequences apply overrides on top that can be reset individually.
#[derive(Debug, Clone)]
pub struct Palette {
    base: [Rgb; 256],
    base_fg: Rgb,
    base_bg: Rgb,
    base_cursor: Option<Rgb>,
    overrides: Box<[Option<Rgb>; 256]>,
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    cursor: Option<Rgb>,
}

impl Default for Palette {
    fn default() -> Self {
        Self::new(default_ansi16(), Rgb::new(0xd8, 0xd8, 0xd8), Rgb::new(0x18, 0x18, 0x18), None)
    }
}

impl Palette {
    /// Build a palette from the 16 ANSI theme colors; 16–255 use the xterm cube and gray ramp.
    pub fn new(ansi: [Rgb; 16], fg: Rgb, bg: Rgb, cursor: Option<Rgb>) -> Self {
        let mut base = [Rgb::default(); 256];
        base[..16].copy_from_slice(&ansi);
        for i in 0..216u16 {
            let level = |v: u16| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
            base[16 + i as usize] = Rgb::new(level(i / 36), level((i / 6) % 6), level(i % 6));
        }
        for i in 0..24u16 {
            let v = (8 + i * 10) as u8;
            base[232 + i as usize] = Rgb::new(v, v, v);
        }
        Self {
            base,
            base_fg: fg,
            base_bg: bg,
            base_cursor: cursor,
            overrides: Box::new([None; 256]),
            fg: None,
            bg: None,
            cursor: None,
        }
    }

    pub fn indexed(&self, i: u8) -> Rgb {
        self.overrides[i as usize].unwrap_or(self.base[i as usize])
    }
    pub fn foreground(&self) -> Rgb {
        self.fg.unwrap_or(self.base_fg)
    }
    pub fn background(&self) -> Rgb {
        self.bg.unwrap_or(self.base_bg)
    }
    /// `None` means "use the cell's foreground/inverse".
    pub fn cursor(&self) -> Option<Rgb> {
        self.cursor.or(self.base_cursor)
    }

    /// Resolve a cell color; `fg` selects which default to use.
    pub fn resolve(&self, c: Color, fg: bool) -> Rgb {
        match c {
            Color::Default if fg => self.foreground(),
            Color::Default => self.background(),
            Color::Indexed(i) => self.indexed(i),
            Color::Rgb(rgb) => rgb,
        }
    }

    pub fn set_indexed(&mut self, i: u8, c: Rgb) {
        self.overrides[i as usize] = Some(c);
    }
    pub fn reset_indexed(&mut self, i: Option<u8>) {
        match i {
            Some(i) => self.overrides[i as usize] = None,
            None => *self.overrides = [None; 256],
        }
    }
    pub fn set_special(&mut self, which: Special, c: Option<Rgb>) {
        match which {
            Special::Foreground => self.fg = c,
            Special::Background => self.bg = c,
            Special::Cursor => self.cursor = c,
        }
    }
    pub fn special(&self, which: Special) -> Rgb {
        match which {
            Special::Foreground => self.foreground(),
            Special::Background => self.background(),
            Special::Cursor => self.cursor().unwrap_or_else(|| self.foreground()),
        }
    }
    /// Drop all OSC overrides (RIS).
    pub fn reset_all(&mut self) {
        self.reset_indexed(None);
        self.fg = None;
        self.bg = None;
        self.cursor = None;
    }
}

/// Special colors addressable via OSC 10/11/12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    Foreground,
    Background,
    Cursor,
}

fn default_ansi16() -> [Rgb; 16] {
    [
        Rgb::new(0x18, 0x18, 0x18),
        Rgb::new(0xcc, 0x44, 0x44),
        Rgb::new(0x55, 0xaa, 0x55),
        Rgb::new(0xcc, 0xaa, 0x44),
        Rgb::new(0x44, 0x77, 0xcc),
        Rgb::new(0xaa, 0x55, 0xbb),
        Rgb::new(0x44, 0xaa, 0xaa),
        Rgb::new(0xd8, 0xd8, 0xd8),
        Rgb::new(0x58, 0x58, 0x58),
        Rgb::new(0xff, 0x66, 0x66),
        Rgb::new(0x77, 0xdd, 0x77),
        Rgb::new(0xff, 0xdd, 0x66),
        Rgb::new(0x66, 0x99, 0xff),
        Rgb::new(0xdd, 0x77, 0xee),
        Rgb::new(0x66, 0xdd, 0xdd),
        Rgb::new(0xf8, 0xf8, 0xf8),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specs() {
        assert_eq!(Rgb::parse("#ff8000"), Some(Rgb::new(255, 128, 0)));
        assert_eq!(Rgb::parse("#f80"), Some(Rgb::new(255, 136, 0)));
        assert_eq!(Rgb::parse("rgb:ffff/8080/0000"), Some(Rgb::new(255, 128, 0)));
        assert_eq!(Rgb::parse("rgb:f/8/0"), Some(Rgb::new(255, 136, 0)));
        assert_eq!(Rgb::parse("nonsense"), None);
        assert_eq!(Rgb::new(255, 128, 0).to_x11(), "rgb:ffff/8080/0000");
    }

    #[test]
    fn cube_and_ramp() {
        let p = Palette::default();
        assert_eq!(p.indexed(16), Rgb::new(0, 0, 0));
        assert_eq!(p.indexed(231), Rgb::new(255, 255, 255));
        assert_eq!(p.indexed(232), Rgb::new(8, 8, 8));
        assert_eq!(p.indexed(255), Rgb::new(238, 238, 238));
    }
}
