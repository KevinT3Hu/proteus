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
//! ```
//!
//! `p.config` may be called several times; later calls override earlier
//! keys. `require` resolves modules inside the config directory. Unknown
//! keys and wrong types are errors, so typos do not go unnoticed. No config
//! file is required.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, LuaOptions, StdLib, Table, Value};
use proteus_core::ClipboardRead;

use crate::modes::{Action, Chord, KeyConfig, Mode, Preset};

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
    pub font_family: String,
    pub font_size: f32,
    pub ligatures: bool,
    pub cursor_blink: Blink,
    /// Mode 2027 (grapheme clustering) on without the application asking.
    pub grapheme_clustering: bool,
    /// Treat East Asian ambiguous-width characters as wide.
    pub ambiguous_wide: bool,
    pub osc52_write: bool,
    pub osc52_read: ClipboardRead,
    /// Confirm multi-line pastes and pastes with control characters when
    /// the application has not enabled bracketed paste.
    pub paste_protection: bool,
    /// Program and arguments; `None` runs `$SHELL`.
    pub command: Option<Vec<OsString>>,
    pub shell_integration: bool,
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
            font_family: "monospace".into(),
            font_size: 11.0,
            ligatures: true,
            cursor_blink: Blink::App,
            grapheme_clustering: false,
            ambiguous_wide: false,
            osc52_write: true,
            osc52_read: ClipboardRead::Ask,
            paste_protection: true,
            command: None,
            shell_integration: true,
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

/// Load `path`. A missing default file is not an error; a missing file
/// given explicitly is.
pub fn load(path: &Path, explicit: bool) -> Result<Config, String> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => return Ok(Config::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    eval(&src, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Evaluate config source; `path` names the chunk and sets the module dir.
pub fn eval(src: &str, path: &Path) -> Result<Config, String> {
    let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).map_err(|e| e.to_string())?;
    let cfg = Rc::new(RefCell::new(Config::default()));
    let setup = || -> mlua::Result<()> {
        let module = lua.create_table()?;
        let c = cfg.clone();
        module.set(
            "config",
            lua.create_function(move |_, t: Table| apply(&mut c.borrow_mut(), &t).map_err(mlua::Error::external))?,
        )?;
        let package: Table = lua.globals().get("package")?;
        let loaded: Table = package.get("loaded")?;
        loaded.set("proteus", module)?;
        if let Some(dir) = path.parent().and_then(Path::to_str) {
            package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?;
        }
        package.set("cpath", "")?;
        Ok(())
    };
    setup().map_err(|e| e.to_string())?;
    lua.load(src).set_name(format!("@{}", path.display())).exec().map_err(|e| clean(&e))?;
    drop(lua);
    Ok(Rc::try_unwrap(cfg).map(RefCell::into_inner).unwrap_or_else(|rc| rc.borrow().clone()))
}

/// First line of a Lua error, without mlua's traceback noise.
fn clean(e: &mlua::Error) -> String {
    let s = match e {
        mlua::Error::CallbackError { cause, .. } => return clean(cause),
        mlua::Error::ExternalError(e) => e.to_string(),
        e => e.to_string(),
    };
    s.lines().next().unwrap_or("").trim_start_matches("runtime error: ").to_string()
}

fn apply(c: &mut Config, t: &Table) -> Result<(), String> {
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| format!("config: {e}"))?;
        match k.as_str() {
            "font" => section(&v, "font", |k, v| {
                match k {
                    "family" => c.font_family = string(v, "font.family")?,
                    "size" => c.font_size = number(v, "font.size")?.clamp(4.0, 96.0) as f32,
                    "ligatures" => c.ligatures = boolean(v, "font.ligatures")?,
                    _ => return Err(unknown("font", k)),
                }
                Ok(())
            })?,
            "cursor" => section(&v, "cursor", |k, v| match k {
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
                            let chord = Chord::parse(key).map_err(|e| format!("{name}: {e}"))?;
                            let action = match action {
                                Value::Boolean(false) => None,
                                _ => match string(action, &format!("{name}[\"{key}\"]"))?.as_str() {
                                    "none" => None,
                                    a => Some(Action::from_name(a).ok_or_else(|| {
                                        format!(
                                            "{name}[\"{key}\"]: unknown action `{a}` (see `proteus --list-actions`)"
                                        )
                                    })?),
                                },
                            };
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
        eval(src, Path::new("/nonexistent/init.lua"))
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
        let c = load(&init, true).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(c.font_size, 20.0);
        assert_eq!(load(&dir.join("missing.lua"), false).unwrap(), Config::default());
        assert!(load(&dir.join("missing.lua"), true).is_err());
    }
}
