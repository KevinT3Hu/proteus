//! Layout files and the saved session: windows' tabs and split trees as
//! Lua data (doc 02, "Layouts" and "Session restore").
//!
//! ```lua
//! return {
//!   tabs = {
//!     { name = "code", cwd = "~/src/app", focus = true,
//!       split = "vertical", ratio = 0.65,          -- side by side
//!       { cmd = "nvim" },
//!       { split = "horizontal",                    -- stacked
//!         { cmd = { "cargo", "watch", "-x", "test" } },
//!         { run = "git status" },                  -- typed into the shell
//!       },
//!     },
//!     { name = "logs", cmd = "journalctl -f",
//!       floating = { { run = "htop", x = 10, y = 3, width = 80, height = 20 } },
//!       show_floating = true,
//!     },
//!     { split = "horizontal", stacked = true, { run = "make" }, { run = "git log" } },
//!   },
//! }
//! ```
//!
//! The session file (and a layout of several windows) uses
//! `windows = { { tabs = { ... } }, ... }` instead of `tabs`.
//!
//! A tab table is a node with extra tab keys (`name`, `cwd`, `focus`,
//! `zoom`). A node is a pane (`cmd`, `run`, `cwd`, `name`, `focus`) or a
//! split (`split`, `ratio`, optionally `stacked = true`, and two or more
//! child nodes; a stack's children must be panes). `cmd` replaces the
//! shell (a string runs through `$SHELL -c`); `run` is typed into the shell,
//! so the pane stays open when the command ends.

use std::path::{Path, PathBuf};

use mlua::{Lua, LuaOptions, StdLib, Table, Value};

use crate::layout::{Axis, Node, PaneId};

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// A command line for the shell.
    Shell(String),
    Argv(Vec<String>),
}

impl Cmd {
    /// Program and arguments to execute.
    pub fn argv(&self) -> Vec<String> {
        match self {
            Cmd::Argv(v) => v.clone(),
            Cmd::Shell(s) => {
                let sh = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
                vec![sh, "-c".into(), s.clone()]
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneSpec {
    pub cmd: Option<Cmd>,
    /// Typed into the shell once it starts.
    pub run: Option<String>,
    pub cwd: Option<PathBuf>,
    pub name: Option<String>,
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeSpec {
    Pane(PaneSpec),
    Split { axis: Axis, ratio: Option<f32>, stacked: bool, children: Vec<NodeSpec> },
}

/// A floating pane: the pane and its grid position/size in cells.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatSpec {
    pub pane: PaneSpec,
    /// (col, row, cols, rows); `None` = default placement.
    pub geometry: Option<(i32, i32, i32, i32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabSpec {
    pub name: Option<String>,
    /// Default working directory of the tab's panes.
    pub cwd: Option<PathBuf>,
    pub focus: bool,
    pub zoomed: bool,
    pub root: NodeSpec,
    pub floating: Vec<FloatSpec>,
    pub show_floating: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutSpec {
    /// Tabs of each window.
    pub windows: Vec<Vec<TabSpec>>,
    /// Session file only: the program exited normally.
    pub clean: Option<bool>,
}

impl LayoutSpec {
    pub fn pane_count(&self) -> usize {
        fn count(n: &NodeSpec) -> usize {
            match n {
                NodeSpec::Pane(_) => 1,
                NodeSpec::Split { children, .. } => children.iter().map(count).sum(),
            }
        }
        self.tabs().map(|t| count(&t.root) + t.floating.len()).sum()
    }

    pub fn tabs(&self) -> impl Iterator<Item = &TabSpec> {
        self.windows.iter().flatten()
    }

    /// One window of tabs.
    pub fn single(tabs: Vec<TabSpec>) -> Self {
        Self { windows: vec![tabs], clean: None }
    }
}

// --- reading -----------------------------------------------------------------

/// `~` and `~/…` expand to `$HOME`; relative paths are relative to `base`.
fn expand(path: &str, base: Option<&Path>) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let p = match (path.strip_prefix('~'), &home) {
        (Some(""), Some(h)) => h.clone(),
        (Some(rest), Some(h)) if rest.starts_with('/') => h.join(&rest[1..]),
        _ => PathBuf::from(path),
    };
    match base {
        Some(b) if p.is_relative() => b.join(p),
        _ => p,
    }
}

fn string(v: &Value, at: &str) -> Result<String, String> {
    match v {
        Value::String(s) => s.to_str().map(|s| s.to_string()).map_err(|_| format!("{at}: not valid UTF-8")),
        _ => Err(format!("{at}: expected a string")),
    }
}

fn boolean(v: &Value, at: &str) -> Result<bool, String> {
    match v {
        Value::Boolean(b) => Ok(*b),
        _ => Err(format!("{at}: expected true or false")),
    }
}

const PANE_KEYS: [&str; 5] = ["cmd", "run", "cwd", "name", "focus"];

/// Which extra keys a node table may have.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Tab keys (read by `tab()`) are skipped here.
    Tab,
    Child,
    /// Floating pane geometry keys (read by `float()`) are skipped here.
    Float,
}

fn number(v: &Value, at: &str) -> Result<f64, String> {
    match v {
        Value::Integer(i) => Ok(*i as f64),
        Value::Number(n) => Ok(*n),
        _ => Err(format!("{at}: expected a number")),
    }
}

/// Parse a node table.
fn node(t: &Table, at: &str, base: Option<&Path>, kind: Kind) -> Result<NodeSpec, String> {
    let tab = kind == Kind::Tab;
    let mut children = Vec::new();
    let mut split = None;
    let mut stacked = false;
    let mut ratio = None;
    let mut pane = PaneSpec::default();
    let mut pane_keys = false;
    for pair in t.pairs::<Value, Value>() {
        let (k, v) = pair.map_err(|e| format!("{at}: {e}"))?;
        match k {
            Value::Integer(_) => {}
            Value::String(k) => {
                let k = k.to_str().map_err(|_| format!("{at}: bad key"))?.to_string();
                let key_at = format!("{at}.{k}");
                match k.as_str() {
                    "split" => {
                        split = Some(match string(&v, &key_at)?.as_str() {
                            "vertical" | "right" | "columns" => Axis::X,
                            "horizontal" | "down" | "rows" => Axis::Y,
                            _ => return Err(format!("{key_at}: expected \"vertical\" or \"horizontal\"")),
                        })
                    }
                    "stacked" => stacked = boolean(&v, &key_at)?,
                    "ratio" => {
                        let r = number(&v, &key_at)?;
                        if !(r > 0.0 && r < 1.0) {
                            return Err(format!("{key_at}: expected a number between 0 and 1"));
                        }
                        ratio = Some(r as f32);
                    }
                    "cmd" => {
                        pane_keys = true;
                        pane.cmd = Some(match &v {
                            Value::String(_) => Cmd::Shell(string(&v, &key_at)?),
                            Value::Table(list) => {
                                let argv = list
                                    .sequence_values::<String>()
                                    .collect::<mlua::Result<Vec<_>>>()
                                    .map_err(|_| format!("{key_at}: expected a list of strings"))?;
                                if argv.is_empty() {
                                    return Err(format!("{key_at}: empty command"));
                                }
                                Cmd::Argv(argv)
                            }
                            _ => return Err(format!("{key_at}: expected a string or a list of strings")),
                        })
                    }
                    "run" => {
                        pane_keys = true;
                        pane.run = Some(string(&v, &key_at)?);
                    }
                    "plugin" => return Err(format!("{key_at}: plugin panes are not supported yet")),
                    // Tab keys are read by `tab()`.
                    "name" | "cwd" | "focus" | "zoom" | "floating" | "show_floating" if tab => {}
                    "x" | "y" | "width" | "height" if kind == Kind::Float => {}
                    "cwd" => {
                        pane_keys = true;
                        pane.cwd = Some(expand(&string(&v, &key_at)?, base));
                    }
                    "name" => {
                        pane_keys = true;
                        pane.name = Some(string(&v, &key_at)?);
                    }
                    "focus" => {
                        pane_keys = true;
                        pane.focus = boolean(&v, &key_at)?;
                    }
                    _ => return Err(format!("{at}: unknown key `{k}`")),
                }
            }
            _ => return Err(format!("{at}: unexpected key")),
        }
    }
    for (i, v) in t.clone().sequence_values::<Value>().enumerate() {
        let v = v.map_err(|e| format!("{at}: {e}"))?;
        let child_at = format!("{at}[{}]", i + 1);
        let Value::Table(c) = v else { return Err(format!("{child_at}: expected a table")) };
        children.push(node(&c, &child_at, base, Kind::Child)?);
    }
    if kind == Kind::Float && !children.is_empty() {
        return Err(format!("{at}: a floating pane cannot be split"));
    }
    match (split, children.len()) {
        (None, 0) => {
            if tab {
                pane.cwd = pane.cwd.or_else(|| base.map(Path::to_path_buf));
            }
            Ok(NodeSpec::Pane(pane))
        }
        (Some(_), 0 | 1) => Err(format!("{at}: a split needs at least two panes")),
        (None, _) => Err(format!("{at}: child panes need `split = \"vertical\"` or `\"horizontal\"`")),
        (Some(_), _) if pane_keys => {
            Err(format!("{at}: a split cannot have pane keys ({})", PANE_KEYS.join(", ")))
        }
        (Some(_), _) if stacked && children.iter().any(|c| !matches!(c, NodeSpec::Pane(_))) => {
            Err(format!("{at}: a stack's children must be panes"))
        }
        (Some(axis), _) => Ok(NodeSpec::Split { axis, ratio, stacked, children }),
    }
}

fn float(t: &Table, at: &str, base: Option<&Path>) -> Result<FloatSpec, String> {
    let NodeSpec::Pane(pane) = node(t, at, base, Kind::Float)? else { unreachable!("floats are panes") };
    let mut g = [None; 4];
    for (i, k) in ["x", "y", "width", "height"].iter().enumerate() {
        match t.raw_get::<Value>(*k).map_err(|e| format!("{at}.{k}: {e}"))? {
            Value::Nil => {}
            v => g[i] = Some(number(&v, &format!("{at}.{k}"))?.round() as i32),
        }
    }
    let geometry = match g {
        [Some(x), Some(y), Some(w), Some(h)] => Some((x, y, w, h)),
        [None, None, None, None] => None,
        _ => return Err(format!("{at}: give all of x, y, width and height, or none")),
    };
    Ok(FloatSpec { pane, geometry })
}

fn tab(t: &Table, at: &str) -> Result<TabSpec, String> {
    let get = |k: &str| t.raw_get::<Value>(k).map_err(|e| format!("{at}.{k}: {e}"));
    let name = match get("name")? {
        Value::Nil => None,
        v => Some(string(&v, &format!("{at}.name"))?),
    };
    let cwd = match get("cwd")? {
        Value::Nil => None,
        v => Some(expand(&string(&v, &format!("{at}.cwd"))?, None)),
    };
    let focus = match get("focus")? {
        Value::Nil => false,
        v => boolean(&v, &format!("{at}.focus"))?,
    };
    let zoomed = match get("zoom")? {
        Value::Nil => false,
        v => boolean(&v, &format!("{at}.zoom"))?,
    };
    let show_floating = match get("show_floating")? {
        Value::Nil => false,
        v => boolean(&v, &format!("{at}.show_floating"))?,
    };
    let mut floating = Vec::new();
    match get("floating")? {
        Value::Nil => {}
        Value::Table(list) => {
            for (i, f) in list.sequence_values::<Value>().enumerate() {
                let fat = format!("{at}.floating[{}]", i + 1);
                match f.map_err(|e| format!("{fat}: {e}"))? {
                    Value::Table(f) => {
                        let mut f = float(&f, &fat, cwd.as_deref())?;
                        if f.pane.cwd.is_none() {
                            f.pane.cwd = cwd.clone();
                        }
                        floating.push(f);
                    }
                    _ => return Err(format!("{fat}: expected a table")),
                }
            }
        }
        _ => return Err(format!("{at}.floating: expected a list")),
    }
    let root = node(t, at, cwd.as_deref(), Kind::Tab)?;
    // Panes without their own cwd use the tab's.
    fn inherit(n: &mut NodeSpec, cwd: &Option<PathBuf>) {
        match n {
            NodeSpec::Pane(p) => {
                if p.cwd.is_none() {
                    p.cwd = cwd.clone();
                }
            }
            NodeSpec::Split { children, .. } => children.iter_mut().for_each(|c| inherit(c, cwd)),
        }
    }
    let mut root = root;
    inherit(&mut root, &cwd);
    Ok(TabSpec { name, cwd, focus, zoomed, root, floating, show_floating })
}

/// Evaluate layout (or session) source. `chunk` names it in errors.
pub fn parse(src: &str, chunk: &str) -> Result<LayoutSpec, String> {
    let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).map_err(|e| e.to_string())?;
    let value: Value = lua.load(src).set_name(format!("@{chunk}")).eval().map_err(|e| {
        let s = e.to_string();
        s.lines().next().unwrap_or("").trim_start_matches("runtime error: ").to_string()
    })?;
    let Value::Table(t) = value else { return Err(format!("{chunk}: the file must `return {{ tabs = {{ ... }} }}`")) };
    let mut spec = LayoutSpec::default();
    let tabs = |v: Value, prefix: &str| -> Result<Vec<TabSpec>, String> {
        let Value::Table(list) = v else { return Err(format!("{chunk}: {prefix}tabs: expected a list")) };
        let mut out = Vec::new();
        for (i, t) in list.sequence_values::<Value>().enumerate() {
            let at = format!("{prefix}tabs[{}]", i + 1);
            match t.map_err(|e| format!("{at}: {e}"))? {
                Value::Table(t) => out.push(tab(&t, &at).map_err(|e| format!("{chunk}: {e}"))?),
                _ => return Err(format!("{chunk}: {at}: expected a table")),
            }
        }
        if out.is_empty() {
            return Err(format!("{chunk}: {prefix}no tabs"));
        }
        Ok(out)
    };
    let mut seen_tabs = false;
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| format!("{chunk}: {e}"))?;
        match k.as_str() {
            "tabs" => {
                seen_tabs = true;
                spec.windows.insert(0, tabs(v, "")?);
            }
            "windows" => {
                let Value::Table(list) = v else { return Err(format!("{chunk}: windows: expected a list")) };
                for (i, w) in list.sequence_values::<Value>().enumerate() {
                    let at = format!("windows[{}].", i + 1);
                    let Ok(Value::Table(w)) = w else { return Err(format!("{chunk}: {at}: expected a table")) };
                    for pair in w.pairs::<String, Value>() {
                        let (k, _) = pair.map_err(|e| format!("{chunk}: {e}"))?;
                        if k != "tabs" {
                            return Err(format!("{chunk}: {at}: unknown key `{k}`"));
                        }
                    }
                    let v = w.raw_get::<Value>("tabs").map_err(|e| e.to_string())?;
                    spec.windows.push(tabs(v, &at)?);
                }
            }
            "clean" => spec.clean = Some(boolean(&v, "clean")?),
            "version" => {}
            _ => return Err(format!("{chunk}: unknown key `{k}`")),
        }
    }
    if seen_tabs && spec.windows.len() > 1 {
        return Err(format!("{chunk}: use either `tabs` or `windows`"));
    }
    if spec.windows.is_empty() {
        return Err(format!("{chunk}: no tabs"));
    }
    Ok(spec)
}

pub fn load(path: &Path) -> Result<LayoutSpec, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&src, &path.display().to_string())
}

/// `$XDG_CONFIG_HOME/proteus/layouts`.
pub fn layouts_dir() -> Option<PathBuf> {
    Some(crate::config::default_path()?.parent()?.join("layouts"))
}

/// A layout name (`dev`) or a path (`./dev.lua`, `/x/y.lua`).
pub fn resolve(name: &str) -> Option<PathBuf> {
    if name.contains('/') || name.ends_with(".lua") {
        return Some(expand(name, None));
    }
    Some(layouts_dir()?.join(format!("{name}.lua")))
}

// --- writing -----------------------------------------------------------------

/// A Lua string literal.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\x7f' => out.push_str(&format!("\\{}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Paths under `$HOME` are written as `~/…` so layouts stay portable.
fn path_str(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = p.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
    }
    p.display().to_string()
}

fn write_pane_keys(p: &PaneSpec, out: &mut Vec<String>, tab_cwd: Option<&Path>) {
    match &p.cmd {
        Some(Cmd::Shell(s)) => out.push(format!("cmd = {}", quote(s))),
        Some(Cmd::Argv(v)) => {
            out.push(format!("cmd = {{ {} }}", v.iter().map(|a| quote(a)).collect::<Vec<_>>().join(", ")))
        }
        None => {}
    }
    if let Some(r) = &p.run {
        out.push(format!("run = {}", quote(r)));
    }
    if let Some(c) = p.cwd.as_deref().filter(|c| Some(*c) != tab_cwd) {
        out.push(format!("cwd = {}", quote(&path_str(c))));
    }
    if let Some(n) = &p.name {
        out.push(format!("name = {}", quote(n)));
    }
    if p.focus {
        out.push("focus = true".into());
    }
}

fn write_node(n: &NodeSpec, indent: usize, keys: Vec<String>, tab_cwd: Option<&Path>, out: &mut String) {
    let pad = "  ".repeat(indent);
    let mut keys = keys;
    match n {
        NodeSpec::Pane(p) => {
            write_pane_keys(p, &mut keys, tab_cwd);
            out.push_str(&format!("{pad}{{ {} }},\n", keys.join(", ")).replace("{  }", "{}"));
        }
        NodeSpec::Split { axis, ratio, stacked, children } => {
            keys.push(format!("split = {}", quote(if *axis == Axis::X { "vertical" } else { "horizontal" })));
            if *stacked {
                keys.push("stacked = true".into());
            }
            if let Some(r) = ratio {
                keys.push(format!("ratio = {}", (r * 1000.0).round() / 1000.0));
            }
            out.push_str(&format!("{pad}{{ {},\n", keys.join(", ")));
            for c in children {
                write_node(c, indent + 1, Vec::new(), tab_cwd, out);
            }
            out.push_str(&format!("{pad}}},\n"));
        }
    }
}

fn write_tabs(tabs: &[TabSpec], indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    out.push_str(&format!("{pad}tabs = {{\n"));
    for t in tabs {
        let mut keys = Vec::new();
        if let Some(n) = &t.name {
            keys.push(format!("name = {}", quote(n)));
        }
        if let Some(c) = &t.cwd {
            keys.push(format!("cwd = {}", quote(&path_str(c))));
        }
        if t.focus {
            keys.push("focus = true".into());
        }
        if t.zoomed {
            keys.push("zoom = true".into());
        }
        if t.show_floating {
            keys.push("show_floating = true".into());
        }
        if !t.floating.is_empty() {
            let floats: Vec<String> = t
                .floating
                .iter()
                .map(|f| {
                    let mut k = Vec::new();
                    write_pane_keys(&f.pane, &mut k, t.cwd.as_deref());
                    if let Some((x, y, w, h)) = f.geometry {
                        k.push(format!("x = {x}, y = {y}, width = {w}, height = {h}"));
                    }
                    format!("{{ {} }}", k.join(", ")).replace("{  }", "{}")
                })
                .collect();
            keys.push(format!("floating = {{ {} }}", floats.join(", ")));
        }
        // A one-pane tab: the tab's own `name`/`focus` mean the tab's.
        let root = match &t.root {
            NodeSpec::Pane(p) => NodeSpec::Pane(PaneSpec { name: None, focus: false, ..p.clone() }),
            n => n.clone(),
        };
        write_node(&root, indent + 1, keys, t.cwd.as_deref(), out);
    }
    out.push_str(&format!("{pad}}},\n"));
}

/// Lua source for `spec`, readable by [`parse`].
pub fn to_lua(spec: &LayoutSpec) -> String {
    let mut out = String::from("-- Proteus layout\nreturn {\n");
    if let Some(c) = spec.clean {
        out.push_str(&format!("  clean = {c},\n"));
    }
    if let [tabs] = spec.windows.as_slice() {
        write_tabs(tabs, 1, &mut out);
    } else {
        out.push_str("  windows = {\n");
        for w in &spec.windows {
            out.push_str("    {\n");
            write_tabs(w, 3, &mut out);
            out.push_str("    },\n");
        }
        out.push_str("  },\n");
    }
    out.push_str("}\n");
    out
}

// --- conversion to and from split trees ------------------------------------------

impl NodeSpec {
    /// Build a split tree, creating panes with `spawn`. Splits with more
    /// than two children become nested splits with equal shares (the first
    /// child gets `ratio` if given). Returns the tree and the focused pane.
    pub fn to_node<E>(
        &self,
        spawn: &mut impl FnMut(&PaneSpec) -> Result<PaneId, E>,
    ) -> Result<(Node, Option<PaneId>), E> {
        match self {
            NodeSpec::Pane(p) => {
                let id = spawn(p)?;
                Ok((Node::Leaf(id), p.focus.then_some(id)))
            }
            NodeSpec::Split { stacked: true, children, .. } => {
                let mut panes = Vec::with_capacity(children.len());
                let mut active = 0;
                for (i, c) in children.iter().enumerate() {
                    let NodeSpec::Pane(p) = c else { continue };
                    panes.push(spawn(p)?);
                    if p.focus {
                        active = i;
                    }
                }
                let focus = children.iter().any(|c| matches!(c, NodeSpec::Pane(p) if p.focus)).then(|| panes[active]);
                Ok((Node::Stack { panes, active }, focus))
            }
            NodeSpec::Split { axis, ratio, children, .. } => {
                let mut built = Vec::with_capacity(children.len());
                let mut focus = None;
                for c in children {
                    let (n, f) = c.to_node(spawn)?;
                    focus = focus.or(f);
                    built.push(n);
                }
                let n = built.len();
                let mut node = built.pop().expect("split has children");
                for (i, child) in built.into_iter().enumerate().rev() {
                    let share = if i == 0 && let Some(r) = ratio { *r } else { 1.0 / (n - i) as f32 };
                    node = Node::Split { axis: *axis, ratio: share, a: Box::new(child), b: Box::new(node) };
                }
                Ok((node, focus))
            }
        }
    }

    /// Describe a split tree; `pane` describes each leaf.
    pub fn from_node(n: &Node, pane: &mut impl FnMut(PaneId) -> PaneSpec) -> NodeSpec {
        match n {
            Node::Leaf(id) => NodeSpec::Pane(pane(*id)),
            Node::Split { axis, ratio, a, b } => NodeSpec::Split {
                axis: *axis,
                ratio: Some(*ratio),
                stacked: false,
                children: vec![Self::from_node(a, pane), Self::from_node(b, pane)],
            },
            Node::Stack { panes, .. } => NodeSpec::Split {
                axis: Axis::Y,
                ratio: None,
                stacked: true,
                children: panes.iter().map(|&id| NodeSpec::Pane(pane(id))).collect(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
        return {
          tabs = {
            { name = "code", cwd = "/src/app", focus = true,
              split = "vertical", ratio = 0.65,
              { cmd = "nvim" },
              { split = "horizontal", { cmd = { "cargo", "watch" }, focus = true }, { run = "ls", cwd = "sub" } },
            },
            { name = "logs", cmd = "journalctl -f" },
          },
        }
    "#;

    #[test]
    fn parse_doc_example() {
        let s = parse(DOC, "dev.lua").unwrap();
        assert_eq!(s.windows[0].len(), 2);
        assert_eq!(s.pane_count(), 4);
        let t = &s.windows[0][0];
        assert_eq!(t.name.as_deref(), Some("code"));
        assert!(t.focus);
        let NodeSpec::Split { axis: Axis::X, ratio: Some(r), children, .. } = &t.root else { panic!("{t:?}") };
        assert!((r - 0.65).abs() < 1e-6);
        let NodeSpec::Pane(nvim) = &children[0] else { panic!() };
        assert_eq!(nvim.cmd, Some(Cmd::Shell("nvim".into())));
        assert_eq!(nvim.cwd.as_deref(), Some(Path::new("/src/app")));
        let NodeSpec::Split { children: inner, .. } = &children[1] else { panic!() };
        let NodeSpec::Pane(ls) = &inner[1] else { panic!() };
        assert_eq!(ls.run.as_deref(), Some("ls"));
        assert_eq!(ls.cwd.as_deref(), Some(Path::new("/src/app/sub")));
        let NodeSpec::Pane(logs) = &s.windows[0][1].root else { panic!() };
        assert_eq!(logs.cmd.as_ref().unwrap().argv()[1..], ["-c".to_string(), "journalctl -f".into()]);
        assert_eq!(s.windows[0][1].name.as_deref(), Some("logs"));
        assert_eq!(logs.name, None);
    }

    #[test]
    fn round_trip() {
        let s = parse(DOC, "dev.lua").unwrap();
        let src = to_lua(&s);
        assert_eq!(parse(&src, "again").unwrap(), s, "{src}");
        let mut session = s.clone();
        session.clean = Some(false);
        // In a one-pane tab, `cwd` is both the tab's and the pane's.
        let odd = PathBuf::from("/tmp/a \"b\"\n");
        session.windows[0][1].cwd = Some(odd.clone());
        session.windows[0][1].root = NodeSpec::Pane(PaneSpec { cwd: Some(odd), ..PaneSpec::default() });
        assert_eq!(parse(&to_lua(&session), "s").unwrap(), session);
        // Several windows, floats and stacks.
        let mut multi = session.clone();
        let mut extra = s.windows[0].clone();
        extra[0].floating = vec![
            // Floats inherit the tab's cwd.
            FloatSpec { pane: PaneSpec { run: Some("htop".into()), cwd: Some("/src/app".into()), ..PaneSpec::default() }, geometry: Some((3, 2, 40, 10)) },
            FloatSpec { pane: PaneSpec { cwd: Some("/src/app".into()), ..PaneSpec::default() }, geometry: None },
        ];
        extra[0].show_floating = true;
        extra[1].root = NodeSpec::Split {
            axis: Axis::Y,
            ratio: None,
            stacked: true,
            children: vec![NodeSpec::Pane(PaneSpec::default()), NodeSpec::Pane(PaneSpec { focus: true, ..PaneSpec::default() })],
        };
        multi.windows.push(extra);
        let src = to_lua(&multi);
        assert!(src.contains("windows = {"), "{src}");
        assert_eq!(parse(&src, "m").unwrap(), multi, "{src}");
    }

    #[test]
    fn errors() {
        for (src, want) in [
            ("return {}", "no tabs"),
            ("return 3", "must `return"),
            ("return { tabs = { { split = 'vertical', {} } } }", "at least two"),
            ("return { tabs = { { {}, {} } } }", "need `split"),
            ("return { tabs = { { cmd = 'x', split = 'horizontal', {}, {} } } }", "pane keys"),
            ("return { tabs = { { split = 'diagonal', {}, {} } } }", "tabs[1].split"),
            ("return { tabs = { { split = 'vertical', ratio = 2, {}, {} } } }", "between 0 and 1"),
            ("return { tabs = { { split = 'vertical', {}, { comand = 'x' } } } }", "tabs[1][2]: unknown key `comand`"),
            ("return { tabs = { { plugin = 'files' } } }", "plugin"),
            ("return { tabz = {} }", "unknown key `tabz`"),
            ("return { tabs = { { split = 'vertical', stacked = true, {}, { split = 'vertical', {}, {} } } } }", "must be panes"),
            ("return { tabs = { { floating = { { x = 1 } } } } }", "x, y, width and height"),
            ("return { tabs = { { floating = { { split = 'vertical', {}, {} } } } } }", "cannot be split"),
            ("return { windows = { { tabs = {} } } }", "no tabs"),
            ("return { tabs = { {} }, windows = { { tabs = { {} } } } }", "either"),
        ] {
            let e = parse(src, "t.lua").unwrap_err();
            assert!(e.contains(want), "{src}: {e}");
        }
    }

    #[test]
    fn tree_conversion() {
        let s = parse("return { tabs = { { split = 'vertical', {}, { focus = true }, {} } } }", "t").unwrap();
        let mut next = 0;
        let (node, focus) = s.windows[0][0]
            .root
            .to_node(&mut |_| -> Result<PaneId, ()> {
                next += 1;
                Ok(next)
            })
            .unwrap();
        assert_eq!(focus, Some(2));
        let mut ids = Vec::new();
        node.leaves(&mut ids);
        assert_eq!(ids, [1, 2, 3]);
        let Node::Split { ratio, b, .. } = &node else { panic!() };
        assert!((ratio - 1.0 / 3.0).abs() < 1e-6);
        let Node::Split { ratio, .. } = &**b else { panic!() };
        assert!((ratio - 0.5).abs() < 1e-6);
        let back = NodeSpec::from_node(&node, &mut |id| PaneSpec { focus: id == 2, ..PaneSpec::default() });
        let NodeSpec::Split { children, .. } = back else { panic!() };
        assert_eq!(children.len(), 2);
        assert!(resolve("dev").unwrap().ends_with("layouts/dev.lua"));
        assert_eq!(resolve("/x/y.lua").unwrap(), PathBuf::from("/x/y.lua"));
    }
}
