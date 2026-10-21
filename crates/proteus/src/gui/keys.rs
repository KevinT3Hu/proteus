//! Translate xkb key events into `proteus_core::input` keys.

use proteus_core::input::{Key, Keypad, Mods, code};
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use xkbcommon::xkb;

/// Keypad key for a keysym.
fn keypad(sym: Keysym) -> Option<Keypad> {
    let raw = sym.raw();
    if (Keysym::KP_0.raw()..=Keysym::KP_9.raw()).contains(&raw) {
        return Some(Keypad::Digit((raw - Keysym::KP_0.raw()) as u8));
    }
    Some(match sym {
        Keysym::KP_Decimal => Keypad::Decimal,
        Keysym::KP_Divide => Keypad::Divide,
        Keysym::KP_Multiply => Keypad::Multiply,
        Keysym::KP_Subtract => Keypad::Subtract,
        Keysym::KP_Add => Keypad::Add,
        Keysym::KP_Enter => Keypad::Enter,
        Keysym::KP_Equal => Keypad::Equal,
        Keysym::KP_Separator => Keypad::Separator,
        Keysym::KP_Left => Keypad::Left,
        Keysym::KP_Right => Keypad::Right,
        Keysym::KP_Up => Keypad::Up,
        Keysym::KP_Down => Keypad::Down,
        Keysym::KP_Page_Up => Keypad::PageUp,
        Keysym::KP_Page_Down => Keypad::PageDown,
        Keysym::KP_Home => Keypad::Home,
        Keysym::KP_End => Keypad::End,
        Keysym::KP_Insert => Keypad::Insert,
        Keysym::KP_Delete => Keypad::Delete,
        Keysym::KP_Begin => Keypad::Begin,
        _ => return None,
    })
}

/// Kitty code of a modifier, lock or other key without a legacy encoding.
fn kitty_only(sym: Keysym) -> Option<u32> {
    Some(match sym {
        Keysym::Shift_L => code::LEFT_SHIFT,
        Keysym::Shift_R => code::RIGHT_SHIFT,
        Keysym::Control_L => code::LEFT_CONTROL,
        Keysym::Control_R => code::RIGHT_CONTROL,
        Keysym::Alt_L => code::LEFT_ALT,
        Keysym::Alt_R => code::RIGHT_ALT,
        Keysym::Super_L => code::LEFT_SUPER,
        Keysym::Super_R => code::RIGHT_SUPER,
        Keysym::Hyper_L => code::LEFT_HYPER,
        Keysym::Hyper_R => code::RIGHT_HYPER,
        Keysym::Meta_L => code::LEFT_META,
        Keysym::Meta_R => code::RIGHT_META,
        Keysym::ISO_Level3_Shift => code::ISO_LEVEL3_SHIFT,
        Keysym::ISO_Level5_Shift => code::ISO_LEVEL5_SHIFT,
        Keysym::Caps_Lock => code::CAPS_LOCK,
        Keysym::Num_Lock => code::NUM_LOCK,
        Keysym::Scroll_Lock => code::SCROLL_LOCK,
        Keysym::Print => code::PRINT_SCREEN,
        Keysym::Pause => code::PAUSE,
        Keysym::Menu => code::MENU,
        _ => return None,
    })
}

/// Functional key for a keysym, if it is one.
pub fn functional(sym: Keysym) -> Option<Key> {
    if let Some(k) = keypad(sym) {
        return Some(Key::Keypad(k));
    }
    if let Some(c) = kitty_only(sym) {
        return Some(Key::Code(c));
    }
    Some(match sym {
        Keysym::Return | Keysym::ISO_Enter => Key::Enter,
        Keysym::Tab | Keysym::ISO_Left_Tab | Keysym::KP_Tab => Key::Tab,
        Keysym::BackSpace => Key::Backspace,
        Keysym::Escape => Key::Escape,
        Keysym::Up => Key::Up,
        Keysym::Down => Key::Down,
        Keysym::Left => Key::Left,
        Keysym::Right => Key::Right,
        Keysym::Home => Key::Home,
        Keysym::End => Key::End,
        Keysym::Page_Up => Key::PageUp,
        Keysym::Page_Down => Key::PageDown,
        Keysym::Insert => Key::Insert,
        Keysym::Delete => Key::Delete,
        s if (Keysym::F1.raw()..=Keysym::F35.raw()).contains(&s.raw()) => {
            Key::F((s.raw() - Keysym::F1.raw() + 1) as u8)
        }
        _ => return None,
    })
}

/// Modifier-only keys produce no input.
pub fn is_modifier(sym: Keysym) -> bool {
    sym.is_modifier_key()
}

pub fn mods(m: &Modifiers) -> Mods {
    let mut out = Mods::empty();
    out.set(Mods::SHIFT, m.shift);
    out.set(Mods::ALT, m.alt);
    out.set(Mods::CTRL, m.ctrl);
    out.set(Mods::SUPER, m.logo);
    out.set(Mods::CAPS_LOCK, m.caps_lock);
    out.set(Mods::NUM_LOCK, m.num_lock);
    out
}

/// Our own copy of the keymap, to look up the unshifted (level 0) and
/// shifted (level 1) symbols of a key as the kitty protocol requires.
pub struct Levels {
    keymap: Option<xkb::Keymap>,
    pub layout: u32,
}

impl Levels {
    pub fn new() -> Self {
        Self { keymap: None, layout: 0 }
    }

    pub fn set_keymap(&mut self, text: String) {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        self.keymap =
            xkb::Keymap::new_from_string(&ctx, text, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS);
    }

    fn level_char(&self, raw_code: u32, level: u32) -> Option<char> {
        let km = self.keymap.as_ref()?;
        let sym = *km.key_get_syms_by_level(xkb::Keycode::new(raw_code + 8), self.layout, level).first()?;
        char::from_u32(xkb::keysym_to_utf32(sym)).filter(|c| !c.is_control() && *c != '\0')
    }

    /// The key for a press: functional keys, else the unshifted character.
    /// Returns the key and the shifted character (for "report alternates").
    pub fn key(&self, sym: Keysym, raw_code: u32) -> Option<(Key, Option<char>)> {
        if let Some(k) = functional(sym) {
            return Some((k, None));
        }
        let base = self
            .level_char(raw_code, 0)
            .or_else(|| sym.key_char().map(|c| c.to_lowercase().next().unwrap_or(c)))
            .filter(|c| !c.is_control())?;
        Some((Key::Char(base), self.level_char(raw_code, 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functional_keys() {
        assert_eq!(functional(Keysym::Return), Some(Key::Enter));
        assert_eq!(functional(Keysym::F12), Some(Key::F(12)));
        assert_eq!(functional(Keysym::KP_Left), Some(Key::Keypad(Keypad::Left)));
        assert_eq!(functional(Keysym::KP_7), Some(Key::Keypad(Keypad::Digit(7))));
        assert_eq!(functional(Keysym::Shift_L), Some(Key::Code(code::LEFT_SHIFT)));
        assert_eq!(functional(Keysym::a), None);
        // Without a keymap the keysym's character is used, lowercased.
        let l = Levels::new();
        assert_eq!(l.key(Keysym::A, 30), Some((Key::Char('a'), None)));
        assert!(is_modifier(Keysym::Shift_L));
    }
}
