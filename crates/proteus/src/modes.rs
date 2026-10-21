//! Keyboard modes (doc 02, D3). Every binding maps a key chord to a named
//! [`Action`] in one [`Mode`]'s table. A preset supplies the default tables;
//! the config overrides single keys per mode. The hint bar is generated from
//! the same tables, so it always matches the bindings.

use proteus_core::input::{Key, Mods};

use crate::layout::{Axis, Dir};

/// Modifiers that take part in bindings (locks are ignored).
const MASK: Mods = Mods::SHIFT.union(Mods::ALT).union(Mods::CTRL).union(Mods::SUPER);

/// A key with modifiers, e.g. `ctrl+g`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: Key,
    pub mods: Mods,
}

fn mods_prefix(m: Mods) -> String {
    let mut s = String::new();
    for (bit, name) in [(Mods::SUPER, "super+"), (Mods::CTRL, "ctrl+"), (Mods::ALT, "alt+"), (Mods::SHIFT, "shift+")] {
        if m.contains(bit) {
            s.push_str(name);
        }
    }
    s
}

impl Chord {
    pub const fn new(key: Key, mods: Mods) -> Self {
        Self { key, mods }
    }

    /// Parse `ctrl+g`, `alt+shift+x`, `ctrl+space`, `f12`, `ctrl++`, `H` (= shift+h).
    pub fn parse(s: &str) -> Result<Chord, String> {
        let s = s.trim();
        let bad = || format!("invalid key `{s}`");
        let (mods_part, key_part) = if let Some(m) = s.strip_suffix("++") {
            (m, "+")
        } else {
            s.rsplit_once('+').unwrap_or(("", s))
        };
        let mut mods = Mods::empty();
        for m in mods_part.split('+').filter(|m| !m.is_empty()) {
            mods |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Mods::CTRL,
                "alt" | "meta" | "opt" => Mods::ALT,
                "shift" => Mods::SHIFT,
                "super" | "logo" | "win" | "cmd" => Mods::SUPER,
                _ => return Err(format!("unknown modifier `{m}` in `{s}`")),
            };
        }
        let lower = key_part.to_ascii_lowercase();
        let key = match lower.as_str() {
            "space" => Key::Char(' '),
            "enter" | "return" => Key::Enter,
            "tab" => Key::Tab,
            "esc" | "escape" => Key::Escape,
            "backspace" => Key::Backspace,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" | "pgup" => Key::PageUp,
            "pagedown" | "pgdn" => Key::PageDown,
            "insert" => Key::Insert,
            "delete" | "del" => Key::Delete,
            f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=35).contains(&n)) => {
                Key::F(f[1..].parse().unwrap_or(1))
            }
            _ => {
                let mut it = key_part.chars();
                let (Some(c), None) = (it.next(), it.next()) else { return Err(bad()) };
                if c.is_ascii_uppercase() {
                    mods |= Mods::SHIFT;
                }
                Key::Char(c.to_ascii_lowercase())
            }
        };
        Ok(Chord { key, mods })
    }

    pub fn matches(&self, key: Key, mods: Mods) -> bool {
        self.key == key && self.mods == mods & MASK
    }

    /// Short display form for the hint bar: `ctrl+g`, `H`, `←`.
    pub fn label(&self) -> String {
        let key = match self.key {
            Key::Char(' ') => "space".to_string(),
            Key::Char(c) if self.mods == Mods::SHIFT && c.is_ascii_lowercase() => {
                return c.to_ascii_uppercase().to_string();
            }
            Key::Char(c) => c.to_string(),
            Key::Enter => "enter".into(),
            Key::Tab => "tab".into(),
            Key::Backspace => "bksp".into(),
            Key::Escape => "esc".into(),
            Key::Up => "↑".into(),
            Key::Down => "↓".into(),
            Key::Left => "←".into(),
            Key::Right => "→".into(),
            Key::Home => "home".into(),
            Key::End => "end".into(),
            Key::PageUp => "pgup".into(),
            Key::PageDown => "pgdn".into(),
            Key::Insert => "ins".into(),
            Key::Delete => "del".into(),
            Key::F(n) => format!("f{n}"),
        };
        mods_prefix(self.mods) + &key
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keys go to the application unless bound in this mode.
    Normal,
    /// Like Normal, with (by default) only the unlock key bound.
    Locked,
    /// Mode picker opened by the leader key (leader preset).
    Menu,
    Pane,
    Tab,
    Resize,
    Move,
    /// Scrollback with a keyboard cursor: vi motions, selection, copy, search.
    Scroll,
    /// Layouts: save the current one, load one into new tabs.
    Session,
}

pub const MODES: [Mode; 9] = [
    Mode::Normal,
    Mode::Locked,
    Mode::Menu,
    Mode::Pane,
    Mode::Tab,
    Mode::Resize,
    Mode::Move,
    Mode::Scroll,
    Mode::Session,
];

impl Mode {
    /// Config / action name: `normal`, `pane`, ...
    pub fn key(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Locked => "locked",
            Mode::Menu => "menu",
            Mode::Pane => "pane",
            Mode::Tab => "tab",
            Mode::Resize => "resize",
            Mode::Move => "move",
            Mode::Scroll => "scroll",
            Mode::Session => "session",
        }
    }

    pub fn from_key(s: &str) -> Option<Mode> {
        MODES.into_iter().find(|m| m.key() == s)
    }

    /// Badge in the hint bar.
    pub fn name(self) -> String {
        self.key().to_ascii_uppercase()
    }

    /// Modes whose actions are naturally repeated stay active after an
    /// action even when `keys.sticky` is off.
    pub fn repeatable(self) -> bool {
        matches!(self, Mode::Resize | Mode::Move | Mode::Scroll)
    }

    /// Modes where unbound keys reach the application.
    pub fn passes_keys(self) -> bool {
        matches!(self, Mode::Normal | Mode::Locked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Lines(i32),
    HalfPages(i32),
    Pages(i32),
    Top,
    Bottom,
    /// Previous (`false`) or next prompt.
    Prompt(bool),
}

/// Copy-cursor movement in Scroll mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordNext,
    WordPrev,
    WordEnd,
    LineStart,
    LineEnd,
    /// Oldest line in the scrollback.
    Top,
    /// Last line of the live screen.
    Bottom,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    PrevPrompt,
    NextPrompt,
}

const MOTIONS: [(Motion, &str); 20] = [
    (Motion::Left, "left"),
    (Motion::Right, "right"),
    (Motion::Up, "up"),
    (Motion::Down, "down"),
    (Motion::WordNext, "word_next"),
    (Motion::WordPrev, "word_prev"),
    (Motion::WordEnd, "word_end"),
    (Motion::LineStart, "line_start"),
    (Motion::LineEnd, "line_end"),
    (Motion::Top, "top"),
    (Motion::Bottom, "bottom"),
    (Motion::HalfPageUp, "half_page_up"),
    (Motion::HalfPageDown, "half_page_down"),
    (Motion::PageUp, "page_up"),
    (Motion::PageDown, "page_down"),
    (Motion::ScreenTop, "screen_top"),
    (Motion::ScreenMiddle, "screen_middle"),
    (Motion::ScreenBottom, "screen_bottom"),
    (Motion::PrevPrompt, "prev_prompt"),
    (Motion::NextPrompt, "next_prompt"),
];

/// Kind of keyboard selection started at the copy cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    Char,
    Line,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Switch mode (`Normal` leaves the current one).
    Mode(Mode),
    /// Send the pressed key to the application.
    Passthrough,
    Focus(Dir),
    /// Focus the neighbor; at the window edge go to the previous/next tab.
    FocusOrTab(Dir),
    Split(Axis),
    /// Split along the longer side of the focused pane.
    NewPane,
    ClosePane,
    Zoom,
    PaneToNewTab,
    Resize(Dir, i32),
    Equalize,
    Swap(Dir),
    NewTab,
    /// Zero-based tab index.
    GotoTab(u8),
    PrevTab,
    NextTab,
    CloseTab,
    Scroll(Scroll),
    Copy,
    Paste,
    PastePrimary,
    /// Font size step (+1/-1); 0 resets.
    Font(i8),
    /// Close the window (asks if programs are running).
    Quit,
    CopyMove(Motion),
    /// Start (or switch to / stop) a selection at the copy cursor.
    CopySelect(Select),
    /// Copy the selection (or the cursor line) and leave Scroll mode.
    CopyYank,
    /// Clear the selection, else the search, else leave Scroll mode.
    CopyEscape,
    /// Type a query (enters Scroll mode).
    SearchStart,
    /// Next match towards older lines.
    SearchUp,
    SearchDown,
    RenameTab,
    RenamePane,
    SaveLayout,
    LoadLayout,
    /// Next pane in order (tmux `o`).
    FocusNext,
    /// Show/hide the floating panes (a first one is created if none).
    ToggleFloating,
    /// Float the focused pane, or embed the focused float.
    ToggleEmbed,
    NewFloating,
    /// Stack the focused pane with its sibling, or unstack.
    ToggleStack,
    TogglePaneTitles,
    NewWindow,
    TabToNewWindow,
    PaneToNewWindow,
    /// Move the tab into the next window.
    TabToNextWindow,
    /// Run any action by name.
    Palette,
}

/// Cells per resize step.
pub const RESIZE_STEP: i32 = 2;

const DIRS: [Dir; 4] = [Dir::Left, Dir::Down, Dir::Up, Dir::Right];

fn dir_name(d: Dir) -> &'static str {
    match d {
        Dir::Left => "left",
        Dir::Right => "right",
        Dir::Up => "up",
        Dir::Down => "down",
    }
}

impl Action {
    /// Stable name, used in the config, `--list-actions` and `PROTEUS_ACTIONS`.
    pub fn name(&self) -> String {
        match *self {
            Action::Mode(m) => format!("mode.{}", m.key()),
            Action::Passthrough => "send_key".into(),
            Action::Focus(d) => format!("pane.focus_{}", dir_name(d)),
            Action::FocusOrTab(d) => format!("pane.focus_or_tab_{}", dir_name(d)),
            Action::Split(Axis::X) => "pane.split_right".into(),
            Action::Split(Axis::Y) => "pane.split_down".into(),
            Action::NewPane => "pane.new".into(),
            Action::ClosePane => "pane.close".into(),
            Action::Zoom => "pane.zoom".into(),
            Action::PaneToNewTab => "pane.to_new_tab".into(),
            Action::Resize(d, n) if n >= 0 => format!("pane.grow_{}", dir_name(d)),
            Action::Resize(d, _) => format!("pane.shrink_{}", dir_name(d)),
            Action::Equalize => "pane.equalize".into(),
            Action::Swap(d) => format!("pane.move_{}", dir_name(d)),
            Action::NewTab => "tab.new".into(),
            Action::GotoTab(i) => format!("tab.goto_{}", i as u32 + 1),
            Action::PrevTab => "tab.prev".into(),
            Action::NextTab => "tab.next".into(),
            Action::CloseTab => "tab.close".into(),
            Action::Scroll(s) => match s {
                Scroll::Lines(n) if n < 0 => "scroll.line_up".into(),
                Scroll::Lines(_) => "scroll.line_down".into(),
                Scroll::HalfPages(n) if n < 0 => "scroll.half_page_up".into(),
                Scroll::HalfPages(_) => "scroll.half_page_down".into(),
                Scroll::Pages(n) if n < 0 => "scroll.page_up".into(),
                Scroll::Pages(_) => "scroll.page_down".into(),
                Scroll::Top => "scroll.top".into(),
                Scroll::Bottom => "scroll.bottom".into(),
                Scroll::Prompt(false) => "scroll.prev_prompt".into(),
                Scroll::Prompt(true) => "scroll.next_prompt".into(),
            },
            Action::Copy => "copy".into(),
            Action::Paste => "paste".into(),
            Action::PastePrimary => "paste_primary".into(),
            Action::Font(n) if n > 0 => "font.increase".into(),
            Action::Font(n) if n < 0 => "font.decrease".into(),
            Action::Font(_) => "font.reset".into(),
            Action::Quit => "window.close".into(),
            Action::CopyMove(m) => {
                format!("copy.{}", MOTIONS.iter().find(|x| x.0 == m).map_or("?", |x| x.1))
            }
            Action::CopySelect(Select::Char) => "copy.select".into(),
            Action::CopySelect(Select::Line) => "copy.select_line".into(),
            Action::CopySelect(Select::Block) => "copy.select_block".into(),
            Action::CopyYank => "copy.yank".into(),
            Action::CopyEscape => "copy.escape".into(),
            Action::SearchStart => "search.start".into(),
            Action::SearchUp => "search.up".into(),
            Action::SearchDown => "search.down".into(),
            Action::RenameTab => "tab.rename".into(),
            Action::RenamePane => "pane.rename".into(),
            Action::SaveLayout => "layout.save".into(),
            Action::LoadLayout => "layout.load".into(),
            Action::FocusNext => "pane.focus_next".into(),
            Action::ToggleFloating => "pane.toggle_floating".into(),
            Action::ToggleEmbed => "pane.toggle_embed".into(),
            Action::NewFloating => "pane.new_floating".into(),
            Action::ToggleStack => "pane.toggle_stack".into(),
            Action::TogglePaneTitles => "ui.toggle_pane_titles".into(),
            Action::NewWindow => "window.new".into(),
            Action::TabToNewWindow => "tab.to_new_window".into(),
            Action::PaneToNewWindow => "pane.to_new_window".into(),
            Action::TabToNextWindow => "tab.to_next_window".into(),
            Action::Palette => "palette".into(),
        }
    }

    /// Every action (tabs 1–9; `tab.goto_N` also parses for larger N).
    pub fn all() -> Vec<Action> {
        let mut v: Vec<Action> = MODES.into_iter().map(Action::Mode).collect();
        v.push(Action::Passthrough);
        v.extend(DIRS.map(Action::Focus));
        v.extend([Action::FocusOrTab(Dir::Left), Action::FocusOrTab(Dir::Right)]);
        v.extend([Action::Split(Axis::X), Action::Split(Axis::Y), Action::NewPane, Action::ClosePane, Action::Zoom]);
        v.push(Action::PaneToNewTab);
        v.extend(DIRS.map(|d| Action::Resize(d, RESIZE_STEP)));
        v.extend(DIRS.map(|d| Action::Resize(d, -RESIZE_STEP)));
        v.push(Action::Equalize);
        v.extend(DIRS.map(Action::Swap));
        v.push(Action::NewTab);
        v.extend((0..9).map(Action::GotoTab));
        v.extend([Action::PrevTab, Action::NextTab, Action::CloseTab]);
        v.extend(
            [
                Scroll::Lines(-1),
                Scroll::Lines(1),
                Scroll::HalfPages(-1),
                Scroll::HalfPages(1),
                Scroll::Pages(-1),
                Scroll::Pages(1),
                Scroll::Top,
                Scroll::Bottom,
                Scroll::Prompt(false),
                Scroll::Prompt(true),
            ]
            .map(Action::Scroll),
        );
        v.extend([Action::Copy, Action::Paste, Action::PastePrimary]);
        v.extend([Action::Font(1), Action::Font(-1), Action::Font(0), Action::Quit]);
        v.extend(MOTIONS.map(|m| Action::CopyMove(m.0)));
        v.extend([Select::Char, Select::Line, Select::Block].map(Action::CopySelect));
        v.extend([Action::CopyYank, Action::CopyEscape, Action::SearchStart, Action::SearchUp, Action::SearchDown]);
        v.extend([Action::RenameTab, Action::RenamePane, Action::SaveLayout, Action::LoadLayout]);
        v.extend([
            Action::FocusNext,
            Action::ToggleFloating,
            Action::ToggleEmbed,
            Action::NewFloating,
            Action::ToggleStack,
            Action::TogglePaneTitles,
            Action::NewWindow,
            Action::TabToNewWindow,
            Action::PaneToNewWindow,
            Action::TabToNextWindow,
            Action::Palette,
        ]);
        v
    }

    pub fn from_name(s: &str) -> Option<Action> {
        if let Some(n) = s.strip_prefix("tab.goto_") {
            return n.parse::<u8>().ok().filter(|n| (1..=99).contains(n)).map(|n| Action::GotoTab(n - 1));
        }
        Action::all().into_iter().find(|a| a.name() == s)
    }

    /// Hint bar label when bound in `mode`; `None` keeps it out of the bar.
    fn hint(&self, mode: Mode) -> Option<&'static str> {
        let in_menu = matches!(mode, Mode::Normal | Mode::Locked | Mode::Menu);
        Some(match *self {
            Action::Mode(Mode::Normal) if mode == Mode::Locked => "unlock",
            Action::Mode(Mode::Normal) if mode == Mode::Normal => return None,
            Action::Mode(Mode::Normal) => "back",
            // Inside a mode, switching to another one is not advertised.
            Action::Mode(_) if !in_menu => return None,
            Action::Mode(Mode::Locked) => "lock",
            Action::Mode(m) => m.key(),
            Action::Passthrough => "send key",
            Action::Focus(_) | Action::FocusOrTab(_) => "focus",
            Action::Split(Axis::X) => "split right",
            Action::Split(Axis::Y) => "split down",
            Action::NewPane => "new pane",
            Action::ClosePane => "close",
            Action::Zoom => "zoom",
            Action::PaneToNewTab => "to new tab",
            Action::Resize(_, n) if n >= 0 => "grow",
            Action::Resize(..) => "shrink",
            Action::Equalize => "equalize",
            Action::Swap(_) => "move",
            Action::NewTab => "new tab",
            Action::GotoTab(_) => "go to tab",
            Action::PrevTab | Action::NextTab => "prev/next tab",
            Action::CloseTab => "close tab",
            // Terminal keys (copy, font, Shift+PgUp) are not advertised.
            Action::Scroll(_) if mode.passes_keys() => return None,
            Action::Scroll(Scroll::Lines(_)) => return None,
            Action::Scroll(Scroll::HalfPages(_)) => "half page",
            Action::Scroll(Scroll::Pages(_)) => "page",
            Action::Scroll(Scroll::Top | Scroll::Bottom) => "top/bottom",
            Action::Scroll(Scroll::Prompt(_)) => "prompt",
            Action::Copy | Action::Paste | Action::PastePrimary | Action::Font(_) => return None,
            Action::Quit => "quit",
            Action::CopyMove(Motion::Left | Motion::Right | Motion::Up | Motion::Down) => "move",
            Action::CopyMove(Motion::WordNext | Motion::WordPrev | Motion::WordEnd) => "word",
            Action::CopyMove(Motion::Top | Motion::Bottom) => "top/bottom",
            Action::CopyMove(Motion::HalfPageUp | Motion::HalfPageDown) => "half page",
            Action::CopyMove(Motion::PrevPrompt | Motion::NextPrompt) => "prompt",
            Action::CopyMove(_) => return None,
            Action::CopySelect(_) => "select",
            Action::CopyYank => "copy",
            Action::CopyEscape => "back",
            Action::SearchStart => "search",
            Action::SearchUp | Action::SearchDown => "match",
            Action::RenameTab | Action::RenamePane => "rename",
            Action::SaveLayout => "save layout",
            Action::LoadLayout => "load layout",
            Action::FocusNext => "next pane",
            Action::ToggleFloating => "floating",
            Action::ToggleEmbed => "float/embed",
            Action::NewFloating => "new floating",
            Action::ToggleStack => "stack",
            Action::TogglePaneTitles => "titles",
            Action::NewWindow => "new window",
            Action::TabToNewWindow | Action::PaneToNewWindow => "to new window",
            Action::TabToNextWindow => "to next window",
            Action::Palette if mode.passes_keys() => return None,
            Action::Palette => "palette",
        })
    }
}

/// Which default bindings to start from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// zellij-style: Ctrl+p/t/n/h/s enter modes directly, the leader
    /// (Ctrl+g) locks; Alt+… quick keys work in Normal mode.
    Zellij,
    /// One leader key opens a mode menu (no chords taken from the shell
    /// besides the leader); the same Alt+… quick keys.
    Leader,
    /// tmux: the prefix (Ctrl+b) then one tmux key (`c`, `%`, `"`, `n`,
    /// `p`, `o`, `x`, `z`, `[`, ...). No Alt quick keys.
    Tmux,
}

impl Preset {
    pub fn default_leader(self) -> Chord {
        match self {
            Preset::Tmux => Chord::new(Key::Char('b'), Mods::CTRL),
            Preset::Zellij | Preset::Leader => Chord::new(Key::Char('g'), Mods::CTRL),
        }
    }
}

/// Key settings from the config: a preset plus per-mode overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyConfig {
    pub preset: Preset,
    /// Zellij preset: lock/unlock key. Leader preset: opens the mode
    /// menu. Tmux preset: the prefix. `None`: the preset's default.
    pub leader: Option<Chord>,
    /// Stay in a mode after an action instead of returning to Normal.
    pub sticky: bool,
    /// Applied in order on top of the preset; `None` unbinds the chord.
    pub binds: Vec<(Mode, Chord, Option<Action>)>,
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self { preset: Preset::Zellij, leader: None, sticky: false, binds: Vec::new() }
    }
}

/// What a key press means in the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// Send it to the application.
    Pass,
    /// Unbound inside a mode: drop it.
    Swallow,
    Do(Action),
}

type Table = Vec<(Chord, Action)>;

/// Add bindings; a chord already in the table keeps its first action.
fn bind(t: &mut Table, keys: &[&str], a: Action) {
    for k in keys {
        let c = Chord::parse(k).expect("default binding parses");
        if !t.iter().any(|x| x.0 == c) {
            t.push((c, a));
        }
    }
}

fn dirs(t: &mut Table, mods: &str, f: impl Fn(Dir) -> Action) {
    for (d, k, arrow) in [(Dir::Left, "h", "left"), (Dir::Down, "j", "down"), (Dir::Up, "k", "up"), (Dir::Right, "l", "right")] {
        bind(t, &[&format!("{mods}{k}"), &format!("{mods}{arrow}")], f(d));
    }
}

fn bind_chord(t: &mut Table, c: Chord, a: Action) {
    if !t.iter().any(|x| x.0 == c) {
        t.push((c, a));
    }
}

/// Basic terminal keys, bound in Normal and Locked.
fn terminal_keys(t: &mut Table) {
    bind(t, &["ctrl+shift+c"], Action::Copy);
    bind(t, &["ctrl+shift+v"], Action::Paste);
    bind(t, &["shift+insert"], Action::PastePrimary);
    bind(t, &["ctrl+=", "ctrl+shift+=", "ctrl++"], Action::Font(1));
    bind(t, &["ctrl+-", "ctrl+shift+-"], Action::Font(-1));
    bind(t, &["ctrl+0", "ctrl+shift+0"], Action::Font(0));
    bind(t, &["ctrl+shift+z"], Action::Scroll(Scroll::Prompt(false)));
    bind(t, &["ctrl+shift+x"], Action::Scroll(Scroll::Prompt(true)));
    bind(t, &["shift+pageup"], Action::Scroll(Scroll::Pages(-1)));
    bind(t, &["shift+pagedown"], Action::Scroll(Scroll::Pages(1)));
    bind(t, &["shift+home"], Action::Scroll(Scroll::Top));
    bind(t, &["shift+end"], Action::Scroll(Scroll::Bottom));
}

/// One-step multiplexer keys in Normal mode.
fn quick_keys(t: &mut Table) {
    dirs(t, "alt+", |d| match d {
        Dir::Left | Dir::Right => Action::FocusOrTab(d),
        _ => Action::Focus(d),
    });
    bind(t, &["alt+n"], Action::NewPane);
    bind(t, &["alt+t"], Action::NewTab);
    bind(t, &["alt+["], Action::PrevTab);
    bind(t, &["alt+]"], Action::NextTab);
    for i in 1..=9u8 {
        bind(t, &[&format!("alt+{i}")], Action::GotoTab(i - 1));
    }
    bind(t, &["alt+f"], Action::ToggleFloating);
    bind(t, &["ctrl+shift+f"], Action::SearchStart);
}

/// Window-level keys, in Normal mode of every preset.
fn window_keys(t: &mut Table) {
    bind(t, &["ctrl+shift+p"], Action::Palette);
    bind(t, &["ctrl+shift+n"], Action::NewWindow);
}

/// tmux's prefix table (in the Menu mode; one key, then back to Normal).
fn tmux_table(t: &mut Table) {
    bind(t, &["c"], Action::NewTab);
    bind(t, &["n"], Action::NextTab);
    bind(t, &["p"], Action::PrevTab);
    for i in 1..=9u8 {
        bind(t, &[&i.to_string()], Action::GotoTab(i - 1));
    }
    bind(t, &["%"], Action::Split(Axis::X));
    bind(t, &["\""], Action::Split(Axis::Y));
    dirs(t, "", Action::Focus);
    bind(t, &["o"], Action::FocusNext);
    bind(t, &["x"], Action::ClosePane);
    bind(t, &["&"], Action::CloseTab);
    bind(t, &["z"], Action::Zoom);
    bind(t, &["!"], Action::PaneToNewTab);
    bind(t, &["{"], Action::Swap(Dir::Left));
    bind(t, &["}"], Action::Swap(Dir::Right));
    bind(t, &[","], Action::RenameTab);
    bind(t, &["."], Action::RenamePane);
    bind(t, &["["], Action::Mode(Mode::Scroll));
    bind(t, &["/"], Action::SearchStart);
    bind(t, &[":"], Action::Palette);
    bind(t, &["w"], Action::ToggleFloating);
    bind(t, &["s"], Action::Mode(Mode::Session));
    bind(t, &["r"], Action::Mode(Mode::Resize));
    bind(t, &["m"], Action::Mode(Mode::Move));
    for (k, d) in [("ctrl+left", Dir::Left), ("ctrl+right", Dir::Right), ("ctrl+up", Dir::Up), ("ctrl+down", Dir::Down)] {
        bind(t, &[k], Action::Resize(d, RESIZE_STEP));
    }
    bind(t, &["esc"], Action::Mode(Mode::Normal));
}

/// Bindings of the modes entered from Normal (shared by both presets).
fn mode_table(mode: Mode) -> Table {
    let mut t = Table::new();
    let s = Action::Scroll;
    match mode {
        Mode::Pane => {
            dirs(&mut t, "", Action::Focus);
            bind(&mut t, &["n"], Action::NewPane);
            bind(&mut t, &["d", "s"], Action::Split(Axis::Y));
            bind(&mut t, &["r", "v"], Action::Split(Axis::X));
            bind(&mut t, &["x"], Action::ClosePane);
            bind(&mut t, &["f", "z"], Action::Zoom);
            bind(&mut t, &["w"], Action::ToggleFloating);
            bind(&mut t, &["e"], Action::ToggleEmbed);
            bind(&mut t, &["S"], Action::ToggleStack);
            bind(&mut t, &["T"], Action::TogglePaneTitles);
            bind(&mut t, &["W"], Action::PaneToNewWindow);
            bind(&mut t, &["c"], Action::RenamePane);
        }
        Mode::Tab => {
            bind(&mut t, &["n"], Action::NewTab);
            for i in 1..=9u8 {
                bind(&mut t, &[&i.to_string()], Action::GotoTab(i - 1));
            }
            bind(&mut t, &["h", "left", "k", "up"], Action::PrevTab);
            bind(&mut t, &["l", "right", "j", "down"], Action::NextTab);
            bind(&mut t, &["b"], Action::PaneToNewTab);
            bind(&mut t, &["r"], Action::RenameTab);
            bind(&mut t, &["x"], Action::CloseTab);
            bind(&mut t, &["w"], Action::TabToNextWindow);
            bind(&mut t, &["W"], Action::TabToNewWindow);
        }
        Mode::Resize => {
            dirs(&mut t, "", |d| Action::Resize(d, RESIZE_STEP));
            dirs(&mut t, "shift+", |d| Action::Resize(d, -RESIZE_STEP));
            bind(&mut t, &["="], Action::Equalize);
        }
        Mode::Move => {
            dirs(&mut t, "", Action::Swap);
            bind(&mut t, &["t", "b"], Action::PaneToNewTab);
        }
        Mode::Scroll => {
            let c = Action::CopyMove;
            bind(&mut t, &["h", "left"], c(Motion::Left));
            bind(&mut t, &["j", "down"], c(Motion::Down));
            bind(&mut t, &["k", "up"], c(Motion::Up));
            bind(&mut t, &["l", "right"], c(Motion::Right));
            bind(&mut t, &["v"], Action::CopySelect(Select::Char));
            bind(&mut t, &["V"], Action::CopySelect(Select::Line));
            bind(&mut t, &["ctrl+v"], Action::CopySelect(Select::Block));
            bind(&mut t, &["y", "enter"], Action::CopyYank);
            bind(&mut t, &["/", "?", "s"], Action::SearchStart);
            bind(&mut t, &["n"], Action::SearchUp);
            bind(&mut t, &["N"], Action::SearchDown);
            bind(&mut t, &["w"], c(Motion::WordNext));
            bind(&mut t, &["b"], c(Motion::WordPrev));
            bind(&mut t, &["e"], c(Motion::WordEnd));
            bind(&mut t, &["0", "^", "home"], c(Motion::LineStart));
            bind(&mut t, &["$", "end"], c(Motion::LineEnd));
            bind(&mut t, &["g"], c(Motion::Top));
            bind(&mut t, &["G"], c(Motion::Bottom));
            bind(&mut t, &["d", "ctrl+d"], c(Motion::HalfPageDown));
            bind(&mut t, &["u", "ctrl+u"], c(Motion::HalfPageUp));
            bind(&mut t, &["ctrl+f", "pagedown"], c(Motion::PageDown));
            bind(&mut t, &["ctrl+b", "pageup"], c(Motion::PageUp));
            bind(&mut t, &["H"], c(Motion::ScreenTop));
            bind(&mut t, &["M"], c(Motion::ScreenMiddle));
            bind(&mut t, &["L"], c(Motion::ScreenBottom));
            bind(&mut t, &["["], c(Motion::PrevPrompt));
            bind(&mut t, &["]"], c(Motion::NextPrompt));
            bind(&mut t, &["ctrl+e"], s(Scroll::Lines(1)));
            bind(&mut t, &["ctrl+y"], s(Scroll::Lines(-1)));
            bind(&mut t, &["esc"], Action::CopyEscape);
            bind(&mut t, &["q"], Action::Mode(Mode::Normal));
        }
        Mode::Session => {
            bind(&mut t, &["s"], Action::SaveLayout);
            bind(&mut t, &["l"], Action::LoadLayout);
        }
        Mode::Normal | Mode::Locked | Mode::Menu => {}
    }
    t
}

/// zellij's mode entry chords.
const ZELLIJ_ENTRY: [(&str, Mode); 6] = [
    ("ctrl+p", Mode::Pane),
    ("ctrl+t", Mode::Tab),
    ("ctrl+n", Mode::Resize),
    ("ctrl+h", Mode::Move),
    ("ctrl+s", Mode::Scroll),
    ("ctrl+o", Mode::Session),
];

#[derive(Debug, Clone)]
pub struct Keymap {
    pub sticky: bool,
    tables: Vec<(Mode, Table)>,
}

impl Keymap {
    pub fn new(cfg: &KeyConfig) -> Self {
        let leader = cfg.leader.unwrap_or(cfg.preset.default_leader());
        let mut tables: Vec<(Mode, Table)> = MODES.into_iter().map(|m| (m, mode_table(m))).collect();
        for (mode, t) in &mut tables {
            match (*mode, cfg.preset) {
                (Mode::Normal, Preset::Zellij) => {
                    bind_chord(t, leader, Action::Mode(Mode::Locked));
                    for (k, m) in ZELLIJ_ENTRY {
                        bind(t, &[k], Action::Mode(m));
                    }
                    quick_keys(t);
                    window_keys(t);
                    terminal_keys(t);
                }
                (Mode::Normal, Preset::Leader) => {
                    bind_chord(t, leader, Action::Mode(Mode::Menu));
                    quick_keys(t);
                    window_keys(t);
                    terminal_keys(t);
                }
                (Mode::Normal, Preset::Tmux) => {
                    bind_chord(t, leader, Action::Mode(Mode::Menu));
                    window_keys(t);
                    terminal_keys(t);
                }
                (Mode::Menu, Preset::Tmux) => {
                    // Prefix twice sends it (tmux `send-prefix`).
                    bind_chord(t, leader, Action::Passthrough);
                    tmux_table(t);
                }
                (Mode::Locked, _) => {
                    bind_chord(t, leader, Action::Mode(Mode::Normal));
                    terminal_keys(t);
                }
                (Mode::Menu, _) => {
                    bind(t, &["p"], Action::Mode(Mode::Pane));
                    bind(t, &["t"], Action::Mode(Mode::Tab));
                    bind(t, &["r"], Action::Mode(Mode::Resize));
                    bind(t, &["m"], Action::Mode(Mode::Move));
                    bind(t, &["s"], Action::Mode(Mode::Scroll));
                    bind(t, &["o"], Action::Mode(Mode::Session));
                    bind(t, &["l"], Action::Mode(Mode::Locked));
                    bind_chord(t, leader, Action::Passthrough);
                    bind(t, &["esc"], Action::Mode(Mode::Normal));
                }
                (m, Preset::Zellij) => {
                    bind(t, &["esc"], Action::Mode(Mode::Normal));
                    // The entry chord toggles back; the others switch modes.
                    for (k, other) in ZELLIJ_ENTRY {
                        bind(t, &[k], Action::Mode(if other == m { Mode::Normal } else { other }));
                    }
                    bind_chord(t, leader, Action::Mode(Mode::Locked));
                }
                (_, Preset::Leader | Preset::Tmux) => {
                    bind(t, &["esc"], Action::Mode(Mode::Normal));
                    bind_chord(t, leader, Action::Mode(Mode::Normal));
                }
            }
        }
        let mut km = Self { sticky: cfg.sticky, tables };
        for &(mode, chord, action) in &cfg.binds {
            km.set(mode, chord, action);
        }
        km
    }

    /// Bind (or with `None` unbind) a chord, keeping its place in the table.
    pub fn set(&mut self, mode: Mode, chord: Chord, action: Option<Action>) {
        let Some((_, t)) = self.tables.iter_mut().find(|t| t.0 == mode) else { return };
        let pos = t.iter().position(|(c, _)| *c == chord);
        match (pos, action) {
            (Some(i), Some(a)) => t[i].1 = a,
            (Some(i), None) => {
                t.remove(i);
            }
            (None, Some(a)) => t.push((chord, a)),
            (None, None) => {}
        }
    }

    pub fn bindings(&self, mode: Mode) -> &[(Chord, Action)] {
        self.tables.iter().find(|t| t.0 == mode).map(|t| t.1.as_slice()).unwrap_or(&[])
    }

    #[cfg(test)]
    pub fn lookup(&self, mode: Mode, key: Key, mods: Mods) -> Lookup {
        self.lookup_shifted(mode, key, mods, None)
    }

    /// Like [`lookup`](Self::lookup); with Shift held, `shifted` (the
    /// character the key types) also matches, so `$` or `?` can be bound.
    pub fn lookup_shifted(&self, mode: Mode, key: Key, mods: Mods, shifted: Option<char>) -> Lookup {
        let t = self.bindings(mode);
        let found = t.iter().find(|(c, _)| c.matches(key, mods)).or_else(|| {
            let ch = shifted.filter(|_| mods.contains(Mods::SHIFT))?;
            t.iter().find(|(c, _)| c.matches(Key::Char(ch), mods - Mods::SHIFT))
        });
        match found {
            Some(&(_, a)) => Lookup::Do(a),
            None if mode.passes_keys() => Lookup::Pass,
            None => Lookup::Swallow,
        }
    }

    /// Mode to be in after `action` ran in `mode`.
    pub fn after(&self, mode: Mode, action: Action) -> Mode {
        match action {
            Action::Mode(m) => m,
            Action::CopyYank => Mode::Normal,
            _ if mode.passes_keys() => mode,
            _ if mode == Mode::Menu => Mode::Normal,
            _ if self.sticky || mode.repeatable() => mode,
            _ => Mode::Normal,
        }
    }

    /// Hint bar entries for `mode`: (keys, label). Bindings with the same
    /// label are merged; each action shows its first key only.
    pub fn hints(&self, mode: Mode) -> Vec<(String, &'static str)> {
        let mut groups: Vec<(&'static str, Vec<(Chord, Action)>)> = Vec::new();
        for &(c, a) in self.bindings(mode) {
            let Some(label) = a.hint(mode) else { continue };
            let i = match groups.iter().position(|g| g.0 == label) {
                Some(i) => i,
                None => {
                    groups.push((label, Vec::new()));
                    groups.len() - 1
                }
            };
            if !groups[i].1.iter().any(|x| x.1 == a) {
                groups[i].1.push((c, a));
            }
        }
        groups.into_iter().map(|(l, k)| (keys_text(&k.iter().map(|x| x.0).collect::<Vec<_>>()), l)).collect()
    }
}

/// `hjkl`, `alt+hjkl`, `HJKL`, `alt+1-9`, else `a/b`.
fn keys_text(keys: &[Chord]) -> String {
    let Some(first) = keys.first() else { return String::new() };
    let mods = first.mods;
    let chars: Option<Vec<char>> = keys
        .iter()
        .map(|c| match c.key {
            Key::Char(ch) if c.mods == mods && ch != ' ' => Some(ch),
            _ => None,
        })
        .collect();
    if let Some(chars) = chars.filter(|c| c.len() > 1) {
        let digits = chars.len() >= 3
            && chars.iter().all(char::is_ascii_digit)
            && chars.windows(2).all(|w| w[1] as u32 == w[0] as u32 + 1);
        if digits {
            return format!("{}{}-{}", mods_prefix(mods), chars[0], chars[chars.len() - 1]);
        }
        if mods == Mods::SHIFT {
            return chars.iter().map(|c| c.to_ascii_uppercase()).collect();
        }
        return mods_prefix(mods) + &chars.iter().collect::<String>();
    }
    keys.iter().map(Chord::label).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn km(preset: Preset) -> Keymap {
        Keymap::new(&KeyConfig { preset, ..KeyConfig::default() })
    }

    const NONE: Mods = Mods::empty();

    #[test]
    fn parse_chords() {
        assert_eq!(Chord::parse("ctrl+g").unwrap(), Chord::new(Key::Char('g'), Mods::CTRL));
        assert_eq!(Chord::parse("Ctrl+Space").unwrap(), Chord::new(Key::Char(' '), Mods::CTRL));
        assert_eq!(Chord::parse("alt+shift+x").unwrap(), Chord::new(Key::Char('x'), Mods::ALT | Mods::SHIFT));
        assert_eq!(Chord::parse("ctrl+B").unwrap(), Chord::new(Key::Char('b'), Mods::CTRL | Mods::SHIFT));
        assert_eq!(Chord::parse("ctrl++").unwrap(), Chord::new(Key::Char('+'), Mods::CTRL));
        assert_eq!(Chord::parse("ctrl+-").unwrap(), Chord::new(Key::Char('-'), Mods::CTRL));
        assert_eq!(Chord::parse("f12").unwrap(), Chord::new(Key::F(12), NONE));
        assert!(Chord::parse("hyper+g").is_err());
        assert!(Chord::parse("ctrl+foo").is_err());
        assert!(Chord::parse("").is_err());
        assert_eq!(Chord::parse("ctrl+g").unwrap().label(), "ctrl+g");
        assert_eq!(Chord::new(Key::Char('h'), Mods::SHIFT).label(), "H");
    }

    #[test]
    fn zellij_preset() {
        let k = km(Preset::Zellij);
        let ch = |c| Key::Char(c);
        // One step: quick keys in Normal mode.
        assert_eq!(k.lookup(Mode::Normal, ch('h'), Mods::ALT), Lookup::Do(Action::FocusOrTab(Dir::Left)));
        assert_eq!(k.lookup(Mode::Normal, ch('j'), Mods::ALT), Lookup::Do(Action::Focus(Dir::Down)));
        assert_eq!(k.lookup(Mode::Normal, ch('3'), Mods::ALT), Lookup::Do(Action::GotoTab(2)));
        assert_eq!(k.lookup(Mode::Normal, ch('t'), Mods::ALT), Lookup::Do(Action::NewTab));
        // Modes without a leader; lock state bits do not break matching.
        assert_eq!(k.lookup(Mode::Normal, ch('p'), Mods::CTRL | Mods::NUM_LOCK), Lookup::Do(Action::Mode(Mode::Pane)));
        assert_eq!(k.lookup(Mode::Pane, ch('p'), Mods::CTRL), Lookup::Do(Action::Mode(Mode::Normal)));
        assert_eq!(k.lookup(Mode::Pane, ch('t'), Mods::CTRL), Lookup::Do(Action::Mode(Mode::Tab)));
        assert_eq!(k.lookup(Mode::Pane, ch('r'), NONE), Lookup::Do(Action::Split(Axis::X)));
        assert_eq!(k.lookup(Mode::Pane, ch('q'), NONE), Lookup::Swallow);
        assert_eq!(k.lookup(Mode::Normal, ch('p'), NONE), Lookup::Pass);
        assert_eq!(k.lookup(Mode::Normal, ch('g'), Mods::CTRL), Lookup::Do(Action::Mode(Mode::Locked)));
        assert_eq!(k.lookup(Mode::Locked, ch('p'), Mods::CTRL), Lookup::Pass);
        assert_eq!(k.lookup(Mode::Locked, ch('g'), Mods::CTRL), Lookup::Do(Action::Mode(Mode::Normal)));
        assert_eq!(k.lookup(Mode::Locked, ch('c'), Mods::CTRL | Mods::SHIFT), Lookup::Do(Action::Copy));
        assert_eq!(k.lookup(Mode::Resize, ch('h'), Mods::SHIFT), Lookup::Do(Action::Resize(Dir::Left, -2)));
        // `$` is Shift+4: the shifted character matches.
        let end = Lookup::Do(Action::CopyMove(Motion::LineEnd));
        assert_eq!(k.lookup_shifted(Mode::Scroll, ch('4'), Mods::SHIFT, Some('$')), end);
        assert_eq!(k.lookup(Mode::Scroll, ch('v'), Mods::SHIFT), Lookup::Do(Action::CopySelect(Select::Line)));
        assert_eq!(k.lookup(Mode::Scroll, Key::Escape, NONE), Lookup::Do(Action::CopyEscape));
        assert_eq!(k.after(Mode::Scroll, Action::CopyYank), Mode::Normal);
        assert_eq!(k.lookup(Mode::Normal, ch('o'), Mods::CTRL), Lookup::Do(Action::Mode(Mode::Session)));
        assert_eq!(k.lookup(Mode::Tab, ch('r'), NONE), Lookup::Do(Action::RenameTab));
    }

    #[test]
    fn leader_preset() {
        let k = km(Preset::Leader);
        let (g, ctrl) = (Key::Char('g'), Mods::CTRL);
        assert_eq!(k.lookup(Mode::Normal, g, ctrl), Lookup::Do(Action::Mode(Mode::Menu)));
        assert_eq!(k.lookup(Mode::Normal, Key::Char('p'), ctrl), Lookup::Pass);
        assert_eq!(k.lookup(Mode::Menu, Key::Char('p'), NONE), Lookup::Do(Action::Mode(Mode::Pane)));
        assert_eq!(k.lookup(Mode::Menu, g, ctrl), Lookup::Do(Action::Passthrough));
        assert_eq!(k.after(Mode::Menu, Action::Passthrough), Mode::Normal);
        assert_eq!(k.lookup(Mode::Pane, g, ctrl), Lookup::Do(Action::Mode(Mode::Normal)));
        assert_eq!(k.lookup(Mode::Normal, Key::Char('l'), Mods::ALT), Lookup::Do(Action::FocusOrTab(Dir::Right)));
    }

    #[test]
    fn tmux_preset() {
        let k = km(Preset::Tmux);
        let (b, ctrl) = (Key::Char('b'), Mods::CTRL);
        assert_eq!(k.lookup(Mode::Normal, b, ctrl), Lookup::Do(Action::Mode(Mode::Menu)));
        assert_eq!(k.lookup(Mode::Normal, Key::Char('p'), ctrl), Lookup::Pass);
        assert_eq!(k.lookup(Mode::Normal, Key::Char('h'), Mods::ALT), Lookup::Pass);
        let pct = k.lookup_shifted(Mode::Menu, Key::Char('5'), Mods::SHIFT, Some('%'));
        assert_eq!(pct, Lookup::Do(Action::Split(Axis::X)));
        assert_eq!(k.lookup(Mode::Menu, Key::Char('c'), NONE), Lookup::Do(Action::NewTab));
        assert_eq!(k.after(Mode::Menu, Action::NewTab), Mode::Normal);
        assert_eq!(k.lookup(Mode::Menu, b, ctrl), Lookup::Do(Action::Passthrough));
        assert_eq!(k.lookup(Mode::Menu, Key::Char('['), NONE), Lookup::Do(Action::Mode(Mode::Scroll)));
        assert_eq!(k.lookup(Mode::Tab, b, ctrl), Lookup::Do(Action::Mode(Mode::Normal)));
        assert_eq!(k.hints(Mode::Normal)[0], ("ctrl+b".to_string(), "menu"));
    }

    #[test]
    fn overrides() {
        let p = |s| Chord::parse(s).unwrap();
        let cfg = KeyConfig {
            binds: vec![
                (Mode::Normal, p("alt+t"), None),
                (Mode::Normal, p("alt+x"), Some(Action::ClosePane)),
                (Mode::Normal, p("alt+n"), Some(Action::Split(Axis::X))),
                (Mode::Pane, p("ctrl+p"), Some(Action::Zoom)),
            ],
            ..KeyConfig::default()
        };
        let k = Keymap::new(&cfg);
        assert_eq!(k.lookup(Mode::Normal, Key::Char('t'), Mods::ALT), Lookup::Pass);
        assert_eq!(k.lookup(Mode::Normal, Key::Char('x'), Mods::ALT), Lookup::Do(Action::ClosePane));
        assert_eq!(k.lookup(Mode::Normal, Key::Char('n'), Mods::ALT), Lookup::Do(Action::Split(Axis::X)));
        assert_eq!(k.lookup(Mode::Pane, Key::Char('p'), Mods::CTRL), Lookup::Do(Action::Zoom));
    }

    #[test]
    fn sticky_and_repeatable() {
        let k = km(Preset::Zellij);
        assert_eq!(k.after(Mode::Pane, Action::Split(Axis::X)), Mode::Normal);
        assert_eq!(k.after(Mode::Resize, Action::Equalize), Mode::Resize);
        assert_eq!(k.after(Mode::Normal, Action::Focus(Dir::Left)), Mode::Normal);
        let sticky = Keymap::new(&KeyConfig { sticky: true, ..KeyConfig::default() });
        assert_eq!(sticky.after(Mode::Pane, Action::Split(Axis::X)), Mode::Pane);
        assert_eq!(sticky.after(Mode::Pane, Action::Mode(Mode::Normal)), Mode::Normal);
    }

    #[test]
    fn hints_follow_bindings() {
        let k = km(Preset::Zellij);
        let h = k.hints(Mode::Pane);
        assert_eq!(h[0], ("hjkl".to_string(), "focus"));
        assert!(h.contains(&("d".to_string(), "split down")));
        assert!(h.contains(&("f".to_string(), "zoom")));
        assert_eq!(h.last().unwrap(), &("esc".to_string(), "back"));
        assert_eq!(k.hints(Mode::Resize)[1], ("HJKL".to_string(), "shrink"));
        assert_eq!(k.hints(Mode::Tab)[1], ("1-9".to_string(), "go to tab"));
        assert_eq!(k.hints(Mode::Locked), vec![("ctrl+g".to_string(), "unlock")]);
        let n = k.hints(Mode::Normal);
        assert_eq!(n[0], ("ctrl+g".to_string(), "lock"));
        assert_eq!(n[1], ("ctrl+p".to_string(), "pane"));
        assert!(n.contains(&("alt+hjkl".to_string(), "focus")));
        assert!(n.contains(&("alt+[]".to_string(), "prev/next tab")));
        assert!(n.contains(&("alt+1-9".to_string(), "go to tab")));
        assert!(!n.iter().any(|h| h.1 == "page"));
        assert_eq!(km(Preset::Leader).hints(Mode::Normal)[0], ("ctrl+g".to_string(), "menu"));
    }

    #[test]
    fn action_names_round_trip() {
        for a in Action::all() {
            assert_eq!(Action::from_name(&a.name()), Some(a), "{}", a.name());
        }
        for m in MODES {
            let all: Vec<Action> = [Preset::Zellij, Preset::Leader, Preset::Tmux]
                .into_iter()
                .flat_map(|p| km(p).bindings(m).iter().map(|x| x.1).collect::<Vec<_>>())
                .collect();
            for a in all {
                assert_eq!(Action::from_name(&a.name()), Some(a), "{}", a.name());
            }
        }
        assert_eq!(Action::from_name("tab.goto_12"), Some(Action::GotoTab(11)));
        assert_eq!(Action::from_name("nope"), None);
    }
}
