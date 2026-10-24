//! Configured colors. Optional UI colors follow the focused terminal palette.
use mlua::{Table, Value};
use proteus_core::color::{Palette, Rgb};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Rgb>,
    pub indexed: BTreeMap<u8, Rgb>,
    pub ui: Ui,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Ui {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub accent: Option<Rgb>,
    pub bar: Option<Rgb>,
    pub dim: Option<Rgb>,
    pub selection_foreground: Option<Rgb>,
    pub selection_background: Option<Rgb>,
    pub search_foreground: Option<Rgb>,
    pub search_background: Option<Rgb>,
    pub search_current_background: Option<Rgb>,
    pub cursor_text: Option<Rgb>,
    pub bell: Option<Rgb>,
}

impl Default for Theme {
    fn default() -> Self {
        let p = Palette::default();
        Self {
            foreground: p.foreground(),
            background: p.background(),
            cursor: None,
            indexed: BTreeMap::new(),
            ui: Ui::default(),
        }
    }
}

impl Theme {
    pub fn palette(&self) -> Palette {
        let p = Palette::default();
        let ansi = std::array::from_fn(|i| p.indexed(i as u8));
        let mut p = Palette::new(ansi, self.foreground, self.background, self.cursor);
        for (&i, &c) in &self.indexed {
            p.set_base_indexed(i, c);
        }
        p
    }

    /// A preset resets the entire theme before overrides, independent of Lua table iteration order.
    pub fn apply(&mut self, t: Table) -> Result<(), String> {
        if let Some(name) = t.get::<Option<String>>("preset").map_err(|e| e.to_string())? {
            *self = Self::default();
            match name.as_str() {
                "default" | "dark" => {}
                "light" => {
                    self.foreground = Rgb::new(0x28, 0x28, 0x28);
                    self.background = Rgb::new(0xfa, 0xfa, 0xfa);
                    let colors = [
                        "#282828", "#a02020", "#286428", "#806000", "#2050a0", "#803080", "#006868", "#b0b0b0",
                        "#606060", "#c03030", "#307830", "#987000", "#3068b8", "#984098", "#008080", "#fafafa",
                    ];
                    for (i, c) in colors.iter().enumerate() {
                        self.indexed.insert(i as u8, Rgb::parse(c).unwrap());
                    }
                }
                _ => return Err(format!("theme.preset: unknown preset `{name}` (default, dark, light)")),
            }
        }
        for pair in t.pairs::<String, Value>() {
            let (k, v) = pair.map_err(|e| e.to_string())?;
            match k.as_str() {
                "preset" => {}
                "foreground" => self.foreground = color(v, "theme.foreground")?,
                "background" => self.background = color(v, "theme.background")?,
                "cursor" => self.cursor = optional_color(v, "theme.cursor")?,
                "ansi" => {
                    let Value::Table(t) = v else { return Err("theme.ansi: expected 16 colors".into()) };
                    let mut colors = BTreeMap::new();
                    for pair in t.pairs::<Value, Value>() {
                        let (i, c) = pair.map_err(|e| e.to_string())?;
                        let Value::Integer(i @ 1..=16) = i else {
                            return Err("theme.ansi: expected exactly 16 colors at indices 1–16".into());
                        };
                        colors.insert((i - 1) as u8, color(c, "theme.ansi")?);
                    }
                    if colors.len() != 16 {
                        return Err("theme.ansi: expected exactly 16 colors".into());
                    }
                    self.indexed.extend(colors);
                }
                // Applied in a separate pass so indexed entries override ansi deterministically.
                "indexed" => {}
                "ui" => {
                    let Value::Table(t) = v else { return Err("theme.ui: expected a table".into()) };
                    for pair in t.pairs::<String, Value>() {
                        let (k, v) = pair.map_err(|e| e.to_string())?;
                        let field = match k.as_str() {
                            "foreground" => &mut self.ui.foreground,
                            "background" => &mut self.ui.background,
                            "accent" => &mut self.ui.accent,
                            "bar" => &mut self.ui.bar,
                            "dim" => &mut self.ui.dim,
                            "selection_foreground" => &mut self.ui.selection_foreground,
                            "selection_background" => &mut self.ui.selection_background,
                            "search_foreground" => &mut self.ui.search_foreground,
                            "search_background" => &mut self.ui.search_background,
                            "search_current_background" => &mut self.ui.search_current_background,
                            "cursor_text" => &mut self.ui.cursor_text,
                            "bell" => &mut self.ui.bell,
                            _ => return Err(format!("unknown config key `theme.ui.{k}`")),
                        };
                        *field = optional_color(v, &format!("theme.ui.{k}"))?;
                    }
                }
                _ => return Err(format!("unknown config key `theme.{k}`")),
            }
        }
        if let Some(t) = t.get::<Option<Table>>("indexed").map_err(|e| format!("theme.indexed: {e}"))? {
            for pair in t.pairs::<Value, Value>() {
                let (i, c) = pair.map_err(|e| e.to_string())?;
                let Value::Integer(i @ 0..=255) = i else {
                    return Err("theme.indexed: expected integer keys 0–255".into());
                };
                if matches!(c, Value::Boolean(false)) {
                    self.indexed.remove(&(i as u8));
                } else {
                    self.indexed.insert(i as u8, color(c, "theme.indexed")?);
                }
            }
        }
        Ok(())
    }
}

fn color(v: Value, name: &str) -> Result<Rgb, String> {
    if let Value::String(s) = v {
        let s = s.to_str().map_err(|e| e.to_string())?;
        // Validate ASCII before the shared X11 parser slices by byte offsets.
        if s.is_ascii()
            && let Some(c) = Rgb::parse(&s)
        {
            return Ok(c);
        }
    }
    Err(format!("{name}: expected a hex color such as \"#rrggbb\""))
}
fn optional_color(v: Value, name: &str) -> Result<Option<Rgb>, String> {
    if matches!(v, Value::Boolean(false)) { Ok(None) } else { color(v, name).map(Some) }
}
