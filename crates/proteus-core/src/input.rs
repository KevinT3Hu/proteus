//! Encoding of keyboard, mouse, focus and paste input into bytes for the PTY.

use bitflags::bitflags;

use crate::modes::{KeyboardFlags, TermMode};

bitflags! {
    /// Modifier bits as defined by the kitty keyboard protocol.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Mods: u8 {
        const SHIFT = 1;
        const ALT = 2;
        const CTRL = 4;
        const SUPER = 8;
        const HYPER = 16;
        const META = 32;
        const CAPS_LOCK = 64;
        const NUM_LOCK = 128;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A text key, identified by its *unshifted* character (e.g. `a`, `1`, `[`).
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1–F35.
    F(u8),
    /// A keypad key (see [`Keypad`]).
    Keypad(Keypad),
    /// A key that only the kitty protocol can report, by its kitty code:
    /// modifiers ([`code`] `LEFT_SHIFT`...), locks, Print, Pause, Menu, media keys.
    Code(u32),
}

/// Keypad keys. Without the kitty protocol they act like their main-block
/// counterparts (digits and operators send their text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keypad {
    Digit(u8),
    Decimal,
    Divide,
    Multiply,
    Subtract,
    Add,
    Enter,
    Equal,
    Separator,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Insert,
    Delete,
    Begin,
}

impl Keypad {
    /// Kitty key code (KP_0 = 57399 ... KP_BEGIN = 57427).
    fn code(self) -> u32 {
        57399
            + match self {
                Keypad::Digit(d) => d.min(9) as u32,
                Keypad::Decimal => 10,
                Keypad::Divide => 11,
                Keypad::Multiply => 12,
                Keypad::Subtract => 13,
                Keypad::Add => 14,
                Keypad::Enter => 15,
                Keypad::Equal => 16,
                Keypad::Separator => 17,
                Keypad::Left => 18,
                Keypad::Right => 19,
                Keypad::Up => 20,
                Keypad::Down => 21,
                Keypad::PageUp => 22,
                Keypad::PageDown => 23,
                Keypad::Home => 24,
                Keypad::End => 25,
                Keypad::Insert => 26,
                Keypad::Delete => 27,
                Keypad::Begin => 28,
            }
    }

    /// The main-block key it stands for, given the text it types: what
    /// key bindings and prompts see.
    pub fn main_key(self, text: &str) -> Key {
        match (self.legacy(), self) {
            (Some(k), _) => k,
            (None, Keypad::Begin) => Key::Keypad(self),
            (None, _) => text.chars().next().filter(|c| !c.is_control()).map_or(Key::Keypad(self), Key::Char),
        }
    }

    /// The main-block key it stands for in legacy encoding (`None`: its text).
    fn legacy(self) -> Option<Key> {
        Some(match self {
            Keypad::Enter => Key::Enter,
            Keypad::Left => Key::Left,
            Keypad::Right => Key::Right,
            Keypad::Up => Key::Up,
            Keypad::Down => Key::Down,
            Keypad::PageUp => Key::PageUp,
            Keypad::PageDown => Key::PageDown,
            Keypad::Home => Key::Home,
            Keypad::End => Key::End,
            Keypad::Insert => Key::Insert,
            Keypad::Delete => Key::Delete,
            _ => return None,
        })
    }
}

/// Kitty codes of keys without a legacy encoding.
pub mod code {
    pub const CAPS_LOCK: u32 = 57358;
    pub const SCROLL_LOCK: u32 = 57359;
    pub const NUM_LOCK: u32 = 57360;
    pub const PRINT_SCREEN: u32 = 57361;
    pub const PAUSE: u32 = 57362;
    pub const MENU: u32 = 57363;
    pub const LEFT_SHIFT: u32 = 57441;
    pub const LEFT_CONTROL: u32 = 57442;
    pub const LEFT_ALT: u32 = 57443;
    pub const LEFT_SUPER: u32 = 57444;
    pub const LEFT_HYPER: u32 = 57445;
    pub const LEFT_META: u32 = 57446;
    pub const RIGHT_SHIFT: u32 = 57447;
    pub const RIGHT_CONTROL: u32 = 57448;
    pub const RIGHT_ALT: u32 = 57449;
    pub const RIGHT_SUPER: u32 = 57450;
    pub const RIGHT_HYPER: u32 = 57451;
    pub const RIGHT_META: u32 = 57452;
    pub const ISO_LEVEL3_SHIFT: u32 = 57453;
    pub const ISO_LEVEL5_SHIFT: u32 = 57454;

    /// Modifier and lock keys: reported only with "report all keys as escape codes".
    pub fn is_modifier(c: u32) -> bool {
        (CAPS_LOCK..=NUM_LOCK).contains(&c) || (LEFT_SHIFT..=ISO_LEVEL5_SHIFT).contains(&c)
    }

    /// The modifier bit a modifier key controls.
    pub(super) fn modifier_bit(c: u32) -> Option<super::Mods> {
        use super::Mods;
        Some(match c {
            LEFT_SHIFT | RIGHT_SHIFT => Mods::SHIFT,
            LEFT_CONTROL | RIGHT_CONTROL => Mods::CTRL,
            LEFT_ALT | RIGHT_ALT => Mods::ALT,
            LEFT_SUPER | RIGHT_SUPER => Mods::SUPER,
            LEFT_HYPER | RIGHT_HYPER => Mods::HYPER,
            LEFT_META | RIGHT_META => Mods::META,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyAction {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent<'a> {
    pub key: Key,
    pub mods: Mods,
    pub action: KeyAction,
    /// Text the key produces with the current modifiers/layout (may be empty).
    pub text: &'a str,
    /// The shifted character of the key, for kitty "report alternates".
    pub shifted: Option<char>,
}

impl<'a> KeyEvent<'a> {
    pub fn new(key: Key, mods: Mods, text: &'a str) -> Self {
        Self { key, mods, action: KeyAction::Press, text, shifted: None }
    }
}

/// Encode a key event. Returns `None` when the key produces nothing.
pub fn encode_key(ev: &KeyEvent, mode: TermMode, flags: KeyboardFlags) -> Option<Vec<u8>> {
    if flags.is_empty() { legacy(ev, mode) } else { kitty(ev, mode, flags) }
}

fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' | '~' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

/// Final byte or `~` number of a functional key, xterm style.
enum Func {
    /// `CSI 1;m X` / `SS3 X` / `CSI X`.
    Letter(u8),
    /// `CSI n ~` / `CSI n;m ~`.
    Tilde(u16),
}

fn func(key: Key) -> Option<Func> {
    Some(match key {
        Key::Up => Func::Letter(b'A'),
        Key::Down => Func::Letter(b'B'),
        Key::Right => Func::Letter(b'C'),
        Key::Left => Func::Letter(b'D'),
        Key::Home => Func::Letter(b'H'),
        Key::End => Func::Letter(b'F'),
        Key::Insert => Func::Tilde(2),
        Key::Delete => Func::Tilde(3),
        Key::PageUp => Func::Tilde(5),
        Key::PageDown => Func::Tilde(6),
        Key::F(1) => Func::Letter(b'P'),
        Key::F(2) => Func::Letter(b'Q'),
        Key::F(3) => Func::Letter(b'R'),
        Key::F(4) => Func::Letter(b'S'),
        Key::F(n @ 5..=12) => Func::Tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        Key::F(n @ 13..=20) => Func::Tilde([25, 26, 28, 29, 31, 32, 33, 34][n as usize - 13]),
        _ => return None,
    })
}

fn legacy(ev: &KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    if ev.action == KeyAction::Release {
        return None;
    }
    match ev.key {
        Key::Code(_) => return None,
        Key::Keypad(k) => {
            return match k.legacy() {
                Some(key) => legacy(&KeyEvent { key, ..*ev }, mode),
                None if k == Keypad::Begin => Some(b"\x1b[E".to_vec()),
                // Digits and operators: their text (Ctrl/Alt as for the main keys).
                None => {
                    let c = ev.text.chars().next().filter(|c| !c.is_control())?;
                    legacy(&KeyEvent { key: Key::Char(c), ..*ev }, mode)
                }
            };
        }
        _ => {}
    }
    let m = ev.mods & (Mods::SHIFT | Mods::ALT | Mods::CTRL | Mods::SUPER);
    let alt = m.contains(Mods::ALT);
    let mut out = Vec::new();
    let prefix_alt = |out: &mut Vec<u8>| {
        if alt {
            out.push(0x1b);
        }
    };
    match ev.key {
        Key::Char(c) => {
            if m.contains(Mods::CTRL)
                && let Some(b) = ctrl_byte(c)
            {
                prefix_alt(&mut out);
                out.push(b);
            } else if !ev.text.is_empty() {
                prefix_alt(&mut out);
                out.extend_from_slice(ev.text.as_bytes());
            } else {
                return None;
            }
        }
        Key::Enter => {
            prefix_alt(&mut out);
            out.extend_from_slice(if mode.contains(TermMode::LINE_FEED_NEW_LINE) { b"\r\n" } else { b"\r" });
        }
        Key::Tab if m.contains(Mods::SHIFT) => out.extend_from_slice(b"\x1b[Z"),
        Key::Tab => {
            prefix_alt(&mut out);
            out.push(b'\t');
        }
        Key::Backspace => {
            prefix_alt(&mut out);
            out.push(if m.contains(Mods::CTRL) { 0x08 } else { 0x7f });
        }
        Key::Escape => {
            prefix_alt(&mut out);
            out.push(0x1b);
        }
        key => {
            let modp = 1 + m.bits();
            match func(key)? {
                Func::Letter(l) if m.is_empty() => {
                    let ss3 = matches!(l, b'P'..=b'S') || mode.contains(TermMode::APP_CURSOR);
                    out.extend_from_slice(if ss3 { b"\x1bO" } else { b"\x1b[" });
                    out.push(l);
                }
                Func::Letter(l) => out.extend_from_slice(format!("\x1b[1;{modp}{}", l as char).as_bytes()),
                Func::Tilde(n) if m.is_empty() => out.extend_from_slice(format!("\x1b[{n}~").as_bytes()),
                Func::Tilde(n) => out.extend_from_slice(format!("\x1b[{n};{modp}~").as_bytes()),
            }
        }
    }
    Some(out)
}

/// Kitty key code and terminator for `key`.
fn kitty_code(key: Key) -> Option<(u32, char)> {
    Some(match key {
        Key::Char(c) => (c.to_lowercase().next().unwrap_or(c) as u32, 'u'),
        Key::Escape => (27, 'u'),
        Key::Enter => (13, 'u'),
        Key::Tab => (9, 'u'),
        Key::Backspace => (127, 'u'),
        Key::F(3) => (13, '~'),
        Key::F(n @ 13..=35) => (57376 + (n as u32 - 13), 'u'),
        Key::Keypad(k) => (k.code(), 'u'),
        Key::Code(c) => (c, 'u'),
        k => match func(k)? {
            Func::Letter(l) => (1, l as char),
            Func::Tilde(n) => (n as u32, '~'),
        },
    })
}

fn kitty(ev: &KeyEvent, mode: TermMode, flags: KeyboardFlags) -> Option<Vec<u8>> {
    let report_all = flags.contains(KeyboardFlags::REPORT_ALL_AS_ESCAPES);
    let events = flags.contains(KeyboardFlags::REPORT_EVENTS);
    if ev.action == KeyAction::Release && !events {
        return None;
    }
    let mut mods = ev.mods;
    let locks = Mods::CAPS_LOCK | Mods::NUM_LOCK;
    let active = mods - locks;
    if let Key::Code(c) = ev.key {
        if code::is_modifier(c) && !report_all {
            return None;
        }
        // A modifier key's own bit reflects the state after the event.
        if let Some(bit) = code::modifier_bit(c) {
            mods.set(bit, ev.action != KeyAction::Release);
        }
    }
    let text_key = match ev.key {
        Key::Char(_) => true,
        // Digits and operators with Num Lock.
        Key::Keypad(_) => !ev.text.is_empty() && !ev.text.chars().any(char::is_control),
        _ => false,
    };
    if !report_all {
        // Lock modifiers are not reported for text keys.
        if text_key {
            mods -= locks;
        }
        // Text keys (optionally with Shift) still send plain text.
        if text_key && (active - Mods::SHIFT).is_empty() && !ev.text.is_empty() {
            return (ev.action != KeyAction::Release).then(|| ev.text.as_bytes().to_vec());
        }
        // Unmodified Enter/Tab/Backspace stay legacy so a crashed app leaves a usable shell.
        if matches!(ev.key, Key::Enter | Key::Tab | Key::Backspace) && active.is_empty() {
            return (ev.action != KeyAction::Release).then(|| legacy(ev, mode)).flatten();
        }
    }
    let (code, term) = kitty_code(ev.key)?;
    let mut key_part = code.to_string();
    if term == 'u'
        && flags.contains(KeyboardFlags::REPORT_ALTERNATES)
        && mods.contains(Mods::SHIFT)
        && let Some(s) = ev.shifted
        && s as u32 != code
    {
        key_part.push_str(&format!(":{}", s as u32));
    }
    let ev_type = match ev.action {
        KeyAction::Press => 1,
        KeyAction::Repeat => 2,
        KeyAction::Release => 3,
    };
    let modv = 1 + mods.bits() as u32;
    let mut mod_part = String::new();
    if modv != 1 || (events && ev_type != 1) {
        mod_part = modv.to_string();
        if events && ev_type != 1 {
            mod_part.push_str(&format!(":{ev_type}"));
        }
    }
    let text_part = if report_all
        && flags.contains(KeyboardFlags::REPORT_TEXT)
        && ev.action != KeyAction::Release
        && !ev.text.is_empty()
        && !ev.text.chars().any(char::is_control)
    {
        ev.text.chars().map(|c| (c as u32).to_string()).collect::<Vec<_>>().join(":")
    } else {
        String::new()
    };
    let s = if term == 'u' || term == '~' {
        let mut s = format!("\x1b[{key_part}");
        if !mod_part.is_empty() || !text_part.is_empty() {
            s.push_str(&format!(";{mod_part}"));
        }
        if !text_part.is_empty() {
            s.push_str(&format!(";{text_part}"));
        }
        s.push(term);
        s
    } else if mod_part.is_empty() {
        format!("\x1b[{term}")
    } else {
        format!("\x1b[1;{mod_part}{term}")
    };
    Some(s.into_bytes())
}

// --- mouse ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    Back,
    Forward,
}

impl MouseButton {
    fn code(self) -> u32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
            MouseButton::WheelLeft => 66,
            MouseButton::WheelRight => 67,
            MouseButton::Back => 128,
            MouseButton::Forward => 129,
        }
    }
    fn is_wheel(self) -> bool {
        matches!(self, Self::WheelUp | Self::WheelDown | Self::WheelLeft | Self::WheelRight)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// For motion: the held button, if any.
    pub button: Option<MouseButton>,
    /// 0-based cell coordinates within the pane.
    pub col: usize,
    pub line: usize,
    /// Pixel position within the pane, for SGR-pixel mode (1016).
    pub pixel: Option<(u32, u32)>,
    pub mods: Mods,
}

/// Encode a mouse event per the active tracking mode, or `None` if the app
/// did not ask for it (the host then handles the mouse itself).
pub fn encode_mouse(ev: &MouseEvent, mode: TermMode) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_ANY) {
        return None;
    }
    let x10 = mode.contains(TermMode::MOUSE_X10);
    let wheel = ev.button.is_some_and(MouseButton::is_wheel);
    match ev.action {
        MouseAction::Press => {}
        MouseAction::Release if x10 || wheel => return None,
        MouseAction::Release => {}
        MouseAction::Motion => {
            let ok = mode.contains(TermMode::MOUSE_ANY_MOTION)
                || (mode.contains(TermMode::MOUSE_BUTTON_MOTION) && ev.button.is_some());
            if !ok {
                return None;
            }
        }
    }
    let mut code = ev.button.map_or(3, MouseButton::code);
    if ev.action == MouseAction::Motion {
        code += 32;
    }
    if !x10 {
        if ev.mods.contains(Mods::SHIFT) {
            code += 4;
        }
        if ev.mods.contains(Mods::ALT) {
            code += 8;
        }
        if ev.mods.contains(Mods::CTRL) {
            code += 16;
        }
    }
    if mode.intersects(TermMode::MOUSE_SGR | TermMode::MOUSE_SGR_PIXELS) {
        let (x, y) = match ev.pixel {
            Some((px, py)) if mode.contains(TermMode::MOUSE_SGR_PIXELS) => (px as usize, py as usize),
            _ => (ev.col + 1, ev.line + 1),
        };
        let fin = if ev.action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes());
    }
    // Legacy encoding: release has no button, coordinates limited to 223.
    if ev.action == MouseAction::Release {
        code = (code & !0b11) | 3;
    }
    let (x, y) = (ev.col + 1 + 32, ev.line + 1 + 32);
    if x > 255 || y > 255 || code + 32 > 255 {
        return None;
    }
    Some(vec![0x1b, b'[', b'M', (code + 32) as u8, x as u8, y as u8])
}

/// Wheel in the alternate screen without mouse tracking scrolls via arrow keys (mode 1007).
pub fn alternate_scroll(lines: i32, mode: TermMode) -> Option<Vec<u8>> {
    if !mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) || mode.intersects(TermMode::MOUSE_ANY) {
        return None;
    }
    let app = mode.contains(TermMode::APP_CURSOR);
    let seq: &[u8] = match (lines > 0, app) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    };
    Some(seq.repeat(lines.unsigned_abs() as usize))
}

// --- focus & paste ------------------------------------------------------------

pub fn encode_focus(focused: bool, mode: TermMode) -> Option<&'static [u8]> {
    mode.contains(TermMode::FOCUS_EVENTS).then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

/// Paste protection: a paste that could execute commands without bracketed
/// paste (newlines or control characters) should be confirmed by the user.
pub fn paste_needs_confirmation(text: &str, mode: TermMode) -> bool {
    !mode.contains(TermMode::BRACKETED_PASTE) && text.chars().any(|c| c.is_control() && c != '\t')
}

/// Bytes to send for a paste. Newlines become CR; with bracketed paste,
/// control characters (including any embedded end marker) are stripped.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if !mode.contains(TermMode::BRACKETED_PASTE) {
        return normalized.into_bytes();
    }
    let body: String = normalized.chars().filter(|&c| !c.is_control() || c == '\t' || c == '\r').collect();
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key, mods: Mods, text: &str, mode: TermMode, flags: KeyboardFlags) -> Vec<u8> {
        encode_key(&KeyEvent::new(k, mods, text), mode, flags).unwrap_or_default()
    }

    #[test]
    fn legacy_keys() {
        let (m, f) = (TermMode::default(), KeyboardFlags::empty());
        assert_eq!(key(Key::Char('a'), Mods::CTRL, "", m, f), b"\x01");
        assert_eq!(key(Key::Char('a'), Mods::ALT, "a", m, f), b"\x1ba");
        assert_eq!(key(Key::Up, Mods::empty(), "", m, f), b"\x1b[A");
        assert_eq!(key(Key::Up, Mods::empty(), "", m | TermMode::APP_CURSOR, f), b"\x1bOA");
        assert_eq!(key(Key::Right, Mods::CTRL, "", m, f), b"\x1b[1;5C");
        assert_eq!(key(Key::Delete, Mods::SHIFT, "", m, f), b"\x1b[3;2~");
        assert_eq!(key(Key::F(5), Mods::empty(), "", m, f), b"\x1b[15~");
        assert_eq!(key(Key::Tab, Mods::SHIFT, "", m, f), b"\x1b[Z");
    }

    #[test]
    fn kitty_keys() {
        let m = TermMode::default();
        let d = KeyboardFlags::DISAMBIGUATE;
        assert_eq!(key(Key::Char('a'), Mods::empty(), "a", m, d), b"a");
        assert_eq!(key(Key::Char('a'), Mods::CTRL, "", m, d), b"\x1b[97;5u");
        assert_eq!(key(Key::Escape, Mods::empty(), "", m, d), b"\x1b[27u");
        assert_eq!(key(Key::Enter, Mods::empty(), "", m, d), b"\r");
        assert_eq!(key(Key::Enter, Mods::SHIFT, "", m, d), b"\x1b[13;2u");
        assert_eq!(key(Key::Up, Mods::empty(), "", m, d), b"\x1b[A");
        let all = KeyboardFlags::REPORT_ALL_AS_ESCAPES | KeyboardFlags::REPORT_TEXT;
        assert_eq!(key(Key::Char('a'), Mods::SHIFT, "A", m, all), b"\x1b[97;2;65u");
        let mut ev = KeyEvent::new(Key::Char('a'), Mods::empty(), "a");
        ev.action = KeyAction::Release;
        assert_eq!(encode_key(&ev, m, d | KeyboardFlags::REPORT_EVENTS), None);
        let all_ev = KeyboardFlags::REPORT_ALL_AS_ESCAPES | KeyboardFlags::REPORT_EVENTS;
        assert_eq!(encode_key(&ev, m, all_ev).unwrap(), b"\x1b[97;1:3u");
        // Lock modifiers: not on text keys unless all keys are escapes.
        assert_eq!(key(Key::Char('a'), Mods::CTRL | Mods::CAPS_LOCK, "", m, d), b"\x1b[97;5u");
        assert_eq!(key(Key::Up, Mods::NUM_LOCK, "", m, d), b"\x1b[1;129A");
        assert_eq!(key(Key::Char('a'), Mods::CAPS_LOCK, "A", m, KeyboardFlags::REPORT_ALL_AS_ESCAPES), b"\x1b[97;65u");
    }

    #[test]
    fn kitty_keypad_and_modifiers() {
        let m = TermMode::default();
        let (none, d) = (KeyboardFlags::empty(), KeyboardFlags::DISAMBIGUATE);
        let all = KeyboardFlags::REPORT_ALL_AS_ESCAPES;
        let kp = |k| Key::Keypad(k);
        // Legacy: like the main keys.
        assert_eq!(key(kp(Keypad::Digit(1)), Mods::NUM_LOCK, "1", m, none), b"1");
        assert_eq!(key(kp(Keypad::Up), Mods::empty(), "", m, none), b"\x1b[A");
        assert_eq!(key(kp(Keypad::Enter), Mods::empty(), "\r", m, none), b"\r");
        assert_eq!(key(kp(Keypad::Begin), Mods::empty(), "", m, none), b"\x1b[E");
        // Disambiguate: text keypad keys still send text, the others CSI u.
        assert_eq!(key(kp(Keypad::Digit(1)), Mods::NUM_LOCK, "1", m, d), b"1");
        assert_eq!(key(kp(Keypad::Up), Mods::empty(), "", m, d), b"\x1b[57419u");
        assert_eq!(key(kp(Keypad::Enter), Mods::empty(), "\r", m, d), b"\x1b[57414u");
        assert_eq!(key(kp(Keypad::Digit(1)), Mods::NUM_LOCK, "1", m, all), b"\x1b[57400;129u");
        // Modifier keys: only with report-all; their own bit follows the event.
        let shift = Key::Code(code::LEFT_SHIFT);
        assert_eq!(encode_key(&KeyEvent::new(shift, Mods::empty(), ""), m, d), None);
        assert_eq!(encode_key(&KeyEvent::new(shift, Mods::empty(), ""), m, none), None);
        assert_eq!(key(shift, Mods::empty(), "", m, all), b"\x1b[57441;2u");
        let mut up = KeyEvent::new(shift, Mods::SHIFT, "");
        up.action = KeyAction::Release;
        let ev = all | KeyboardFlags::REPORT_EVENTS;
        assert_eq!(encode_key(&up, m, ev).unwrap(), b"\x1b[57441;1:3u");
        // Other kitty-only keys: CSI u with any enhancement, nothing in legacy mode.
        assert_eq!(key(Key::Code(code::MENU), Mods::empty(), "", m, d), b"\x1b[57363u");
        assert_eq!(encode_key(&KeyEvent::new(Key::Code(code::MENU), Mods::empty(), ""), m, none), None);
    }

    #[test]
    fn mouse() {
        let ev = MouseEvent {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            col: 4,
            line: 2,
            pixel: None,
            mods: Mods::empty(),
        };
        assert_eq!(encode_mouse(&ev, TermMode::default()), None);
        let sgr = TermMode::MOUSE_NORMAL | TermMode::MOUSE_SGR;
        assert_eq!(encode_mouse(&ev, sgr).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(encode_mouse(&ev, TermMode::MOUSE_NORMAL).unwrap(), b"\x1b[M\x20\x25\x23");
    }

    #[test]
    fn paste() {
        let bp = TermMode::BRACKETED_PASTE;
        assert_eq!(encode_paste("a\nb\x1b[201~", bp), b"\x1b[200~a\rb[201~\x1b[201~");
        assert!(paste_needs_confirmation("ls\n", TermMode::default()));
        assert!(!paste_needs_confirmation("ls\n", bp));
    }
}
