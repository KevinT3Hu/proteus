//! `p.ui`: plugin-style surfaces for Lua (doc 05). Lua builds frames from
//! strings and span tables; events reach the surface's `on_event`.
//! Rows and columns are 1-based on the Lua side.

use mlua::{Lua, Table, Value};
use proteus_wasm::types::{
    Attrs, Color, Frame, Line, Modifiers, MouseAction, MouseButton, Span, Style, SurfaceKind, SurfaceOptions, UiEvent,
};

use super::runtime;

/// `p.ui.open{...}` options.
pub fn options(t: Option<Table>) -> mlua::Result<SurfaceOptions> {
    let mut o =
        SurfaceOptions { kind: SurfaceKind::Float, title: String::new(), cols: 40, rows: 0, focus: true, pane: None };
    let Some(t) = t else { return Ok(o) };
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair?;
        match (k.as_str(), v) {
            ("kind", Value::String(s)) => {
                o.kind = match s.to_str()?.as_ref() {
                    "float" => SurfaceKind::Float,
                    "sidebar_left" => SurfaceKind::SidebarLeft,
                    "sidebar_right" => SurfaceKind::SidebarRight,
                    "overlay" => SurfaceKind::Overlay,
                    "pane_right" => SurfaceKind::PaneRight,
                    "pane_down" => SurfaceKind::PaneDown,
                    "tab" => SurfaceKind::Tab,
                    k => {
                        return Err(runtime(format!(
                            "p.ui.open: unknown kind `{k}` (float, sidebar_left, sidebar_right, overlay, pane_right, pane_down, tab)"
                        )));
                    }
                }
            }
            ("title", Value::String(s)) => o.title = s.to_str()?.to_string(),
            ("cols", Value::Integer(n)) if n > 0 => o.cols = n.min(500) as u32,
            ("rows", Value::Integer(n)) if n >= 0 => o.rows = n.min(500) as u32,
            ("focus", Value::Boolean(b)) => o.focus = b,
            ("pane", Value::Integer(n)) if n >= 0 => o.pane = Some(n as u64),
            ("pane", Value::Table(p)) => o.pane = Some(p.raw_get::<i64>("id")? as u64),
            ("on_event", Value::Function(_) | Value::Nil) => {}
            (k, _) => return Err(runtime(format!("p.ui.open: unknown or invalid option `{k}`"))),
        }
    }
    Ok(o)
}

/// `0`..`255` or `"#rrggbb"`.
fn color(v: Value, what: &str) -> mlua::Result<Color> {
    match v {
        Value::Integer(i) if (0..=255).contains(&i) => Ok(Color::Indexed(i as u8)),
        Value::String(s) => {
            let s = s.to_str()?;
            s.strip_prefix('#')
                .filter(|h| h.len() == 6)
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .map(Color::Rgb)
                .ok_or_else(|| runtime(format!("{what}: expected \"#rrggbb\", got {s:?}")))
        }
        _ => Err(runtime(format!("{what}: expected a palette index 0-255 or \"#rrggbb\""))),
    }
}

fn span(v: Value) -> mlua::Result<Span> {
    let mut s = Span { text: String::new(), style: Style { fg: None, bg: None, attrs: Attrs::empty() }, link: None };
    match v {
        Value::String(t) => s.text = t.to_str()?.to_string(),
        Value::Integer(_) | Value::Number(_) => s.text = v.to_string()?,
        Value::Table(t) => {
            for pair in t.pairs::<Value, Value>() {
                let (k, v) = pair?;
                let k = match k {
                    Value::Integer(1) => "text".to_string(),
                    Value::String(k) => k.to_str()?.to_string(),
                    _ => return Err(runtime("span: unexpected key")),
                };
                let flag = |v: &Value| matches!(v, Value::Boolean(true));
                match k.as_str() {
                    "text" => s.text = v.to_string()?,
                    "fg" => s.style.fg = Some(color(v, "span.fg")?),
                    "bg" => s.style.bg = Some(color(v, "span.bg")?),
                    "link" => s.link = Some(v.to_string()?),
                    "bold" if flag(&v) => s.style.attrs |= Attrs::BOLD,
                    "dim" if flag(&v) => s.style.attrs |= Attrs::DIM,
                    "italic" if flag(&v) => s.style.attrs |= Attrs::ITALIC,
                    "underline" if flag(&v) => s.style.attrs |= Attrs::UNDERLINE,
                    "reverse" if flag(&v) => s.style.attrs |= Attrs::REVERSE,
                    "strike" if flag(&v) => s.style.attrs |= Attrs::STRIKE,
                    "bold" | "dim" | "italic" | "underline" | "reverse" | "strike" => {}
                    k => return Err(runtime(format!("span: unknown key `{k}`"))),
                }
            }
        }
        _ => return Err(runtime("surface:draw: a span is a string or a table")),
    }
    Ok(s)
}

/// A line: a string, or a list of spans (strings or span tables).
fn line(v: Value) -> mlua::Result<Line> {
    match v {
        Value::Table(t) if t.raw_get::<Value>("text")?.is_nil() && t.raw_get::<Value>(1)?.is_table() => {
            t.sequence_values::<Value>().map(|v| span(v?)).collect()
        }
        Value::Table(t) if t.raw_get::<Value>("text")?.is_nil() && t.raw_len() > 1 => {
            t.sequence_values::<Value>().map(|v| span(v?)).collect()
        }
        v => Ok(vec![span(v)?]),
    }
}

/// `surface:draw(lines, { cursor = { col, row }, background = color })`.
pub fn frame(lines: Table, opts: Option<Table>) -> mlua::Result<Frame> {
    let lines = lines.sequence_values::<Value>().take(1000).map(|v| line(v?)).collect::<mlua::Result<_>>()?;
    let mut f = Frame { lines, cursor: None, background: None, images: Vec::new() };
    if let Some(o) = opts {
        for pair in o.pairs::<String, Value>() {
            let (k, v) = pair?;
            match (k.as_str(), v) {
                ("cursor", Value::Table(c)) => {
                    let (col, row): (u32, u32) = (c.get(1)?, c.get(2)?);
                    f.cursor = Some((col.saturating_sub(1), row.saturating_sub(1)));
                }
                ("cursor", Value::Nil) => {}
                ("background", v) => f.background = Some(color(v, "background")?),
                (k, _) => return Err(runtime(format!("surface:draw: unknown option `{k}`"))),
            }
        }
    }
    Ok(f)
}

fn mods(lua: &Lua, m: Modifiers) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("shift", m.contains(Modifiers::SHIFT))?;
    t.set("ctrl", m.contains(Modifiers::CTRL))?;
    t.set("alt", m.contains(Modifiers::ALT))?;
    t.set("logo", m.contains(Modifiers::LOGO))?;
    Ok(t)
}

/// An event table for `on_event`.
pub fn event(lua: &Lua, e: &UiEvent) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match e {
        UiEvent::Key(k) => {
            t.set("type", "key")?;
            t.set("key", k.key.as_str())?;
            t.set("text", k.text.as_str())?;
            t.set("mods", mods(lua, k.mods)?)?;
            t.set("repeat", k.repeat)?;
        }
        UiEvent::Text(s) => {
            t.set("type", "text")?;
            t.set("text", s.as_str())?;
        }
        UiEvent::Preedit(p) => {
            t.set("type", "preedit")?;
            if let Some((text, cursor)) = p {
                t.set("text", text.as_str())?;
                t.set("cursor", *cursor)?;
            }
        }
        UiEvent::Mouse(m) => {
            t.set("type", "mouse")?;
            t.set(
                "button",
                match m.button {
                    MouseButton::Left => "left",
                    MouseButton::Middle => "middle",
                    MouseButton::Right => "right",
                    MouseButton::WheelUp => "wheel_up",
                    MouseButton::WheelDown => "wheel_down",
                },
            )?;
            t.set(
                "action",
                match m.action {
                    MouseAction::Press => "press",
                    MouseAction::Release => "release",
                    MouseAction::Drag => "drag",
                },
            )?;
            t.set("col", m.col + 1)?;
            t.set("row", m.row + 1)?;
            t.set("mods", mods(lua, m.mods)?)?;
        }
        UiEvent::Resize((c, r)) => {
            t.set("type", "resize")?;
            t.set("cols", *c)?;
            t.set("rows", *r)?;
        }
        UiEvent::Focus(f) => {
            t.set("type", "focus")?;
            t.set("focused", *f)?;
        }
        UiEvent::Closed => t.set("type", "close")?,
        UiEvent::Drop(paths) => {
            t.set("type", "drop")?;
            t.set("paths", lua.create_sequence_from(paths.iter().map(String::as_str))?)?;
        }
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_from_lua() {
        let lua = Lua::new();
        let (lines, opts): (Table, Table) = lua
            .load(
                r##"return {
                    "plain",
                    { "a", { text = "b", fg = 2, bold = true }, { "c", bg = "#102030", link = "https://e.x" } },
                    { text = "one span", italic = true },
                }, { cursor = { 3, 2 } }"##,
            )
            .eval()
            .unwrap();
        let f = frame(lines, Some(opts)).unwrap();
        assert_eq!(f.lines.len(), 3);
        assert_eq!(f.lines[0][0].text, "plain");
        assert_eq!(f.lines[1].len(), 3);
        assert!(matches!(f.lines[1][1].style.fg, Some(Color::Indexed(2))));
        assert!(f.lines[1][1].style.attrs.contains(Attrs::BOLD));
        assert!(matches!(f.lines[1][2].style.bg, Some(Color::Rgb(0x102030))));
        assert_eq!(f.lines[1][2].link.as_deref(), Some("https://e.x"));
        assert_eq!(f.lines[2].len(), 1);
        assert!(f.lines[2][0].style.attrs.contains(Attrs::ITALIC));
        assert_eq!(f.cursor, Some((2, 1)));
        let bad: Table = lua.load(r#"return { { text = "x", fg = "red" } }"#).eval().unwrap();
        assert!(frame(bad, None).unwrap_err().to_string().contains("#rrggbb"));
        let o: Table = lua.load(r#"return { kind = "sidebar_left", cols = 30, title = "T" }"#).eval().unwrap();
        let o = options(Some(o)).unwrap();
        assert_eq!((o.kind, o.cols, o.title.as_str()), (SurfaceKind::SidebarLeft, 30, "T"));
    }
}
