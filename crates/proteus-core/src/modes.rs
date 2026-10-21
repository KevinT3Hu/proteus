//! Terminal modes (SM/RM, DECSET/DECRST) and cursor shape.

use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TermMode: u32 {
        /// DECCKM: cursor keys send SS3 sequences.
        const APP_CURSOR = 1 << 0;
        /// DECKPAM: keypad sends application sequences.
        const APP_KEYPAD = 1 << 1;
        /// DECOM: cursor addressing relative to the scroll region.
        const ORIGIN = 1 << 2;
        /// DECAWM.
        const AUTOWRAP = 1 << 3;
        /// DECTCEM.
        const SHOW_CURSOR = 1 << 4;
        /// IRM (SM 4).
        const INSERT = 1 << 5;
        /// LNM (SM 20): LF also does CR; Enter sends CR LF.
        const LINE_FEED_NEW_LINE = 1 << 6;
        /// Alternate screen is active.
        const ALT_SCREEN = 1 << 7;
        /// 1004.
        const FOCUS_EVENTS = 1 << 8;
        /// 2004.
        const BRACKETED_PASTE = 1 << 9;
        /// 2026.
        const SYNC_OUTPUT = 1 << 10;
        /// 2027: grapheme clustering.
        const GRAPHEME_CLUSTERING = 1 << 11;
        /// 1007: wheel sends arrow keys in the alternate screen.
        const ALTERNATE_SCROLL = 1 << 12;
        /// 9: X10 mouse (press only).
        const MOUSE_X10 = 1 << 13;
        /// 1000: press and release.
        const MOUSE_NORMAL = 1 << 14;
        /// 1002: + motion while a button is held.
        const MOUSE_BUTTON_MOTION = 1 << 15;
        /// 1003: + any motion.
        const MOUSE_ANY_MOTION = 1 << 16;
        /// 1006: SGR mouse encoding.
        const MOUSE_SGR = 1 << 17;
        /// 12: cursor blinking (att610).
        const CURSOR_BLINK = 1 << 18;
        /// 2031: report color-scheme changes.
        const COLOR_SCHEME_UPDATES = 1 << 19;
        /// 2048: in-band window resize notifications.
        const IN_BAND_RESIZE = 1 << 20;
        /// 1016: SGR-pixel mouse coordinates.
        const MOUSE_SGR_PIXELS = 1 << 21;
        /// DECSCNM (5): reverse video for the whole screen.
        const REVERSE_VIDEO = 1 << 22;
        /// DECSDM (80): Sixel display mode — images at the top-left, no scrolling.
        const SIXEL_DISPLAY = 1 << 23;
        /// 8452: after a Sixel image the cursor goes to its right instead of below.
        const SIXEL_CURSOR_RIGHT = 1 << 24;

        const MOUSE_ANY = Self::MOUSE_X10.bits() | Self::MOUSE_NORMAL.bits()
            | Self::MOUSE_BUTTON_MOTION.bits() | Self::MOUSE_ANY_MOTION.bits();
    }
}

impl Default for TermMode {
    fn default() -> Self {
        TermMode::AUTOWRAP | TermMode::SHOW_CURSOR | TermMode::ALTERNATE_SCROLL
    }
}

/// DECSCUSR cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

bitflags! {
    /// Kitty keyboard protocol progressive-enhancement flags.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct KeyboardFlags: u8 {
        const DISAMBIGUATE = 1;
        const REPORT_EVENTS = 2;
        const REPORT_ALTERNATES = 4;
        const REPORT_ALL_AS_ESCAPES = 8;
        const REPORT_TEXT = 16;
    }
}
