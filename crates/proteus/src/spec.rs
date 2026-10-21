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
//!     { name = "logs", cmd = "journalctl -f" },
//!   },
//! }
//! ```
//!
//! A tab table is a node with extra tab keys (`name`, `cwd`, `focus`,
//! `zoom`). A node is a pane (`cmd`, `run`, `cwd`, `name`, `focus`) or a
//! split (`split`, `ratio` and two or more child nodes). `cmd` replaces the
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
    Split { axis: Axis, ratio: Option<f32>, children: Vec<NodeSpec> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabSpec {
    pub name: Option<String>,
    /// Default working directory of the tab's panes.
    pub cwd: Option<PathBuf>,
    pub focus: bool,
    pub zoomed: bool,
    pub root: NodeSpec,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutSpec {
    pub tabs: Vec<TabSpec>,
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
        self.tabs.iter().map(|t| count(&t.root)).sum()
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

/// Parse a node table. `tab` allows the tab keys, which are returned as
/// (name, cwd, focus, zoom) and not treated as pane keys.
fn node(t: &Table, at: &str, base: Option<&Path>, tab: bool) -> Result<NodeSpec, String> {
    let mut children = Vec::new();
    let mut split = None;
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
                    "ratio" => {
                        let r = match v {
                            Value::Integer(i) => i as f64,
                            Value::Number(n) => n,
                            _ => return Err(format!("{key_at}: expected a number")),
                        };
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
                    "name" | "cwd" | "focus" | "zoom" if tab => {}
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
        children.push(node(&c, &child_at, base, false)?);
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
        (Some(axis), _) => Ok(NodeSpec::Split { axis, ratio, children }),
    }
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
    let root = node(t, at, cwd.as_deref(), true)?;
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
    Ok(TabSpec { name, cwd, focus, zoomed, root })
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
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair.map_err(|e| format!("{chunk}: {e}"))?;
        match k.as_str() {
            "tabs" => {
                let Value::Table(list) = v else { return Err(format!("{chunk}: tabs: expected a list")) };
                for (i, t) in list.sequence_values::<Value>().enumerate() {
                    let at = format!("tabs[{}]", i + 1);
                    match t.map_err(|e| format!("{at}: {e}"))? {
                        Value::Table(t) => spec.tabs.push(tab(&t, &at).map_err(|e| format!("{chunk}: {e}"))?),
                        _ => return Err(format!("{chunk}: {at}: expected a table")),
                    }
                }
            }
            "clean" => spec.clean = Some(boolean(&v, "clean")?),
            "version" => {}
            _ => return Err(format!("{chunk}: unknown key `{k}`")),
        }
    }
    if spec.tabs.is_empty() {
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
        NodeSpec::Split { axis, ratio, children } => {
            keys.push(format!("split = {}", quote(if *axis == Axis::X { "vertical" } else { "horizontal" })));
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

/// Lua source for `spec`, readable by [`parse`].
pub fn to_lua(spec: &LayoutSpec) -> String {
    let mut out = String::from("-- Proteus layout\nreturn {\n");
    if let Some(c) = spec.clean {
        out.push_str(&format!("  clean = {c},\n"));
    }
    out.push_str("  tabs = {\n");
    for t in &spec.tabs {
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
        // A one-pane tab: the tab's own `name`/`focus` mean the tab's.
        let root = match &t.root {
            NodeSpec::Pane(p) => NodeSpec::Pane(PaneSpec { name: None, focus: false, ..p.clone() }),
            n => n.clone(),
        };
        write_node(&root, 2, keys, t.cwd.as_deref(), &mut out);
    }
    out.push_str("  },\n}\n");
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
            NodeSpec::Split { axis, ratio, children } => {
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
                children: vec![Self::from_node(a, pane), Self::from_node(b, pane)],
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
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.pane_count(), 4);
        let t = &s.tabs[0];
        assert_eq!(t.name.as_deref(), Some("code"));
        assert!(t.focus);
        let NodeSpec::Split { axis: Axis::X, ratio: Some(r), children } = &t.root else { panic!("{t:?}") };
        assert!((r - 0.65).abs() < 1e-6);
        let NodeSpec::Pane(nvim) = &children[0] else { panic!() };
        assert_eq!(nvim.cmd, Some(Cmd::Shell("nvim".into())));
        assert_eq!(nvim.cwd.as_deref(), Some(Path::new("/src/app")));
        let NodeSpec::Split { children: inner, .. } = &children[1] else { panic!() };
        let NodeSpec::Pane(ls) = &inner[1] else { panic!() };
        assert_eq!(ls.run.as_deref(), Some("ls"));
        assert_eq!(ls.cwd.as_deref(), Some(Path::new("/src/app/sub")));
        let NodeSpec::Pane(logs) = &s.tabs[1].root else { panic!() };
        assert_eq!(logs.cmd.as_ref().unwrap().argv()[1..], ["-c".to_string(), "journalctl -f".into()]);
        assert_eq!(s.tabs[1].name.as_deref(), Some("logs"));
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
        session.tabs[1].cwd = Some(odd.clone());
        session.tabs[1].root = NodeSpec::Pane(PaneSpec { cwd: Some(odd), ..PaneSpec::default() });
        assert_eq!(parse(&to_lua(&session), "s").unwrap(), session);
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
        ] {
            let e = parse(src, "t.lua").unwrap_err();
            assert!(e.contains(want), "{src}: {e}");
        }
    }

    #[test]
    fn tree_conversion() {
        let s = parse("return { tabs = { { split = 'vertical', {}, { focus = true }, {} } } }", "t").unwrap();
        let mut next = 0;
        let (node, focus) = s.tabs[0]
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
