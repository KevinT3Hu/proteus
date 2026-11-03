//! User configuration from `$XDG_CONFIG_HOME/proteus/init.lua` (Lua 5.4).
//!
//! ```lua
//! local p = require("proteus")
//! p.config {
//!   font = { family = "JetBrains Mono", size = 12, ligatures = true },
//!   cursor = { blink = true },
//!   text = { grapheme_clustering = true },
//!   clipboard = { osc52_read = "ask", paste_protection = true },
//!   shell = { command = { "fish", "-l" }, integration = true },
//!   scrollback = 20000,
//!   ui = { hint_bar = "in-mode", tab_bar = "auto", pane_titles = "never", gutter = 0.5 },
//!   panes = { confirm_close = true, confirm_close_ignore = { "less", "man" } },
//!   session = { restore = "ask", restore_commands = { "nvim", "htop" } },
//!   notify = { min_duration = 10, osc = true },  -- min_duration = false: never
//!   bell = { visual = true },
//!   -- Inline images (kitty, Sixel, iTerm2). Sizes in MiB.
//!   graphics = { enabled = true, max_image = 64, pane_memory = 256, total_memory = 1024,
//!                gpu_memory = 512, local_files = true },
//!   hints = {
//!     alphabet = "asdfghjkl",
//!     patterns = { { name = "jira", regex = "\\b[A-Z]+-\\d+\\b", url = "https://jira.example/browse/{}" } },
//!   },
//!   keys = {
//!     preset = "zellij",   -- or "leader", "tmux"
//!     leader = "ctrl+g",   -- zellij: lock/unlock; leader: opens the mode menu
//!     sticky = false,
//!     -- Per mode: key = action name (`proteus --list-actions`), or "none".
//!     normal = { ["alt+x"] = "pane.close", ["alt+t"] = "none" },
//!     pane = { ["v"] = "pane.split_right" },
//!   },
//! }
//!
//! -- Keys can run Lua; hooks, status segments, palette entries, timers:
//! p.keymap("pane", "g", function(ctx) ctx.pane:split { cmd = "lazygit" } end)
//! p.on("command_finished", function(ev)
//!   if ev.exit_code ~= nil and ev.exit_code ~= 0 and (ev.duration or 0) > 30 then p.notify("failed: " .. (ev.command or "(unknown command)")) end
//! end)
//! p.status.segment("right", function() return os.date("%H:%M") end)
//! p.palette.add("Git status", function(ctx) ctx.pane:send_text("git status\r") end)
//! ```
//!
//! `p.config` may be called several times; later calls override earlier
//! keys. `require` resolves modules inside the config directory. Unknown
//! keys and wrong types are errors, so typos do not go unnoticed. No config
//! file is required. The scripting API is in [`crate::script`].

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Table, Value};
use proteus_core::ClipboardRead;

use crate::modes::{Action, Chord, KeyConfig, Mode, Preset};
use crate::script::{Registrar, Script};

/// Window decorations (title bar, borders).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decorations {
    /// Ask the compositor to draw them.
    Auto,
    /// Tell the compositor the client decorates, and draw none.
    None,
    /// Draw our own title bar, buttons and border.
    Client,
}

/// Cursor blinking policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blink {
    /// Blink when the application asks for it (DECSCUSR, mode 12).
    App,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub i18n: crate::i18n::Catalog,
    /// Explicit WASM paths/URLs; Lua extensions are ordinary config modules.
    pub plugins: Vec<proteus_wasm::source::Source>,
    pub theme: crate::theme::Theme,
    /// Window padding in logical pixels.
    pub padding: f64,
    pub padding_vertical: f64,
    pub pane_gap: Option<f64>,
    pub border_width: f64,
    pub font_settings: crate::appearance::FontSettings,
    pub window: crate::appearance::WindowSettings,
    /// Opacity of the default background (cells with their own colors and
    /// the bars stay opaque).
    pub background_opacity: f32,
    /// Ask the compositor to blur what shows through (`ext-background-effect-v1`).
    pub background_blur: bool,
    pub decorations: Decorations,
    pub font_family: String,
    pub font_size: f32,
    pub ligatures: bool,
    pub cursor_blink: Blink,
    pub cursor_shape: Option<proteus_core::CursorShape>,
    /// Mode 2027 (grapheme clustering) on without the application asking.
    pub grapheme_clustering: bool,
    /// Treat East Asian ambiguous-width characters as wide.
    pub ambiguous_wide: bool,
    pub bidi: proteus_core::bidi::Mode,
    pub osc52_write: bool,
    pub osc52_read: ClipboardRead,
    /// Confirm multi-line pastes and pastes with control characters when
    /// the application has not enabled bracketed paste.
    pub paste_protection: bool,
    /// Program and arguments; `None` runs `$SHELL`.
    pub command: Option<Vec<OsString>>,
    /// Opt-in control socket, created at startup.
    pub ipc_socket: Option<PathBuf>,
    pub shell_integration: bool,
    pub term: Option<String>,
    pub scrollback: usize,
    pub keys: KeyConfig,
    pub hint_bar: HintBar,
    pub tab_bar: TabBar,
    pub pane_titles: PaneTitles,
    /// Closing the last pane of a window starts a new shell instead of
    /// closing the window.
    pub keep_last_shell: bool,
    /// Ask before closing a pane whose foreground process is not the shell.
    pub confirm_close: bool,
    /// Foreground programs that never need the close confirmation.
    pub confirm_close_ignore: Vec<String>,
    pub restore: Restore,
    /// Foreground programs that are started again when a session is restored.
    pub restore_commands: Vec<String>,
    /// Width of the command-block gutter left of each pane, in cells (0: none).
    pub gutter: f32,
    /// Desktop notification when a command that ran at least this long
    /// finishes in a pane without focus (`None`: never).
    pub notify_after: Option<std::time::Duration>,
    /// Pass OSC 9 / OSC 777 notifications from applications to the desktop.
    pub notify_osc: bool,
    /// Flash the pane on the bell.
    pub visual_bell: bool,
    /// Hint labels are made of these characters.
    pub hint_alphabet: String,
    /// Extra hint patterns, tried before the built-in ones.
    pub hint_patterns: Vec<HintPatternConfig>,
    pub graphics: Graphics,
}

/// Inline image settings (`graphics = { ... }`); sizes in bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Graphics {
    /// Accept image protocols at all.
    pub enabled: bool,
    /// Largest single decoded image (all frames).
    pub max_image: usize,
    /// Decoded image memory per pane.
    pub pane_memory: usize,
    /// Decoded image memory of all panes together.
    pub total_memory: usize,
    /// GPU texture memory per window before least recently drawn images are
    /// dropped (and uploaded again when needed).
    pub gpu_memory: usize,
    /// Let local programs send images as files or shared memory (kitty `t=f/t/s`).
    pub local_files: bool,
}

impl Default for Graphics {
    fn default() -> Self {
        const MIB: usize = 1 << 20;
        Self {
            enabled: true,
            max_image: 64 * MIB,
            pane_memory: 256 * MIB,
            total_memory: 1024 * MIB,
            gpu_memory: 512 * MIB,
            local_files: true,
        }
    }
}

/// A user-defined hint pattern: `{ name = "jira", regex = "[A-Z]+-\\d+", url = "https://x/browse/{}" }`.
#[derive(Debug, Clone, PartialEq)]
pub struct HintPatternConfig {
    pub name: String,
    pub regex: String,
    /// Opening the hint opens this URL with `{}` replaced by the text.
    pub url: Option<String>,
}

/// Restoring the previous session at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// Offer to restore after an unclean exit (crash, logout, killed).
    Ask,
    /// Always restore the last session.
    Always,
    /// Never restore (and do not save).
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintBar {
    /// Over the bottom row while a mode other than Normal is active.
    InMode,
    /// A reserved bottom row, always visible.
    Always,
    Never,
}

/// Title rows above tiled panes (stacked and floating panes always have one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneTitles {
    /// When the tab has more than one tiled pane.
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBar {
    /// Shown when there is more than one tab.
    Auto,
    Always,
    Never,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            theme: crate::theme::Theme::default(),
            i18n: crate::i18n::defaults(),
            padding: 4.0,
            padding_vertical: 4.0,
            pane_gap: None,
            border_width: 1.0,
            font_settings: Default::default(),
            window: Default::default(),
            background_opacity: 1.0,
            background_blur: false,
            decorations: Decorations::Auto,
            font_family: "monospace".into(),
            font_size: 11.0,
            ligatures: true,
            cursor_blink: Blink::App,
            cursor_shape: None,
            grapheme_clustering: false,
            ambiguous_wide: false,
            bidi: Default::default(),
            osc52_write: true,
            osc52_read: ClipboardRead::Ask,
            paste_protection: true,
            command: None,
            ipc_socket: None,
            shell_integration: true,
            term: None,
            scrollback: 10_000,
            keys: KeyConfig::default(),
            hint_bar: HintBar::InMode,
            pane_titles: PaneTitles::Never,
            keep_last_shell: false,
            tab_bar: TabBar::Auto,
            confirm_close: true,
            confirm_close_ignore: ["less", "more", "man", "top", "htop", "btop", "watch", "tail"]
                .map(String::from)
                .to_vec(),
            restore: Restore::Ask,
            restore_commands: ["nvim", "vim", "hx", "less", "htop", "btop", "top", "man"].map(String::from).to_vec(),
            gutter: 0.5,
            notify_after: Some(std::time::Duration::from_secs(10)),
            notify_osc: true,
            visual_bell: true,
            hint_alphabet: proteus_core::hints::DEFAULT_ALPHABET.into(),
            hint_patterns: Vec::new(),
            graphics: Graphics::default(),
        }
    }
}

impl Config {
    /// Emulation settings for the core.
    pub fn core(&self) -> proteus_core::Config {
        let mut c = crate::session::local_config();
        c.scrollback = self.scrollback;
        c.ambiguous_wide = self.ambiguous_wide;
        c.bidi = self.bidi;
        c.osc52_write = self.osc52_write;
        c.osc52_read = self.osc52_read;
        c.grapheme_clustering = self.grapheme_clustering;
        let g = &mut c.graphics;
        g.enabled = self.graphics.enabled;
        g.max_image_bytes = self.graphics.max_image;
        g.pane_budget = self.graphics.pane_memory;
        g.allow_local_files = self.graphics.local_files;
        c
    }

    /// Initial size of the window in logical pixels. Compositors retain the final say.
    pub fn window_size(&self, cell: (u32, u32), scale: f64) -> (u32, u32) {
        self.window.size(
            (cell.0 as f64 / scale, cell.1 as f64 / scale),
            ((self.padding * scale).round() / scale, (self.padding_vertical * scale).round() / scale),
            u32::from(self.tab_bar == TabBar::Always)
                + u32::from(self.hint_bar == HintBar::Always)
                + u32::from(self.decorations == Decorations::Client),
            self.pane_titles == PaneTitles::Always,
            ((self.gutter as f64 * cell.0 as f64).round() + 1.0) / scale,
        )
    }

    pub fn plugin_should_load(&self, entry: &proteus_wasm::source::Resolved) -> bool {
        self.plugins.contains(&entry.source)
    }

    pub fn keymap(&self) -> crate::modes::Keymap {
        crate::modes::Keymap::new(&self.keys)
    }
}

/// `$XDG_CONFIG_HOME/proteus/init.lua` (or `~/.config/...`).
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("proteus").join("init.lua"))
}

/// A loaded config: the settings, and the Lua state the file ran in (with
/// the hooks, key functions and segments it registered).
pub struct Loaded {
    pub config: Config,
    pub script: Script,
}

/// Load `path`. A missing default file is not an error; a missing file
/// given explicitly is.
pub fn load(path: &Path, explicit: bool) -> Result<Loaded, String> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => return eval("", path),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    eval(&src, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Built-in defaults, with an empty script.
pub fn defaults() -> Loaded {
    Loaded { config: Config::default(), script: Script::new(None).expect("a Lua state can be created") }
}

/// Evaluate config source; `path` names the chunk and sets the module dir.
pub fn eval(src: &str, path: &Path) -> Result<Loaded, String> {
    let script = Script::new(path.parent())?;
    let cfg = Rc::new(RefCell::new(Config::default()));
    // `p.config` and `p.keymap` only work while the file runs.
    let loading = Rc::new(Cell::new(true));
    let setup = || -> mlua::Result<()> {
        let lua = script.lua();
        let module = script.module()?;
        let i18n = lua.create_table()?;
        let c = cfg.clone();
        i18n.set("locale", lua.create_function(move |_, ()| Ok(c.borrow().i18n.locale().to_owned()))?)?;
        let c = cfg.clone();
        i18n.set(
            "t",
            lua.create_function(move |_, (id, values): (String, Option<Table>)| {
                let mut args = proteus_i18n::FluentArgs::new();
                if let Some(values) = values {
                    for pair in values.pairs::<String, Value>() {
                        let (key, value) = pair?;
                        let value = match value {
                            Value::String(v) => proteus_i18n::FluentValue::from(v.to_str()?.to_owned()),
                            Value::Integer(v) => proteus_i18n::FluentValue::from(v),
                            Value::Number(v) => proteus_i18n::FluentValue::from(v),
                            Value::Boolean(v) => proteus_i18n::FluentValue::from(v.to_string()),
                            _ => {
                                return Err(mlua::Error::runtime(
                                    "Fluent arguments must be strings, numbers, or booleans",
                                ));
                            }
                        };
                        args.set(key, value);
                    }
                }
                c.borrow().i18n.format(&id, Some(&args)).map_err(mlua::Error::runtime)
            })?,
        )?;
        let c = cfg.clone();
        let on = loading.clone();
        i18n.set(
            "load",
            lua.create_function(move |_, (locale, source): (String, String)| {
                if !on.get() {
                    return Err(mlua::Error::runtime("load Fluent resources while config loads"));
                }
                c.borrow_mut().i18n.add_resource(&locale, &source).map_err(mlua::Error::runtime)
            })?,
        )?;
        module.set("i18n", i18n)?;
        let (c, on) = (cfg.clone(), loading.clone());
        module.set(
            "load_plugin",
            lua.create_function(move |lua, value: Value| {
                if !on.get() {
                    return Err(mlua::Error::runtime("p.load_plugin: only while the config loads"));
                }
                let source = plugin_source(lua, &value).map_err(mlua::Error::runtime)?;
                let mut config = c.borrow_mut();
                if config.plugins.iter().any(|p| same_source(p, &source)) {
                    return Err(mlua::Error::runtime("p.load_plugin: duplicate source"));
                }
                config.plugins.push(source);
                Ok(())
            })?,
        )?;
        let (c, reg, on) = (cfg.clone(), script.registrar(), loading.clone());
        module.set(
            "config",
            lua.create_function(move |_, t: Table| {
                if !on.get() {
                    return Err(mlua::Error::runtime("p.config: only while the config loads"));
                }
                apply(&mut c.borrow_mut(), &t, &reg).map_err(mlua::Error::external)
            })?,
        )?;
        let (c, reg, on) = (cfg.clone(), script.registrar(), loading.clone());
        module.set(
            "keymap",
            lua.create_function(move |_, (mode, key, action): (String, String, Value)| {
                if !on.get() {
                    return Err(mlua::Error::runtime("p.keymap: only while the config loads"));
                }
                let m = Mode::from_key(&mode).ok_or_else(|| {
                    mlua::Error::runtime(format!("p.keymap: unknown mode `{mode}` (normal, locked, pane, ...)"))
                })?;
                let bind = binding(&key, &action, &format!("p.keymap(\"{mode}\", \"{key}\")"), &reg)
                    .map_err(mlua::Error::runtime)?;
                c.borrow_mut().keys.binds.push((m, bind.0, bind.1));
                Ok(())
            })?,
        )?;
        Ok(())
    };
    setup().map_err(|e| e.to_string())?;
    script.exec(src, path)?;
    loading.set(false);
    let mut config = cfg.borrow().clone();
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let base = if base.is_absolute() {
        base.to_path_buf()
    } else {
        std::env::current_dir().map_err(|e| e.to_string())?.join(base)
    };
    for p in &mut config.plugins {
        if p.source == "~" || p.source.starts_with("~/") {
            let home = std::env::var_os("HOME").ok_or("HOME is needed for ~/ plugin paths")?;
            p.source = PathBuf::from(home).join(p.source.strip_prefix("~/").unwrap_or("")).display().to_string();
        } else if !p.source.contains("://") && !p.source.starts_with("git@") && !Path::new(&p.source).is_absolute() {
            p.source = base.join(&p.source).components().collect::<PathBuf>().display().to_string();
        }
    }
    for (i, source) in config.plugins.iter().enumerate() {
        if config.plugins[..i].iter().any(|p| same_source(p, source)) {
            return Err("p.load_plugin: duplicate source".into());
        }
    }
    Ok(Loaded { config, script })
}

/// A key binding: `key` = action name, "none"/false, or a Lua function.
fn binding(key: &str, action: &Value, name: &str, reg: &Registrar) -> Result<(Chord, Option<Action>), String> {
    let chord = Chord::parse(key).map_err(|e| format!("{name}: {e}"))?;
    let action = match action {
        Value::Boolean(false) => None,
        Value::Function(f) => Some(Action::Lua(reg.add(f.clone()).map_err(|e| e.to_string())?)),
        _ => match string(action, name)?.as_str() {
            "none" => None,
            a => Some(
                Action::from_name(a)
                    .ok_or_else(|| format!("{name}: unknown action `{a}` (see `proteus --list-actions`)"))?,
            ),
        },
    };
    Ok((chord, action))
}

fn apply(c: &mut Config, t: &Table, reg: &Registrar) -> Result<(), String> {
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| format!("config: {e}"))?;
        match k.as_str() {
            "language" => c.i18n.set_locale(&string(&v, "language")?)?,
            "plugins" => return Err("plugins: use p.load_plugin { source = ... } instead".into()),
            "theme" => {
                let Value::Table(t) = v else { return Err("theme: expected a table".into()) };
                c.theme.apply(t)?;
            }
            "window" => section(&v, "window", |k, v| {
                match k {
                    "opacity" => {
                        c.background_opacity = bounded(v, "window.opacity", 0.0, 1.0)? as f32;
                        return Ok(());
                    }
                    "background_blur" => {
                        c.background_blur = boolean(v, "window.background_blur")?;
                        return Ok(());
                    }
                    "decorations" => {
                        c.decorations = match string(v, "window.decorations")?.as_str() {
                            "auto" => Decorations::Auto,
                            "none" => Decorations::None,
                            "client" => Decorations::Client,
                            _ => return Err("window.decorations: expected \"auto\", \"client\" or \"none\"".into()),
                        };
                        return Ok(());
                    }
                    _ => {}
                }
                let field = match k {
                    "width" => &mut c.window.width,
                    "height" => &mut c.window.height,
                    "columns" => &mut c.window.columns,
                    "rows" => &mut c.window.rows,
                    _ => return Err(unknown("window", k)),
                };
                let min = match k {
                    "width" => 100.0,
                    "height" => 60.0,
                    "columns" => 2.0,
                    _ => 1.0,
                };
                let n = optional_number(v, &format!("window.{k}"), min, 8192.0)?;
                if n.is_some_and(|n| n.fract() != 0.0) {
                    return Err(format!("window.{k}: expected an integer"));
                }
                *field = n.map(|n| n as u32);
                Ok(())
            })?,
            "font" => section(&v, "font", |k, v| {
                match k {
                    "family" => c.font_family = string(v, "font.family")?,
                    "size" => c.font_size = bounded(v, "font.size", 4.0, 96.0)? as f32,
                    "fallback_families" => {
                        let families = strings(v, "font.fallback_families")?;
                        if families.len() > 32 || families.iter().any(|f| f.is_empty() || f.contains('\0')) {
                            return Err("font.fallback_families: expected at most 32 nonempty font names".into());
                        }
                        c.font_settings.fallback_families = families;
                    }
                    "bold_family" => c.font_settings.bold_family = optional_string(v, "font.bold_family")?,
                    "italic_family" => c.font_settings.italic_family = optional_string(v, "font.italic_family")?,
                    "bold_italic_family" => {
                        c.font_settings.bold_italic_family = optional_string(v, "font.bold_italic_family")?
                    }
                    "cell_width" => c.font_settings.cell_width = optional_number(v, "font.cell_width", 1.0, 512.0)?,
                    "cell_height" => c.font_settings.cell_height = optional_number(v, "font.cell_height", 1.0, 512.0)?,
                    "letter_spacing" => {
                        c.font_settings.letter_spacing = bounded(v, "font.letter_spacing", -64.0, 256.0)?
                    }
                    "line_spacing" => c.font_settings.line_spacing = bounded(v, "font.line_spacing", -64.0, 256.0)?,
                    "offset_x" => c.font_settings.offset_x = bounded(v, "font.offset_x", -256.0, 256.0)?,
                    "offset_y" => c.font_settings.offset_y = bounded(v, "font.offset_y", -256.0, 256.0)?,
                    "ligatures" => c.ligatures = boolean(v, "font.ligatures")?,
                    _ => return Err(unknown("font", k)),
                }
                Ok(())
            })?,
            "cursor" => section(&v, "cursor", |k, v| match k {
                "shape" => {
                    c.cursor_shape = match string(v, "cursor.shape")?.as_str() {
                        "app" => None,
                        "block" => Some(proteus_core::CursorShape::Block),
                        "bar" => Some(proteus_core::CursorShape::Bar),
                        "underline" => Some(proteus_core::CursorShape::Underline),
                        _ => return Err("cursor.shape: expected app, block, bar or underline".into()),
                    };
                    Ok(())
                }
                "blink" => {
                    c.cursor_blink = match v {
                        Value::Nil => Blink::App,
                        Value::Boolean(true) => Blink::Always,
                        Value::Boolean(false) => Blink::Never,
                        Value::String(s) if s.to_str().is_ok_and(|s| s == "app") => Blink::App,
                        _ => return Err("cursor.blink: expected true, false or \"app\"".into()),
                    };
                    Ok(())
                }
                _ => Err(unknown("cursor", k)),
            })?,
            "text" => section(&v, "text", |k, v| {
                match k {
                    "bidi" => {
                        c.bidi = match string(v, "text.bidi")?.as_str() {
                            "auto" => proteus_core::bidi::Mode::Auto,
                            "on" => proteus_core::bidi::Mode::On,
                            "off" => proteus_core::bidi::Mode::Off,
                            _ => return Err("text.bidi: expected auto, on or off".into()),
                        }
                    }
                    "grapheme_clustering" => c.grapheme_clustering = boolean(v, "text.grapheme_clustering")?,
                    "ambiguous_width" => {
                        c.ambiguous_wide = match number(v, "text.ambiguous_width")? as i64 {
                            1 => false,
                            2 => true,
                            _ => return Err("text.ambiguous_width: expected 1 or 2".into()),
                        }
                    }
                    _ => return Err(unknown("text", k)),
                }
                Ok(())
            })?,
            "clipboard" => section(&v, "clipboard", |k, v| {
                match k {
                    "osc52_write" => c.osc52_write = boolean(v, "clipboard.osc52_write")?,
                    "osc52_read" => {
                        c.osc52_read = match string(v, "clipboard.osc52_read")?.as_str() {
                            "ask" => ClipboardRead::Ask,
                            "allow" => ClipboardRead::Allow,
                            "deny" => ClipboardRead::Deny,
                            _ => return Err("clipboard.osc52_read: expected \"ask\", \"allow\" or \"deny\"".into()),
                        }
                    }
                    "paste_protection" => c.paste_protection = boolean(v, "clipboard.paste_protection")?,
                    _ => return Err(unknown("clipboard", k)),
                }
                Ok(())
            })?,
            "ipc" => section(&v, "ipc", |k, v| {
                match k {
                    "socket" => {
                        let path = string(v, "ipc.socket")?;
                        if path.is_empty() || path.contains('\0') {
                            return Err("ipc.socket: expected a socket path".into());
                        }
                        c.ipc_socket = Some(PathBuf::from(path));
                    }
                    _ => return Err(unknown("ipc", k)),
                }
                Ok(())
            })?,
            "shell" => section(&v, "shell", |k, v| {
                match k {
                    "command" => {
                        c.command = match v {
                            Value::Nil => None,
                            Value::String(s) => {
                                Some(vec![OsString::from(s.to_str().map_err(|e| e.to_string())?.to_string())])
                            }
                            Value::Table(t) => {
                                let argv = t
                                    .sequence_values::<String>()
                                    .collect::<mlua::Result<Vec<_>>>()
                                    .map_err(|_| "shell.command: expected a list of strings".to_string())?;
                                (!argv.is_empty()).then(|| argv.into_iter().map(OsString::from).collect())
                            }
                            _ => return Err("shell.command: expected a string or a list of strings".into()),
                        }
                    }
                    "term" => {
                        let term = string(v, "shell.term")?;
                        if term.is_empty()
                            || term.len() > 128
                            || !term.bytes().all(|b| b.is_ascii_alphanumeric() || b"-+_.".contains(&b))
                        {
                            return Err("shell.term: expected a terminal type name".into());
                        }
                        c.term = Some(term);
                    }
                    "integration" => c.shell_integration = boolean(v, "shell.integration")?,
                    _ => return Err(unknown("shell", k)),
                }
                Ok(())
            })?,
            "ui" => section(&v, "ui", |k, v| {
                match k {
                    "hint_bar" => {
                        c.hint_bar = match string(v, "ui.hint_bar")?.as_str() {
                            "in-mode" => HintBar::InMode,
                            "always" => HintBar::Always,
                            "never" => HintBar::Never,
                            _ => return Err("ui.hint_bar: expected \"in-mode\", \"always\" or \"never\"".into()),
                        }
                    }
                    "tab_bar" => {
                        c.tab_bar = match string(v, "ui.tab_bar")?.as_str() {
                            "auto" => TabBar::Auto,
                            "always" => TabBar::Always,
                            "never" => TabBar::Never,
                            _ => return Err("ui.tab_bar: expected \"auto\", \"always\" or \"never\"".into()),
                        }
                    }
                    "pane_titles" => {
                        c.pane_titles = match string(v, "ui.pane_titles")?.as_str() {
                            "auto" => PaneTitles::Auto,
                            "always" => PaneTitles::Always,
                            "never" => PaneTitles::Never,
                            _ => return Err("ui.pane_titles: expected \"auto\", \"always\" or \"never\"".into()),
                        }
                    }
                    "padding" => {
                        if let Value::Table(t) = v {
                            section(&Value::Table(t.clone()), "ui.padding", |k, v| {
                                match k {
                                    "horizontal" => c.padding = bounded(v, "ui.padding.horizontal", 0.0, 256.0)?,
                                    "vertical" => c.padding_vertical = bounded(v, "ui.padding.vertical", 0.0, 256.0)?,
                                    _ => return Err(unknown("ui.padding", k)),
                                }
                                Ok(())
                            })?;
                        } else {
                            c.padding = bounded(v, "ui.padding", 0.0, 256.0)?;
                            c.padding_vertical = c.padding;
                        }
                    }
                    "pane_gap" => c.pane_gap = optional_number(v, "ui.pane_gap", 0.0, 256.0)?,
                    "border_width" => c.border_width = bounded(v, "ui.border_width", 0.0, 32.0)?,
                    "gutter" => c.gutter = number(v, "ui.gutter")?.clamp(0.0, 2.0) as f32,
                    _ => return Err(unknown("ui", k)),
                }
                Ok(())
            })?,
            "keys" => section(&v, "keys", |k, v| {
                match k {
                    "preset" => {
                        c.keys.preset = match string(v, "keys.preset")?.as_str() {
                            "zellij" => Preset::Zellij,
                            "leader" => Preset::Leader,
                            "tmux" => Preset::Tmux,
                            _ => return Err("keys.preset: expected \"zellij\", \"leader\" or \"tmux\"".into()),
                        }
                    }
                    "leader" => {
                        c.keys.leader =
                            Some(Chord::parse(&string(v, "keys.leader")?).map_err(|e| format!("keys.leader: {e}"))?)
                    }
                    "sticky" => c.keys.sticky = boolean(v, "keys.sticky")?,
                    m => {
                        let mode = Mode::from_key(m).ok_or_else(|| unknown("keys", m))?;
                        let name = format!("keys.{m}");
                        section(v, &name, |key, action| {
                            let (chord, action) = binding(key, action, &format!("{name}[\"{key}\"]"), reg)?;
                            c.keys.binds.push((mode, chord, action));
                            Ok(())
                        })?;
                    }
                }
                Ok(())
            })?,
            "panes" => section(&v, "panes", |k, v| {
                match k {
                    "confirm_close" => c.confirm_close = boolean(v, "panes.confirm_close")?,
                    "keep_last_shell" => c.keep_last_shell = boolean(v, "panes.keep_last_shell")?,
                    "confirm_close_ignore" => c.confirm_close_ignore = strings(v, "panes.confirm_close_ignore")?,
                    _ => return Err(unknown("panes", k)),
                }
                Ok(())
            })?,
            "session" => section(&v, "session", |k, v| {
                match k {
                    "restore" => {
                        c.restore = match string(v, "session.restore")?.as_str() {
                            "ask" => Restore::Ask,
                            "always" => Restore::Always,
                            "never" => Restore::Never,
                            _ => return Err("session.restore: expected \"ask\", \"always\" or \"never\"".into()),
                        }
                    }
                    "restore_commands" => c.restore_commands = strings(v, "session.restore_commands")?,
                    _ => return Err(unknown("session", k)),
                }
                Ok(())
            })?,
            "scrollback" => c.scrollback = number(&v, "scrollback")?.max(0.0) as usize,
            "notify" => section(&v, "notify", |k, v| {
                match k {
                    "min_duration" => {
                        c.notify_after = match v {
                            Value::Boolean(false) => None,
                            _ => Some(std::time::Duration::from_secs_f64(number(v, "notify.min_duration")?.max(0.0))),
                        }
                    }
                    "osc" => c.notify_osc = boolean(v, "notify.osc")?,
                    _ => return Err(unknown("notify", k)),
                }
                Ok(())
            })?,
            "bell" => section(&v, "bell", |k, v| {
                match k {
                    "visual" => c.visual_bell = boolean(v, "bell.visual")?,
                    _ => return Err(unknown("bell", k)),
                }
                Ok(())
            })?,
            "hints" => section(&v, "hints", |k, v| {
                match k {
                    "alphabet" => {
                        let a = string(v, "hints.alphabet")?;
                        let mut chars: Vec<char> = a.chars().collect();
                        chars.sort();
                        chars.dedup();
                        if chars.len() < 2 || chars.iter().any(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit()) {
                            return Err("hints.alphabet: expected at least 2 lowercase letters or digits".into());
                        }
                        c.hint_alphabet = a;
                    }
                    "patterns" => c.hint_patterns = hint_patterns(v)?,
                    _ => return Err(unknown("hints", k)),
                }
                Ok(())
            })?,
            "graphics" => section(&v, "graphics", |k, v| {
                let mib = |v: &Value, name: &str| -> Result<usize, String> {
                    let n = number(v, name)?;
                    if !(1.0..=1048576.0).contains(&n) {
                        return Err(format!("{name}: expected a size in MiB (1 or more)"));
                    }
                    Ok((n * 1048576.0) as usize)
                };
                let g = &mut c.graphics;
                match k {
                    "enabled" => g.enabled = boolean(v, "graphics.enabled")?,
                    "max_image" => g.max_image = mib(v, "graphics.max_image")?,
                    "pane_memory" => g.pane_memory = mib(v, "graphics.pane_memory")?,
                    "total_memory" => g.total_memory = mib(v, "graphics.total_memory")?,
                    "gpu_memory" => g.gpu_memory = mib(v, "graphics.gpu_memory")?,
                    "local_files" => g.local_files = boolean(v, "graphics.local_files")?,
                    _ => return Err(unknown("graphics", k)),
                }
                Ok(())
            })?,
            _ => return Err(format!("unknown config key `{k}`")),
        }
    }
    Ok(())
}

/// `hints.patterns`: a list of `{ name, regex, url }` tables.
fn hint_patterns(v: &Value) -> Result<Vec<HintPatternConfig>, String> {
    let Value::Table(t) = v else { return Err("hints.patterns: expected a list".into()) };
    let mut out = Vec::new();
    for (i, p) in t.sequence_values::<Value>().enumerate() {
        let p = p.map_err(|e| format!("hints.patterns: {e}"))?;
        let name = format!("hints.patterns[{}]", i + 1);
        let mut hp = HintPatternConfig { name: String::new(), regex: String::new(), url: None };
        section(&p, &name, |k, v| {
            match k {
                "name" => hp.name = string(v, &format!("{name}.name"))?,
                "regex" => hp.regex = string(v, &format!("{name}.regex"))?,
                "url" => hp.url = Some(string(v, &format!("{name}.url"))?),
                _ => return Err(format!("unknown config key `{name}.{k}`")),
            }
            Ok(())
        })?;
        if hp.regex.is_empty() {
            return Err(format!("{name}: `regex` is required"));
        }
        if let Err(e) = regex::Regex::new(&hp.regex) {
            return Err(format!("{name}.regex: {}", e.to_string().lines().last().unwrap_or("invalid")));
        }
        if hp.name.is_empty() {
            hp.name = format!("pattern{}", i + 1);
        }
        out.push(hp);
    }
    Ok(out)
}

fn section(v: &Value, name: &str, mut f: impl FnMut(&str, &Value) -> Result<(), String>) -> Result<(), String> {
    let Value::Table(t) = v else { return Err(format!("{name}: expected a table")) };
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| format!("{name}: {e}"))?;
        f(&k, &v)?;
    }
    Ok(())
}

fn unknown(section: &str, k: &str) -> String {
    format!("unknown config key `{section}.{k}`")
}

fn string(v: &Value, name: &str) -> Result<String, String> {
    match v {
        Value::String(s) => s.to_str().map(|s| s.to_string()).map_err(|_| format!("{name}: not valid UTF-8")),
        _ => Err(format!("{name}: expected a string")),
    }
}

fn number(v: &Value, name: &str) -> Result<f64, String> {
    match v {
        Value::Integer(i) => Ok(*i as f64),
        Value::Number(n) => Ok(*n),
        _ => Err(format!("{name}: expected a number")),
    }
}

/// One explicit WASM load request. Lua modules use ordinary `require`.
fn plugin_source(lua: &mlua::Lua, v: &Value) -> Result<proteus_wasm::source::Source, String> {
    use proteus_wasm::source::Source;
    let Value::Table(t) = v else { return Err("p.load_plugin: expected a plugin config table".into()) };
    let mut p = Source::default();
    let mut location = None;
    for pair in t.pairs::<String, Value>() {
        let (key, value) = pair.map_err(|e| format!("p.load_plugin: {e}"))?;
        match key.as_str() {
            "source" | "path" | "url" => {
                if location.is_some() {
                    return Err("p.load_plugin: specify only one of source, path or url".into());
                }
                location = Some(string(&value, "plugin source")?);
            }
            "sha256" => p.sha256 = Some(string(&value, "plugin sha256")?),
            "rev" => p.rev = Some(string(&value, "plugin rev")?),
            "subdir" => p.subdir = Some(PathBuf::from(string(&value, "plugin subdir")?)),
            "id" => p.id = Some(string(&value, "plugin id")?),
            "config" => {
                use mlua::LuaSerdeExt;
                let Value::Table(_) = &value else { return Err("plugin config: expected a table".into()) };
                p.config = lua.from_value(value).map_err(|e| format!("plugin config: {e}"))?;
            }
            "permissions" => p.permissions = Some(config_permissions(&value)?),
            _ => return Err(format!("p.load_plugin: unknown key `{key}`")),
        }
    }
    p.source = location.ok_or("p.load_plugin: source, path or url is required")?;
    if p.source.is_empty() || p.source.contains('\0') {
        return Err("p.load_plugin: source must be nonempty".into());
    }
    if p.source.split(['?', '#']).next().is_some_and(|s| s.ends_with(".lua")) {
        return Err("Lua is config code: load modules with require(), not p.load_plugin".into());
    }
    if p.id.as_ref().is_some_and(|id| !proteus_wasm::manifest::valid_plugin_id(id)) {
        return Err("p.load_plugin: invalid id".into());
    }
    if p.sha256.as_ref().is_some_and(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err("plugin sha256 must contain exactly 64 hexadecimal digits".into());
    }
    if p.rev.as_ref().is_some_and(|r| r.is_empty() || r.starts_with('-') || r.contains('\0')) {
        return Err("p.load_plugin: invalid Git rev".into());
    }
    if p.subdir
        .as_ref()
        .is_some_and(|p| p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)))
    {
        return Err("p.load_plugin: subdir must stay within the source".into());
    }
    Ok(p)
}
fn same_source(a: &proteus_wasm::source::Source, b: &proteus_wasm::source::Source) -> bool {
    a.source == b.source && a.rev == b.rev && a.subdir == b.subdir
}

fn config_permissions(v: &Value) -> Result<proteus_wasm::Permissions, String> {
    let Value::Table(t) = v else { return Err("plugin permissions: expected a table".into()) };
    let mut out = toml::Table::new();
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| e.to_string())?;
        let value = match v {
            Value::Boolean(b) => toml::Value::Boolean(b),
            Value::Table(t) => {
                let mut values = std::collections::BTreeMap::new();
                for pair in t.pairs::<Value, Value>() {
                    let (i, v) = pair.map_err(|e| e.to_string())?;
                    let Value::Integer(i @ 1..) = i else {
                        return Err("plugin permission lists must be contiguous".into());
                    };
                    let Value::String(v) = v else { return Err("plugin permission lists contain strings".into()) };
                    values.insert(i, toml::Value::String(v.to_str().map_err(|e| e.to_string())?.to_string()));
                }
                if values.keys().copied().ne(1..=values.len() as i64) {
                    return Err("plugin permission list has holes".into());
                }
                toml::Value::Array(values.into_values().collect())
            }
            _ => return Err(format!("plugin permission `{k}`: expected boolean or string list")),
        };
        out.insert(k, value);
    }
    toml::Value::Table(out).try_into().map_err(|e| format!("plugin permissions: {e}"))
}

fn bounded(v: &Value, name: &str, min: f64, max: f64) -> Result<f64, String> {
    let n = number(v, name)?;
    if !n.is_finite() || !(min..=max).contains(&n) {
        return Err(format!("{name}: expected a finite number from {min} to {max}"));
    }
    Ok(n)
}
fn optional_number(v: &Value, name: &str, min: f64, max: f64) -> Result<Option<f64>, String> {
    if matches!(v, Value::Boolean(false)) { Ok(None) } else { bounded(v, name, min, max).map(Some) }
}
fn optional_string(v: &Value, name: &str) -> Result<Option<String>, String> {
    if matches!(v, Value::Boolean(false)) {
        return Ok(None);
    }
    let s = string(v, name)?;
    if s.is_empty() || s.contains('\0') {
        return Err(format!("{name}: expected a nonempty font family"));
    }
    Ok(Some(s))
}

fn strings(v: &Value, name: &str) -> Result<Vec<String>, String> {
    let err = || format!("{name}: expected a list of strings");
    let Value::Table(t) = v else { return Err(err()) };
    t.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>().map_err(|_| err())
}

fn boolean(v: &Value, name: &str) -> Result<bool, String> {
    match v {
        Value::Boolean(b) => Ok(*b),
        _ => Err(format!("{name}: expected true or false")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(src: &str) -> Result<Config, String> {
        eval(src, Path::new("/nonexistent/init.lua")).map(|l| l.config)
    }

    #[test]
    fn bidi_modes_are_validated_and_reach_terminal_configuration() {
        for (value, mode) in [
            ("auto", proteus_core::bidi::Mode::Auto),
            ("on", proteus_core::bidi::Mode::On),
            ("off", proteus_core::bidi::Mode::Off),
        ] {
            let loaded =
                eval(&format!("require('proteus').config {{text={{bidi='{value}'}}}}"), Path::new("/tmp/init.lua"))
                    .unwrap();
            assert_eq!(loaded.config.bidi, mode);
            assert_eq!(loaded.config.core().bidi, mode);
        }
        assert!(eval("require('proteus').config {text={bidi='invalid'}}", Path::new("/tmp/init.lua")).is_err());
    }

    #[test]
    fn control_socket_is_explicit_and_validated() {
        assert!(Config::default().ipc_socket.is_none());
        let loaded =
            eval("local p=require('proteus'); p.config {ipc={socket='/tmp/test.sock'}}", Path::new("/tmp/init.lua"))
                .unwrap();
        assert_eq!(loaded.config.ipc_socket.as_deref(), Some(Path::new("/tmp/test.sock")));
        for value in ["''", "'a'..string.char(0)..'b'"] {
            assert!(
                eval(&format!("require('proteus').config {{ipc={{socket={value}}}}}"), Path::new("/tmp/init.lua"))
                    .is_err()
            );
        }
    }

    #[test]
    fn terminal_type_and_artifact_pins_are_validated() {
        let loaded = eval("local p=require('proteus'); p.config {shell={term='xterm-256color'}}; p.load_plugin {source='demo.wasm', sha256=string.rep('a',64)}", Path::new("/tmp/init.lua")).unwrap();
        assert_eq!(loaded.config.term.as_deref(), Some("xterm-256color"));
        assert_eq!(loaded.config.plugins[0].sha256.as_deref(), Some("a".repeat(64).as_str()));
        for src in [
            "p.config {shell={term='../bad'}}",
            "p.config {shell={term=''}}",
            "p.load_plugin {source='x.wasm',sha256='xyz'}",
        ] {
            assert!(eval(&format!("local p=require('proteus'); {src}"), Path::new("/tmp/init.lua")).is_err());
        }
    }

    #[test]
    fn lua_fluent_resources_use_config_locale_and_typed_arguments() {
        let c = ev(r#"
            local p = require('proteus')
            p.config { language = 'zh_CN.UTF-8' }
            p.i18n.load('en', [[
hello = Hello { $name }
files = { $count ->
    [one] { $count } file
   *[other] { $count } files
}
            ]])
            p.i18n.load('zh', [[hello = 你好 { $name }]])
            assert(p.i18n.locale() == 'zh-CN')
            assert(p.i18n.t('hello', {name = '世界'}) == '你好 世界')
            assert(p.i18n.t('files', {count = 2}) == '2 files')
            assert(p.i18n.t('files', {count = 1}) == '1 file')
            assert(p.i18n.t('files', {count = '1'}) == '1 files')
            assert(not pcall(p.i18n.t, 'unknown'))
            assert(not pcall(p.i18n.t, 'hello'))
            p.on('config_changed', function() assert(p.i18n.t('hello', {name = 'Lua'}) == '你好 Lua') end)
        "#)
        .unwrap();
        assert_eq!(c.i18n.locale(), "zh-CN");
        assert_eq!(c.i18n.t("ui-split-right", &[]), "向右分割");
        assert!(ev("require('proteus').config { language = '../bad' }").is_err());
        assert!(ev("require('proteus').i18n.load('en', 'broken = {')").is_err());
        assert!(ev("require('proteus').i18n.load('en', 'hello = one\\nhello = two')").is_err());
    }

    #[test]
    fn lua_bindings() {
        let l = eval(
            r#"
            local p = require("proteus")
            p.keymap("pane", "g", function(ctx) end)
            p.keymap("normal", "alt+x", "pane.close")
            p.keymap("normal", "alt+t", false)
            p.config { keys = { tab = { ["shift+g"] = function() end } } }
            later = function() p.keymap("normal", "alt+y", "pane.close") end
        "#,
            Path::new("/nonexistent/init.lua"),
        )
        .unwrap();
        let b = &l.config.keys.binds;
        assert_eq!(b[0], (Mode::Pane, Chord::parse("g").unwrap(), Some(Action::Lua(0))));
        assert_eq!(b[1], (Mode::Normal, Chord::parse("alt+x").unwrap(), Some(Action::ClosePane)));
        assert_eq!(b[2], (Mode::Normal, Chord::parse("alt+t").unwrap(), None));
        assert_eq!(b[3], (Mode::Tab, Chord::parse("G").unwrap(), Some(Action::Lua(1))));
        // Only while loading.
        let e = l.script.lua().load("later()").exec().unwrap_err();
        assert!(crate::script::clean(&e).contains("only while the config loads"), "{e}");
        for (src, want) in [
            (r#"keymap("insert", "a", "copy")"#, "unknown mode `insert`"),
            (r#"keymap("normal", "hyper+a", "copy")"#, "p.keymap(\"normal\", \"hyper+a\")"),
            (r#"keymap("normal", "a", "explode")"#, "unknown action `explode`"),
        ] {
            let e = ev(&format!(r#"require("proteus").{src}"#)).unwrap_err();
            assert!(e.contains(want), "{e}");
        }
    }

    #[test]
    fn themes_merge_and_validate() {
        let c = ev(r##"
            local p = require("proteus")
            p.config { theme = { preset = "light", foreground = "#123456",
                ansi = {"#000", "#111", "#222", "#333", "#444", "#555", "#666", "#777",
                        "#888", "#999", "#aaa", "#bbb", "#ccc", "#ddd", "#eee", "#fff"},
                indexed = {[4] = "#abcdef", [255] = "#fedcba"},
                ui = {accent = "#fa0", selection_background = "#345"} } }
            p.config { theme = { background = "#101010", ui = {accent = false} },
                       ui = { padding = 12 }, cursor = {shape = "bar"} }
            p.on("theme_changed", function() end)
        "##)
        .unwrap();
        let p = c.theme.palette();
        use proteus_core::color::Rgb;
        assert_eq!(p.foreground(), Rgb::new(0x12, 0x34, 0x56));
        assert_eq!(p.background(), Rgb::new(16, 16, 16));
        assert_eq!(p.indexed(4), Rgb::new(0xab, 0xcd, 0xef));
        assert_eq!(p.indexed(255), Rgb::new(0xfe, 0xdc, 0xba));
        assert_eq!(p.indexed(15), Rgb::new(255, 255, 255));
        assert_eq!(c.theme.ui.accent, None);
        assert_eq!(c.padding, 12.0);
        assert_eq!(c.cursor_shape, Some(proteus_core::CursorShape::Bar));
        let reset = ev(r#"require("proteus").config{theme={preset="light"}}
                         require("proteus").config{theme={preset="default"}}"#)
        .unwrap();
        assert_eq!(reset.theme, crate::theme::Theme::default());
        for src in [
            r##"theme={foreground="red"}"##,
            r##"theme={background="#ééé"}"##,
            r##"theme={preset="unknown"}"##,
            r##"theme={ansi={"#fff"}}"##,
            r##"theme={indexed={[256]="#fff"}}"##,
            r##"theme={ui={typo="#fff"}}"##,
            r##"theme={typo=true}"##,
            r##"ui={padding=-1}"##,
            r##"ui={padding=0/0}"##,
            r##"cursor={shape="triangle"}"##,
        ] {
            assert!(ev(&format!("require(\"proteus\").config{{{src}}}")).is_err(), "{src}");
        }
    }

    #[test]
    fn typography_and_window_geometry() {
        let c = ev(r#"
            local p = require("proteus")
            p.config {
                font = { family = "monospace", bold_family = "serif", italic_family = "sans-serif",
                         bold_italic_family = "monospace", cell_width = 10, cell_height = 20,
                         line_spacing = 2, letter_spacing = 1, offset_x = -1, offset_y = 3 },
                window = { columns = 100, rows = 30 },
                ui = { padding = {horizontal = 12, vertical = 8}, pane_gap = 6, border_width = 2,
                       tab_bar = "always", hint_bar = "always", pane_titles = "always", gutter = 0 },
            }
        "#)
        .unwrap();
        assert_eq!(c.font_settings.cell_width, Some(10.0));
        assert_eq!(c.font_settings.bold_family.as_deref(), Some("serif"));
        assert_eq!(c.font_settings.offset_x, -1.0);
        assert_eq!(c.pane_gap, Some(6.0));
        assert_eq!(c.border_width, 2.0);
        assert_eq!(c.window_size((11, 22), 1.0), (1124, 758));
        assert_eq!(c.window_size((22, 44), 2.0), (1124, 758));
        let c = ev(r#"local p = require("proteus")
            p.config{window={width=1200, height=800, columns=100, rows=40}}
            p.config{window={width=false}, font={cell_width=10, bold_family="serif"}}
            p.config{font={cell_width=false, bold_family=false}, ui={padding=5}}"#)
        .unwrap();
        assert_eq!(c.window_size((10, 20), 1.0), (1011, 800));
        assert_eq!(c.font_settings.cell_width, None);
        assert_eq!(c.font_settings.bold_family, None);
        assert_eq!(c.padding_vertical, 5.0);
        let c =
            ev(r#"require("proteus").config{window={opacity=0.9, background_blur=true, decorations="none"}}"#).unwrap();
        assert_eq!((c.background_opacity, c.background_blur, c.decorations), (0.9, true, Decorations::None));
        assert_eq!(c.window, Default::default(), "window style does not change initial sizing");
        let c = ev(r#"require("proteus").config{window={decorations="client"}}"#).unwrap();
        assert_eq!(c.decorations, Decorations::Client);
        let plain = Config::default().window_size((10, 20), 1.0);
        assert_eq!(c.window_size((10, 20), 1.0), (plain.0, plain.1 + 24), "room for the title bar");
        for src in [
            "window={opacity=1.5}",
            "window={opacity='x'}",
            "window={decorations='server'}",
            "window={background_blur=1}",
            "font={size=0/0}",
            "font={size=1/0}",
            "font={cell_width=0}",
            "font={line_spacing=-100}",
            "font={offset_x=1/0}",
            "font={bold_family=''}",
            "window={width=99}",
            "window={height=1.5}",
            "window={rows=0}",
            "window={columns=2.5}",
            "window={typo=10}",
            "ui={padding={left=5}}",
            "ui={border_width=-1}",
            "ui={pane_gap=-1}",
        ] {
            assert!(ev(&format!("require(\"proteus\").config{{{src}}}")).is_err(), "{src}");
        }
    }

    #[test]
    fn shipped_appearance_example_is_valid() {
        let c = ev(include_str!("../../../examples/appearance.lua")).unwrap();
        assert_eq!(c.window_size((10, 20), 1.0), (1200, 800));
        assert_eq!(c.font_settings.line_spacing, 2.0);
        assert_eq!(c.padding_vertical, 8.0);
    }

    #[test]
    fn plugin_owned_config_is_data_and_part_of_instance_identity() {
        let c = ev(r#"require('proteus').load_plugin {
            source='./files', config={position='right', width=40,
                enabled=true, nested={patterns={'*.rs', '*.lua'}}}
        }"#)
        .unwrap();
        assert_eq!(
            c.plugins[0].config,
            serde_json::json!({
                "position":"right", "width":40, "enabled":true,
                "nested":{"patterns":["*.rs", "*.lua"]}
            })
        );
        let mut changed = c.plugins[0].clone();
        changed.config["width"] = 50.into();
        assert_ne!(changed, c.plugins[0]);
        assert!(same_source(&changed, &c.plugins[0]));
        assert_eq!(
            ev("require('proteus').load_plugin{source='./files',config={}}").unwrap().plugins[0].config,
            serde_json::json!({})
        );
        for bad in ["function() end", "{callback=function() end}", "(function() local t={} t.self=t return t end)()"] {
            assert!(ev(&format!("require('proteus').load_plugin{{source='./files',config={bad}}}")).is_err());
        }
    }

    #[test]
    fn wasm_sources_are_explicit_and_lua_is_ordinary_config() {
        let c = ev(r#"local p = require("proteus")
            p.load_plugin { source = "./plugins/demo" }
            p.load_plugin { url = "https://example.com/demo.git" }
            p.load_plugin { path = "./demo.wasm", id = "local.demo", permissions = { ui = true, ["read-panes"] = true, fs = {"/:r"} } }
        "#).unwrap();
        assert_eq!(c.plugins[0].source, "/nonexistent/plugins/demo");
        assert_eq!(c.plugins[1].source, "https://example.com/demo.git");
        assert!(c.plugins[2].permissions.as_ref().unwrap().read_panes);
        assert_eq!(c.plugins[2].permissions.as_ref().unwrap().fs[0].path, "/");
        let resolved = proteus_wasm::Resolved {
            id: "local.demo".into(),
            name: "Demo".into(),
            version: "1".into(),
            source: c.plugins[2].clone(),
            hash: "unused".into(),
            granted: Default::default(),
            wasm: std::sync::Arc::new(Vec::new()),
        };
        assert!(c.plugin_should_load(&resolved));
        let mut next = c.clone();
        next.plugins.clear();
        assert!(!next.plugin_should_load(&resolved));
        for src in [
            r#"'x'"#,
            r#"false"#,
            r#"{}"#,
            r#"{source=''}"#,
            r#"{source='module.lua'}"#,
            r#"{[2]='./x'}"#,
            r#"{x='./x'}"#,
            r#"{source='./x',path='./y'}"#,
            r#"{source='',path='./y'}"#,
            r#"{source='./x',id='../bad'}"#,
            r#"{source='./x',subdir='../other'}"#,
            r#"{source='./x',rev='--bad'}"#,
            r#"{source='./x',permissions={unknown=true}}"#,
        ] {
            assert!(ev(&format!("require('proteus').load_plugin({src})")).is_err(), "{src}");
        }
        for src in [
            "local p=require('proteus'); p.load_plugin{path='./x'}; p.load_plugin{source='./x'}",
            "local p=require('proteus'); p.load_plugin{path='./x'}; p.load_plugin{source='x'}",
            "require('proteus').config{plugins={}}",
        ] {
            assert!(ev(src).is_err(), "{src}");
        }
        let loaded = eval(
            "local p=require('proteus'); later=function() p.load_plugin{path='./x'} end",
            Path::new("/nonexistent/init.lua"),
        )
        .unwrap();
        let error = loaded.script.lua().load("later()").exec().unwrap_err();
        assert!(error.to_string().contains("only while the config loads"));
        assert!(Config::default().plugins.is_empty());
    }

    #[test]
    fn config_modules_share_inline_apis_with_explicit_wasm_source() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/config/init.lua");
        let loaded = load(&path, true).unwrap();
        assert_eq!(loaded.config.plugins.len(), 1);
        let source = Path::new(&loaded.config.plugins[0].source);
        assert!(source.is_absolute());
        assert_eq!(
            source.canonicalize().unwrap(),
            path.parent().unwrap().join("../../plugins/files").canonicalize().unwrap()
        );
        assert!(source.join("Cargo.toml").is_file());
        assert_eq!(loaded.config.font_size, 13.0);
        assert!(loaded.script.has_segments());
        assert!(loaded.script.has_hooks("command_finished"));
        assert!(
            loaded.config.keys.binds.iter().any(
                |(_, key, action)| *key == Chord::parse("alt+i").unwrap() && matches!(action, Some(Action::Lua(_)))
            )
        );
    }

    #[test]
    fn plugin_action_ids_use_the_builtin_keymap_configuration_path() {
        let c = ev(r#"local p=require("proteus")
            p.config{keys={normal={["alt+e"]=false,["ctrl+alt+e"]="plugin:proteus.files:sidebar.toggle"}}}
            p.keymap("pane","E","plugin:proteus.files:pane.open")"#)
        .unwrap();
        let sidebar = Action::from_name("plugin:proteus.files:sidebar.toggle").unwrap();
        let map = crate::modes::Keymap::with_plugin_defaults(
            &c.keys,
            &[(Mode::Normal, Chord::parse("alt+e").unwrap(), sidebar)],
        );
        assert!(!map.bindings(Mode::Normal).iter().any(|(chord, _)| *chord == Chord::parse("alt+e").unwrap()));
        assert!(map.bindings(Mode::Normal).contains(&(Chord::parse("ctrl+alt+e").unwrap(), sidebar)));
        assert!(
            map.bindings(Mode::Pane)
                .contains(&(Chord::parse("E").unwrap(), Action::from_name("plugin:proteus.files:pane.open").unwrap()))
        );
    }

    #[test]
    fn empty_is_default() {
        assert_eq!(ev("").unwrap(), Config::default());
    }

    #[test]
    fn full_config() {
        let c = ev(r#"
            local p = require("proteus")
            p.config { font = { family = "Fira Code", size = 13.5, ligatures = false } }
            p.config {
              cursor = { blink = false },
              text = { grapheme_clustering = true, ambiguous_width = 2 },
              clipboard = { osc52_read = "deny", osc52_write = false, paste_protection = false },
              shell = { command = { "fish", "-l" }, integration = false },
              scrollback = 500,
            }
            p.config { font = { size = 14 } } -- later calls win, per key
        "#)
        .unwrap();
        assert_eq!(c.font_family, "Fira Code");
        assert_eq!(c.font_size, 14.0);
        assert!(!c.ligatures);
        assert_eq!(c.cursor_blink, Blink::Never);
        assert!(c.grapheme_clustering && c.ambiguous_wide);
        assert_eq!(c.osc52_read, ClipboardRead::Deny);
        assert!(!c.osc52_write && !c.paste_protection && !c.shell_integration);
        assert_eq!(c.command, Some(vec!["fish".into(), "-l".into()]));
        assert_eq!(c.scrollback, 500);
        let core = c.core();
        assert!(core.grapheme_clustering && core.ambiguous_wide && core.scrollback == 500);
    }

    #[test]
    fn errors_are_reported() {
        let e = ev(r#"require("proteus").config { font = { famly = "x" } }"#).unwrap_err();
        assert!(e.contains("font.famly"), "{e}");
        let e = ev(r#"require("proteus").config { font = { size = "big" } }"#).unwrap_err();
        assert!(e.contains("font.size: expected a number"), "{e}");
        let e = ev(r#"require("proteus").config { cursor = { blink = 3 } }"#).unwrap_err();
        assert!(e.contains("cursor.blink"), "{e}");
        let e = ev("this is not lua").unwrap_err();
        assert!(e.contains("init.lua"), "{e}");
    }

    #[test]
    fn mux_config() {
        let c = ev(r#"require("proteus").config {
            ui = { hint_bar = "always", tab_bar = "never", pane_titles = "auto" },
            panes = { confirm_close = false, confirm_close_ignore = { "less" }, keep_last_shell = true },
            session = { restore = "always", restore_commands = { "nvim" } },
            keys = {
              preset = "leader", leader = "ctrl+space", sticky = true,
              normal = { ["alt+x"] = "pane.close", ["alt+t"] = "none", ["alt+n"] = false },
              pane = { V = "pane.split_down" },
            },
        }"#)
        .unwrap();
        let k = &c.keys;
        assert_eq!(k.preset, Preset::Leader);
        assert_eq!(k.leader, Some(Chord::parse("ctrl+space").unwrap()));
        assert!(k.sticky && !c.confirm_close);
        assert_eq!(c.hint_bar, HintBar::Always);
        assert_eq!(c.tab_bar, TabBar::Never);
        assert_eq!(c.pane_titles, PaneTitles::Auto);
        assert!(c.keep_last_shell);
        assert_eq!(c.confirm_close_ignore, ["less"]);
        assert_eq!((c.restore, c.restore_commands.clone()), (Restore::Always, vec!["nvim".to_string()]));
        assert_eq!(k.binds.len(), 4);
        assert!(k.binds.contains(&(Mode::Normal, Chord::parse("alt+x").unwrap(), Some(Action::ClosePane))));
        assert!(k.binds.contains(&(Mode::Normal, Chord::parse("alt+n").unwrap(), None)));
        assert!(k.binds.contains(&(
            Mode::Pane,
            Chord::parse("shift+v").unwrap(),
            Some(Action::Split(crate::layout::Axis::Y))
        )));
        assert_eq!(Config::default().keys.preset, Preset::Zellij);
        for (src, want) in [
            (r#"keys = { leader = "ctrl+nope" }"#, "keys.leader"),
            (r#"keys = { normal = { ["alt+x"] = "pane.explode" } }"#, "unknown action `pane.explode`"),
            (r#"keys = { normal = { ["hyper+x"] = "pane.close" } }"#, "keys.normal"),
            (r#"keys = { insert = {} }"#, "keys.insert"),
            (r#"ui = { tab_bar = "top" }"#, "ui.tab_bar"),
        ] {
            let e = ev(&format!(r#"require("proteus").config {{ {src} }}"#)).unwrap_err();
            assert!(e.contains(want), "{e}");
        }
    }

    #[test]
    fn shell_integration_config() {
        let c = ev(r#"require("proteus").config {
            ui = { gutter = 1 },
            notify = { min_duration = 3.5, osc = false },
            bell = { visual = false },
            hints = { alphabet = "jkl", patterns = { { name = "jira", regex = "[A-Z]+-\\d+", url = "https://j/{}" }, { regex = "x+" } } },
        }"#)
        .unwrap();
        assert_eq!(c.gutter, 1.0);
        assert_eq!(c.notify_after, Some(std::time::Duration::from_millis(3500)));
        assert!(!c.notify_osc && !c.visual_bell);
        assert_eq!(c.hint_alphabet, "jkl");
        assert_eq!(c.hint_patterns.len(), 2);
        assert_eq!(c.hint_patterns[0].url.as_deref(), Some("https://j/{}"));
        assert_eq!(c.hint_patterns[1].name, "pattern2");
        assert_eq!(
            ev(r#"require("proteus").config { notify = { min_duration = false } }"#).unwrap().notify_after,
            None
        );
        for (src, want) in [
            (r#"hints = { alphabet = "a" }"#, "hints.alphabet"),
            (r#"hints = { patterns = { { regex = "(" } } }"#, "hints.patterns[1].regex"),
            (r#"hints = { patterns = { { name = "x" } } }"#, "`regex` is required"),
            (r#"notify = { when = 1 }"#, "notify.when"),
        ] {
            let e = ev(&format!(r#"require("proteus").config {{ {src} }}"#)).unwrap_err();
            assert!(e.contains(want), "{e}");
        }
    }

    #[test]
    fn graphics_config() {
        let c = ev(r#"require("proteus").config {
            graphics = { enabled = false, max_image = 8, pane_memory = 32, total_memory = 100, gpu_memory = 64.5, local_files = false },
        }"#)
        .unwrap();
        let g = &c.graphics;
        assert!(!g.enabled && !g.local_files);
        assert_eq!((g.max_image, g.pane_memory, g.total_memory), (8 << 20, 32 << 20, 100 << 20));
        assert_eq!(g.gpu_memory, (64.5 * 1048576.0) as usize);
        let core = c.core();
        assert!(!core.graphics.enabled && !core.graphics.allow_local_files);
        assert_eq!((core.graphics.max_image_bytes, core.graphics.pane_budget), (8 << 20, 32 << 20));
        for (src, want) in [
            (r#"graphics = { max_image = 0 }"#, "graphics.max_image"),
            (r#"graphics = { sixel = true }"#, "graphics.sixel"),
        ] {
            let e = ev(&format!(r#"require("proteus").config {{ {src} }}"#)).unwrap_err();
            assert!(e.contains(want), "{e}");
        }
    }

    #[test]
    fn require_from_config_dir() {
        let dir = std::env::temp_dir().join(format!("proteus-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("theme.lua"), "return { size = 20 }").unwrap();
        let init = dir.join("init.lua");
        std::fs::write(&init, r#"require("proteus").config { font = require("theme") }"#).unwrap();
        let c = load(&init, true).unwrap().config;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(c.font_size, 20.0);
        assert_eq!(load(&dir.join("missing.lua"), false).unwrap().config, Config::default());
        assert!(load(&dir.join("missing.lua"), true).is_err());
    }
}
