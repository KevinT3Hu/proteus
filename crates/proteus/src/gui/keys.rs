//! Translate xkb key events into `proteus_core::input` keys.

use proteus_core::input::{Key, Mods};
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use xkbcommon::xkb;

/// Functional key for a keysym, if it is one.
pub fn functional(sym: Keysym) -> Option<Key> {
    Some(match sym {
        Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter => Key::Enter,
        Keysym::Tab | Keysym::ISO_Left_Tab | Keysym::KP_Tab => Key::Tab,
        Keysym::BackSpace => Key::Backspace,
        Keysym::Escape => Key::Escape,
        Keysym::Up | Keysym::KP_Up => Key::Up,
        Keysym::Down | Keysym::KP_Down => Key::Down,
        Keysym::Left | Keysym::KP_Left => Key::Left,
        Keysym::Right | Keysym::KP_Right => Key::Right,
        Keysym::Home | Keysym::KP_Home => Key::Home,
        Keysym::End | Keysym::KP_End => Key::End,
        Keysym::Page_Up | Keysym::KP_Page_Up => Key::PageUp,
        Keysym::Page_Down | Keysym::KP_Page_Down => Key::PageDown,
        Keysym::Insert | Keysym::KP_Insert => Key::Insert,
        Keysym::Delete | Keysym::KP_Delete => Key::Delete,
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
        assert_eq!(functional(Keysym::KP_Left), Some(Key::Left));
        assert_eq!(functional(Keysym::a), None);
        // Without a keymap the keysym's character is used, lowercased.
        let l = Levels::new();
        assert_eq!(l.key(Keysym::A, 30), Some((Key::Char('a'), None)));
        assert!(is_modifier(Keysym::Shift_L));
    }
}
