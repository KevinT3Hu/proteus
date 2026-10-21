//! The Lua scripting host (doc 05): the state `init.lua` ran in stays alive,
//! so the functions it registered can run later — bound to keys, as event
//! hooks, status-bar segments, palette entries and timers.
//!
//! While a callback runs, the terminal is reachable through a [`Host`];
//! outside callbacks (while the config loads) the pane functions raise an
//! error. Every callback has a time budget: slower than [`SLOW`] warns once,
//! longer than [`RUNAWAY`] is stopped.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Value, VmState};
use proteus_core::BlockPart;

use crate::layout::{Axis, PaneId};
use crate::modes::Action;
use crate::spec::Cmd;

/// Index of a registered Lua function (see [`Action::Lua`]).
pub type FnId = u16;

/// Events `p.on` accepts.
pub const EVENTS: &[&str] = &[
    "pane_opened",
    "pane_closed",
    "focus_changed",
    "cwd_changed",
    "title_changed",
    "command_started",
    "command_finished",
    "bell",
    "config_changed",
];

/// A callback slower than this is reported (once per function).
pub const SLOW: Duration = Duration::from_millis(5);
/// A callback running longer than this is stopped with an error.
pub const RUNAWAY: Duration = Duration::from_secs(2);
/// Time allowed for `init.lua` itself.
pub const LOAD_LIMIT: Duration = Duration::from_secs(5);
/// Shortest timer interval.
const MIN_TIMER: f64 = 0.01;

const HOST: &str = "proteus.host";
const PRELUDE: &str = include_str!("prelude.lua");
/// LuaLS annotations of the API (`proteus --emit-lua-types`).
pub const TYPES: &str = include_str!("proteus.lua");

/// Where a status-bar segment goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// After the tabs.
    Left,
    /// At the right end of the tab bar.
    Right,
}

/// A timer the host should start (`p.after`, `p.every`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimerSpec {
    pub id: u32,
    pub after: Duration,
    pub repeat: bool,
}

/// Something to tell the user (errors in callbacks, slow callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    pub error: bool,
}

#[derive(Default)]
struct Registry {
    functions: Vec<Function>,
    hooks: Vec<(&'static str, FnId)>,
    segments: Vec<(Side, FnId)>,
    palette: Vec<(String, FnId)>,
    /// Running timers: function, interval, repeating.
    timers: HashMap<u32, (FnId, Duration, bool)>,
    new_timers: Vec<TimerSpec>,
    next_timer: u32,
    /// Functions already reported as slow.
    slow: HashSet<FnId>,
}

impl Registry {
    fn add(&mut self, f: Function) -> mlua::Result<FnId> {
        let id =
            FnId::try_from(self.functions.len()).map_err(|_| mlua::Error::runtime("too many registered functions"))?;
        self.functions.push(f);
        Ok(id)
    }
}

/// Registers functions from outside this module (`p.keymap` in the config).
#[derive(Clone)]
pub struct Registrar(Rc<RefCell<Registry>>);

impl Registrar {
    pub fn add(&self, f: Function) -> mlua::Result<FnId> {
        self.0.borrow_mut().add(f)
    }
}

/// Pane metadata as scripts see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneInfo {
    pub id: PaneId,
    /// What the tab bar shows (name, else title, else process).
    pub title: String,
    /// Set by the user (`pane.rename`, `pane:set_name`).
    pub name: Option<String>,
    pub cwd: Option<String>,
    /// Foreground program name (the shell when idle).
    pub process: Option<String>,
    /// Command line of the running command block (shell integration).
    pub command: Option<String>,
    pub window: u32,
    /// 1-based tab index in its window.
    pub tab: usize,
    /// The focused pane of the active tab of its window.
    pub focused: bool,
    pub floating: bool,
    pub cols: usize,
    pub rows: usize,
}

/// A command block (shell integration) as scripts see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockInfo {
    pub id: u64,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub exit_code: Option<i32>,
    /// Seconds (so far, while running).
    pub duration: Option<f64>,
    /// `prompt`, `input`, `running` or `finished`.
    pub state: &'static str,
}

/// What a new pane runs and where.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spawn {
    pub cmd: Option<Cmd>,
    pub cwd: Option<String>,
}

/// The terminal, as far as scripts can reach it. Pane ids that no longer
/// exist are ignored by the mutating calls.
pub trait Host {
    fn pane_ids(&self) -> Vec<PaneId>;
    fn focused(&self) -> Option<PaneId>;
    fn pane_info(&self, id: PaneId) -> Option<PaneInfo>;
    /// `None`: along the longer side.
    fn split(&mut self, id: PaneId, axis: Option<Axis>, spawn: Spawn) -> Result<PaneId, String>;
    fn new_tab(&mut self, spawn: Spawn) -> Result<PaneId, String>;
    fn float(&mut self, spawn: Spawn) -> Result<PaneId, String>;
    fn close(&mut self, id: PaneId);
    fn focus(&mut self, id: PaneId);
    /// `paste`: as a paste (bracketed if the application asked for it).
    fn send_text(&mut self, id: PaneId, text: &str, paste: bool);
    fn set_name(&mut self, id: PaneId, name: Option<String>);
    /// The screen, with up to `scrollback` lines above it.
    fn read_screen(&self, id: PaneId, scrollback: usize) -> Option<String>;
    fn read_blocks(&self, id: PaneId) -> Vec<BlockInfo>;
    fn block_text(&self, id: PaneId, block: u64, part: BlockPart) -> Option<String>;
    /// Run an action on the focused pane of the keyboard window.
    fn run_action(&mut self, action: Action);
    fn notify(&mut self, text: String, error: bool, timeout: Option<Duration>);
}

/// A field value of an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Nil,
    Int(i64),
    Num(f64),
    Str(String),
    Pane(PaneId),
}

/// An event for `p.on` hooks: `{ name = ..., pane = <pane>, ... }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptEvent {
    pub name: &'static str,
    pub fields: Vec<(&'static str, Val)>,
}

impl ScriptEvent {
    pub fn new(name: &'static str) -> Self {
        debug_assert!(EVENTS.contains(&name), "unknown event {name}");
        Self { name, fields: Vec::new() }
    }

    pub fn with(mut self, key: &'static str, v: Val) -> Self {
        self.fields.push((key, v));
        self
    }

    pub fn pane(self, id: PaneId) -> Self {
        self.with("pane", Val::Pane(id))
    }
}

/// A live Lua state with the `proteus` module.
pub struct Script {
    lua: Lua,
    reg: Rc<RefCell<Registry>>,
    deadline: Rc<Cell<Option<Instant>>>,
    /// Prelude function making pane objects from ids.
    pane_ctor: Function,
    messages: RefCell<Vec<Message>>,
    /// Time spent in host calls during the current callback (not counted
    /// against its budget: starting a shell takes what it takes).
    host_time: Cell<Duration>,
}

fn runtime(msg: impl Into<String>) -> mlua::Error {
    mlua::Error::runtime(msg.into())
}

impl Script {
    /// A fresh state; `require` resolves inside `dir`.
    pub fn new(dir: Option<&Path>) -> Result<Script, String> {
        let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).map_err(|e| e.to_string())?;
        let reg = Rc::new(RefCell::new(Registry::default()));
        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let d = deadline.clone();
        lua.set_global_hook(HookTriggers::new().every_nth_instruction(10_000), move |_, _| {
            match d.get() {
                Some(t) if Instant::now() > t => {
                    // Once: an error handler (pcall) gets to finish.
                    d.set(Some(Instant::now() + Duration::from_millis(100)));
                    Err(runtime("took too long and was stopped"))
                }
                _ => Ok(VmState::Continue),
            }
        })
        .map_err(|e| e.to_string())?;
        let setup = || -> mlua::Result<Function> {
            let p = lua.create_table()?;
            let r = reg.clone();
            p.set(
                "on",
                lua.create_function(move |_, (name, f): (String, Function)| {
                    let ev = EVENTS.iter().find(|e| **e == name).ok_or_else(|| {
                        runtime(format!("p.on: unknown event `{name}` (one of: {})", EVENTS.join(", ")))
                    })?;
                    let mut r = r.borrow_mut();
                    let id = r.add(f)?;
                    r.hooks.push((ev, id));
                    Ok(())
                })?,
            )?;
            let status = lua.create_table()?;
            let r = reg.clone();
            status.set(
                "segment",
                lua.create_function(move |_, (side, f): (String, Function)| {
                    let side = match side.as_str() {
                        "left" => Side::Left,
                        "right" => Side::Right,
                        _ => return Err(runtime("p.status.segment: side must be \"left\" or \"right\"")),
                    };
                    let mut r = r.borrow_mut();
                    let id = r.add(f)?;
                    r.segments.push((side, id));
                    Ok(())
                })?,
            )?;
            p.set("status", status)?;
            let palette = lua.create_table()?;
            let r = reg.clone();
            palette.set(
                "add",
                lua.create_function(move |_, (name, f): (String, Function)| {
                    if name.trim().is_empty() {
                        return Err(runtime("p.palette.add: the name is empty"));
                    }
                    let mut r = r.borrow_mut();
                    let id = r.add(f)?;
                    r.palette.retain(|e| e.0 != name);
                    r.palette.push((name, id));
                    Ok(())
                })?,
            )?;
            p.set("palette", palette)?;
            p.set("version", env!("CARGO_PKG_VERSION"))?;
            let call =
                lua.create_function(|lua, args: MultiValue| match lua.named_registry_value::<Value>(HOST)? {
                    Value::Function(h) => h.call::<MultiValue>(args),
                    _ => Err(runtime("only available in callbacks, not while the config loads")),
                })?;
            let r = reg.clone();
            let start = lua.create_function(move |_, (secs, f, repeat): (f64, Function, bool)| {
                if !secs.is_finite() || secs < 0.0 {
                    return Err(runtime("timer: expected a number of seconds"));
                }
                let after = Duration::from_secs_f64(secs.max(MIN_TIMER));
                let mut r = r.borrow_mut();
                let fid = r.add(f)?;
                r.next_timer += 1;
                let id = r.next_timer;
                r.timers.insert(id, (fid, after, repeat));
                r.new_timers.push(TimerSpec { id, after, repeat });
                Ok(id)
            })?;
            let r = reg.clone();
            let stop = lua.create_function(move |_, id: u32| {
                r.borrow_mut().timers.remove(&id);
                Ok(())
            })?;
            let ctor: Function = lua.load(PRELUDE).set_name("=proteus").call((p.clone(), call, start, stop))?;
            let package: Table = lua.globals().get("package")?;
            let loaded: Table = package.get("loaded")?;
            loaded.set("proteus", p)?;
            match dir.and_then(Path::to_str) {
                Some(dir) => package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?,
                None => package.set("path", "")?,
            }
            package.set("cpath", "")?;
            Ok(ctor)
        };
        let pane_ctor = setup().map_err(|e| clean(&e))?;
        Ok(Script {
            lua,
            reg,
            deadline,
            pane_ctor,
            messages: RefCell::new(Vec::new()),
            host_time: Cell::new(Duration::ZERO),
        })
    }

    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// The `proteus` module table.
    pub fn module(&self) -> mlua::Result<Table> {
        let package: Table = self.lua.globals().get("package")?;
        let loaded: Table = package.get("loaded")?;
        loaded.get("proteus")
    }

    pub fn registrar(&self) -> Registrar {
        Registrar(self.reg.clone())
    }

    /// Run the config source (`path` names the chunk).
    pub fn exec(&self, src: &str, path: &Path) -> Result<(), String> {
        self.deadline.set(Some(Instant::now() + LOAD_LIMIT));
        let r = self.lua.load(src).set_name(format!("@{}", path.display())).exec();
        self.deadline.set(None);
        r.map_err(|e| clean(&e))
    }

    pub fn has_hooks(&self, event: &str) -> bool {
        self.reg.borrow().hooks.iter().any(|h| h.0 == event)
    }

    pub fn has_segments(&self) -> bool {
        !self.reg.borrow().segments.is_empty()
    }

    /// Palette entries added with `p.palette.add`.
    pub fn palette(&self) -> Vec<(String, FnId)> {
        self.reg.borrow().palette.clone()
    }

    pub fn take_new_timers(&self) -> Vec<TimerSpec> {
        std::mem::take(&mut self.reg.borrow_mut().new_timers)
    }

    pub fn take_messages(&self) -> Vec<Message> {
        std::mem::take(&mut self.messages.borrow_mut())
    }

    /// Run a bound or palette function with `ctx = { pane = <pane> }`.
    pub fn call_fn(&self, host: &mut dyn Host, f: FnId, pane: Option<PaneId>) {
        let ctx = self.ctx(pane);
        self.invoke(host, f, ctx);
    }

    /// Run the hooks of an event.
    pub fn emit(&self, host: &mut dyn Host, ev: &ScriptEvent) {
        let hooks: Vec<FnId> = self.reg.borrow().hooks.iter().filter(|h| h.0 == ev.name).map(|h| h.1).collect();
        if hooks.is_empty() {
            return;
        }
        let table = match self.event_table(ev) {
            Ok(t) => t,
            Err(e) => return self.message(format!("Lua: {}", clean(&e)), true),
        };
        for f in hooks {
            self.invoke(host, f, Value::Table(table.clone()));
        }
    }

    /// A timer went off. Returns false once it is done (one-shot, stopped).
    pub fn timer_fired(&self, host: &mut dyn Host, id: u32) -> bool {
        let Some((f, _, repeat)) = self.reg.borrow().timers.get(&id).copied() else { return false };
        if !repeat {
            self.reg.borrow_mut().timers.remove(&id);
        }
        self.invoke(host, f, Value::Nil);
        repeat && self.reg.borrow().timers.contains_key(&id)
    }

    /// Text of the left and right status segments for a window whose
    /// focused pane is `pane`. Segments returning nil or "" are left out.
    pub fn segments(&self, host: &mut dyn Host, pane: Option<PaneId>) -> (Vec<String>, Vec<String>) {
        let segs = self.reg.borrow().segments.clone();
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for (side, f) in segs {
            let ctx = self.ctx(pane);
            let text = match self.invoke(host, f, ctx) {
                Some(Value::String(s)) => s.to_string_lossy(),
                Some(Value::Integer(i)) => i.to_string(),
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::Nil) => continue,
                Some(_) => {
                    self.message("Lua: a status segment returned neither a string nor nil".into(), true);
                    "?".into()
                }
                None => "⚠".into(),
            };
            // One line only.
            let text = text.lines().next().unwrap_or("").to_string();
            if text.is_empty() {
                continue;
            }
            match side {
                Side::Left => left.push(text),
                Side::Right => right.push(text),
            }
        }
        (left, right)
    }

    fn ctx(&self, pane: Option<PaneId>) -> Value {
        let make = || -> mlua::Result<Value> {
            let t = self.lua.create_table()?;
            t.set("pane", self.pane_ctor.call::<Value>(pane)?)?;
            Ok(Value::Table(t))
        };
        make().unwrap_or(Value::Nil)
    }

    fn event_table(&self, ev: &ScriptEvent) -> mlua::Result<Table> {
        let t = self.lua.create_table()?;
        t.set("name", ev.name)?;
        for (k, v) in &ev.fields {
            let v = match v {
                Val::Nil => Value::Nil,
                Val::Int(i) => Value::Integer(*i),
                Val::Num(n) => Value::Number(*n),
                Val::Str(s) => Value::String(self.lua.create_string(s)?),
                Val::Pane(id) => self.pane_ctor.call::<Value>(*id)?,
            };
            t.set(*k, v)?;
        }
        Ok(t)
    }

    fn message(&self, text: String, error: bool) {
        self.messages.borrow_mut().push(Message { text, error });
    }

    /// Call function `f` with the host reachable; errors and slowness
    /// become messages. `None` if it failed.
    fn invoke(&self, host: &mut dyn Host, f: FnId, arg: Value) -> Option<Value> {
        let func = self.reg.borrow().functions.get(f as usize).cloned()?;
        let start = Instant::now();
        self.host_time.set(Duration::ZERO);
        let r = self.with_host(host, |_| func.call::<Value>(arg));
        let took = start.elapsed().saturating_sub(self.host_time.get());
        if took > SLOW && r.is_ok() && self.reg.borrow_mut().slow.insert(f) {
            self.message(
                format!(
                    "slow Lua callback ({}): {} ms; callbacks should take under {} ms",
                    describe(&func),
                    took.as_millis(),
                    SLOW.as_millis()
                ),
                false,
            );
        }
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.message(format!("Lua: {}", clean(&e)), true);
                None
            }
        }
    }

    fn with_host<R>(&self, host: &mut dyn Host, f: impl FnOnce(&Lua) -> mlua::Result<R>) -> mlua::Result<R> {
        let lua = &self.lua;
        lua.scope(|scope| {
            let h = scope.create_function_mut(|lua, (op, args): (String, MultiValue)| {
                let start = Instant::now();
                let r = host_op(host, lua, &op, args);
                self.host_time.set(self.host_time.get() + start.elapsed());
                r
            })?;
            lua.set_named_registry_value(HOST, h)?;
            self.deadline.set(Some(Instant::now() + RUNAWAY));
            let r = f(lua);
            self.deadline.set(None);
            lua.set_named_registry_value(HOST, Value::Nil)?;
            r
        })
    }
}

/// `init.lua:12` for a Lua function.
fn describe(f: &Function) -> String {
    let i = f.info();
    let src = i.short_src.unwrap_or_else(|| "?".into());
    let src = src.rsplit('/').next().unwrap_or(&src).to_string();
    match i.line_defined {
        Some(l) => format!("{src}:{l}"),
        None => src,
    }
}

/// First line of a Lua error, without mlua's traceback noise.
pub fn clean(e: &mlua::Error) -> String {
    let s = match e {
        mlua::Error::CallbackError { cause, .. } => return clean(cause),
        mlua::Error::ExternalError(e) => e.to_string(),
        e => e.to_string(),
    };
    s.lines().next().unwrap_or("").trim_start_matches("runtime error: ").to_string()
}

fn spawn_opts(t: Option<Table>, what: &str, split: bool) -> mlua::Result<(Spawn, Option<Axis>)> {
    let mut s = Spawn::default();
    let mut axis = None;
    let Some(t) = t else { return Ok((s, axis)) };
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair?;
        match (k.as_str(), v) {
            ("cmd", Value::String(c)) => s.cmd = Some(Cmd::Shell(c.to_str()?.to_string())),
            ("cmd", Value::Table(list)) => {
                let argv = list
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()
                    .map_err(|_| runtime(format!("{what}: cmd: expected a string or a list of strings")))?;
                if argv.is_empty() {
                    return Err(runtime(format!("{what}: cmd is empty")));
                }
                s.cmd = Some(Cmd::Argv(argv));
            }
            ("cwd", Value::String(c)) => {
                let c = c.to_str()?.to_string();
                s.cwd = Some(match c.strip_prefix("~/").or(if c == "~" { Some("") } else { None }) {
                    Some(rest) => {
                        let home = std::env::var("HOME").unwrap_or_default();
                        if rest.is_empty() { home } else { format!("{home}/{rest}") }
                    }
                    None => c,
                });
            }
            ("dir", Value::String(d)) if split => {
                axis = Some(match d.to_str()?.as_ref() {
                    "right" => Axis::X,
                    "down" => Axis::Y,
                    _ => return Err(runtime(format!("{what}: dir must be \"right\" or \"down\""))),
                });
            }
            (k, _) => return Err(runtime(format!("{what}: unknown or invalid option `{k}`"))),
        }
    }
    Ok((s, axis))
}

/// One call from Lua into the terminal (`call(op, ...)` in the prelude).
fn host_op(host: &mut dyn Host, lua: &Lua, op: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    let one = |v: Value| Ok(MultiValue::from_iter([v]));
    let none = || Ok(MultiValue::new());
    let id_or_nil = |r: Result<PaneId, String>| match r {
        Ok(id) => one(Value::Integer(id as i64)),
        Err(e) => Err(runtime(e)),
    };
    match op {
        "pane_ids" => {
            let t = lua.create_sequence_from(host.pane_ids().into_iter().map(|i| i as i64))?;
            one(Value::Table(t))
        }
        "focused" => one(host.focused().map_or(Value::Nil, |i| Value::Integer(i as i64))),
        "pane_info" => {
            let id: Option<i64> = lua.unpack_multi(args)?;
            let Some(info) = id.and_then(|id| host.pane_info(id as PaneId)) else { return one(Value::Nil) };
            let t = lua.create_table()?;
            t.set("id", info.id as i64)?;
            t.set("title", info.title)?;
            t.set("name", info.name)?;
            t.set("cwd", info.cwd)?;
            t.set("process", info.process)?;
            t.set("command", info.command)?;
            t.set("window", info.window)?;
            t.set("tab", info.tab)?;
            t.set("focused", info.focused)?;
            t.set("floating", info.floating)?;
            t.set("cols", info.cols)?;
            t.set("rows", info.rows)?;
            one(Value::Table(t))
        }
        "split" => {
            let (id, opts): (i64, Option<Table>) = lua.unpack_multi(args)?;
            let (spawn, axis) = spawn_opts(opts, "pane:split", true)?;
            id_or_nil(host.split(id as PaneId, axis, spawn))
        }
        "new_tab" | "float" => {
            let opts: Option<Table> = lua.unpack_multi(args)?;
            let what = if op == "float" { "p.panes.float" } else { "p.panes.new_tab" };
            let (spawn, _) = spawn_opts(opts, what, false)?;
            id_or_nil(if op == "float" { host.float(spawn) } else { host.new_tab(spawn) })
        }
        "send_text" => {
            let (id, text, paste): (i64, String, bool) = lua.unpack_multi(args)?;
            host.send_text(id as PaneId, &text, paste);
            none()
        }
        "close" => {
            let id: i64 = lua.unpack_multi(args)?;
            host.close(id as PaneId);
            none()
        }
        "focus" => {
            let id: i64 = lua.unpack_multi(args)?;
            host.focus(id as PaneId);
            none()
        }
        "set_name" => {
            let (id, name): (i64, Option<String>) = lua.unpack_multi(args)?;
            host.set_name(id as PaneId, name.filter(|n| !n.is_empty()));
            none()
        }
        "read_screen" => {
            let (id, n): (i64, Option<i64>) = lua.unpack_multi(args)?;
            let n = n.unwrap_or(0).clamp(0, 1_000_000) as usize;
            one(match host.read_screen(id as PaneId, n) {
                Some(s) => Value::String(lua.create_string(s)?),
                None => Value::Nil,
            })
        }
        "read_blocks" => {
            let id: i64 = lua.unpack_multi(args)?;
            let list = lua.create_table()?;
            for (i, b) in host.read_blocks(id as PaneId).into_iter().enumerate() {
                let t = lua.create_table()?;
                t.set("id", b.id as i64)?;
                t.set("command", b.command)?;
                t.set("cwd", b.cwd)?;
                t.set("exit_code", b.exit_code)?;
                t.set("duration", b.duration)?;
                t.set("state", b.state)?;
                list.set(i + 1, t)?;
            }
            one(Value::Table(list))
        }
        "block_text" => {
            let (id, block, part): (i64, i64, String) = lua.unpack_multi(args)?;
            let part = match part.as_str() {
                "command" => BlockPart::Command,
                "output" => BlockPart::Output,
                "all" => BlockPart::All,
                _ => return Err(runtime("pane:block_text: part must be \"command\", \"output\" or \"all\"")),
            };
            one(match host.block_text(id as PaneId, block as u64, part) {
                Some(s) => Value::String(lua.create_string(s)?),
                None => Value::Nil,
            })
        }
        "run_action" => {
            let name: String = lua.unpack_multi(args)?;
            let a = Action::from_name(&name).ok_or_else(|| {
                runtime(format!("p.run_action: unknown action `{name}` (see `proteus --list-actions`)"))
            })?;
            host.run_action(a);
            none()
        }
        "notify" => {
            let (text, opts): (String, Option<Table>) = lua.unpack_multi(args)?;
            let (mut error, mut timeout) = (false, None);
            if let Some(o) = opts {
                for pair in o.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    match (k.as_str(), v) {
                        ("level", Value::String(l)) => {
                            error = match l.to_str()?.as_ref() {
                                "info" => false,
                                "error" => true,
                                _ => return Err(runtime("p.notify: level must be \"info\" or \"error\"")),
                            }
                        }
                        ("timeout", Value::Integer(n)) => timeout = Some(Duration::from_secs(n.clamp(0, 3600) as u64)),
                        ("timeout", Value::Number(n)) if n.is_finite() => {
                            timeout = Some(Duration::from_secs_f64(n.clamp(0.0, 3600.0)))
                        }
                        (k, _) => return Err(runtime(format!("p.notify: unknown or invalid option `{k}`"))),
                    }
                }
            }
            host.notify(text, error, timeout);
            none()
        }
        _ => Err(runtime(format!("unknown host call `{op}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what scripts do; panes 1..=n exist.
    #[derive(Default)]
    struct Mock {
        panes: Vec<PaneId>,
        focused: Option<PaneId>,
        log: Vec<String>,
        next: PaneId,
    }

    impl Host for Mock {
        fn pane_ids(&self) -> Vec<PaneId> {
            self.panes.clone()
        }
        fn focused(&self) -> Option<PaneId> {
            self.focused
        }
        fn pane_info(&self, id: PaneId) -> Option<PaneInfo> {
            self.panes.contains(&id).then(|| PaneInfo {
                id,
                title: format!("t{id}"),
                cwd: Some("/tmp".into()),
                focused: self.focused == Some(id),
                tab: 1,
                window: 1,
                ..PaneInfo::default()
            })
        }
        fn split(&mut self, id: PaneId, axis: Option<Axis>, spawn: Spawn) -> Result<PaneId, String> {
            if !self.panes.contains(&id) {
                return Err(format!("no pane {id}"));
            }
            self.next += 1;
            let new = 100 + self.next;
            self.panes.push(new);
            self.log.push(format!("split {id} {axis:?} {:?} {:?}", spawn.cmd, spawn.cwd));
            Ok(new)
        }
        fn new_tab(&mut self, spawn: Spawn) -> Result<PaneId, String> {
            self.log.push(format!("tab {:?}", spawn.cmd));
            Ok(50)
        }
        fn float(&mut self, _: Spawn) -> Result<PaneId, String> {
            Err("cannot start shell".into())
        }
        fn close(&mut self, id: PaneId) {
            self.log.push(format!("close {id}"));
        }
        fn focus(&mut self, id: PaneId) {
            self.log.push(format!("focus {id}"));
        }
        fn send_text(&mut self, id: PaneId, text: &str, paste: bool) {
            self.log.push(format!("send {id} {text:?} {paste}"));
        }
        fn set_name(&mut self, id: PaneId, name: Option<String>) {
            self.log.push(format!("name {id} {name:?}"));
        }
        fn read_screen(&self, id: PaneId, n: usize) -> Option<String> {
            Some(format!("screen {id} {n}"))
        }
        fn read_blocks(&self, _: PaneId) -> Vec<BlockInfo> {
            vec![BlockInfo {
                id: 7,
                command: Some("make".into()),
                exit_code: Some(2),
                state: "finished",
                ..Default::default()
            }]
        }
        fn block_text(&self, _: PaneId, block: u64, part: BlockPart) -> Option<String> {
            Some(format!("{block} {part:?}"))
        }
        fn run_action(&mut self, a: Action) {
            self.log.push(format!("action {}", a.name()));
        }
        fn notify(&mut self, text: String, error: bool, timeout: Option<Duration>) {
            self.log.push(format!("notify {text} {error} {timeout:?}"));
        }
    }

    fn script(src: &str) -> Script {
        let s = Script::new(None).unwrap();
        s.exec(src, Path::new("/x/init.lua")).unwrap();
        s
    }

    fn mock() -> Mock {
        Mock { panes: vec![1, 2], focused: Some(1), ..Mock::default() }
    }

    #[test]
    fn hooks_reach_the_host() {
        let s = script(
            r#"
            local p = require("proteus")
            p.on("command_finished", function(ev)
              if ev.exit_code ~= 0 then
                p.notify("failed: " .. ev.command .. " in " .. ev.pane.cwd, { level = "error", timeout = 3 })
              end
              local new = ev.pane:split { dir = "right", cmd = { "htop", "-d", "5" }, cwd = "/srv" }
              new:send_text("hi\n")
              ev.pane:insert_text("x")
              new:set_name("mon")
              p.notify(ev.pane:read_screen { scrollback = 3 })
              local b = ev.pane:read_blocks()[1]
              p.notify(b.command .. " " .. b.exit_code .. " " .. b.state .. " " .. ev.pane:block_text(b.id))
              p.run_action("pane.zoom")
              for _, q in ipairs(p.panes.list()) do q:focus() end
              p.notify(tostring(p.panes.focused() == ev.pane) .. " " .. tostring(p.panes.get(9)))
            end)
        "#,
        );
        assert!(s.has_hooks("command_finished") && !s.has_hooks("bell"));
        let mut h = mock();
        let ev = ScriptEvent::new("command_finished")
            .pane(1)
            .with("command", Val::Str("make".into()))
            .with("exit_code", Val::Int(2));
        s.emit(&mut h, &ev);
        assert_eq!(s.take_messages(), []);
        assert_eq!(
            h.log,
            [
                "notify failed: make in /tmp true Some(3s)",
                r#"split 1 Some(X) Some(Argv(["htop", "-d", "5"])) Some("/srv")"#,
                r#"send 101 "hi\n" false"#,
                r#"send 1 "x" true"#,
                r#"name 101 Some("mon")"#,
                "notify screen 1 3 false None",
                "notify make 2 finished 7 Output false None",
                "action pane.zoom",
                "focus 1",
                "focus 2",
                "focus 101",
                "notify true nil false None",
            ]
        );
    }

    #[test]
    fn errors_become_messages() {
        let s = script(
            r#"
            local p = require("proteus")
            p.on("bell", function(ev) error("boom") end)
            p.on("bell", function(ev) p.panes.float {} end)
            p.on("bell", function(ev) ev.pane:split { size = 3 } end)
            p.on("bell", function(ev) p.run_action("nope") end)
            p.on("bell", function(ev) ev.pane.title = "x" end)
        "#,
        );
        let mut h = mock();
        s.emit(&mut h, &ScriptEvent::new("bell").pane(1));
        let m: Vec<String> = s.take_messages().into_iter().map(|m| m.text).collect();
        assert_eq!(m.len(), 5, "{m:?}");
        assert!(m[0].contains("init.lua:3: boom"), "{}", m[0]);
        assert!(m[1].contains("cannot start shell"), "{}", m[1]);
        assert!(m[2].contains("unknown or invalid option `size`"), "{}", m[2]);
        assert!(m[3].contains("unknown action `nope`"), "{}", m[3]);
        assert!(m[4].contains("read-only"), "{}", m[4]);
        // Registration errors fail the config.
        let s = Script::new(None).unwrap();
        let e = s.exec(r#"require("proteus").on("blah", print)"#, Path::new("/x/init.lua")).unwrap_err();
        assert!(e.contains("unknown event `blah`"), "{e}");
        // Pane calls need a callback.
        let e = s.exec(r#"require("proteus").panes.list()"#, Path::new("/x/init.lua")).unwrap_err();
        assert!(e.contains("only available in callbacks"), "{e}");
    }

    #[test]
    fn segments_palette_timers() {
        let s = script(
            r#"
            local p = require("proteus")
            p.status.segment("right", function(ctx) return ctx.pane.title end)
            p.status.segment("right", function() return nil end)
            p.status.segment("left", function() return 42 end)
            p.status.segment("left", function() error("bad") end)
            p.palette.add("Say hi", function(ctx) ctx.pane:send_text("hi") end)
            n = 0
            local t
            t = p.every(0.5, function() n = n + 1; if n == 2 then t:stop() end end)
            p.after(0, function() p.notify("once") end)
        "#,
        );
        let mut h = mock();
        let (l, r) = s.segments(&mut h, Some(2));
        assert_eq!((l, r), (vec!["42".to_string(), "⚠".into()], vec!["t2".to_string()]));
        assert!(s.take_messages()[0].text.contains("bad"));
        let pal = s.palette();
        assert_eq!(pal.len(), 1);
        s.call_fn(&mut h, pal[0].1, Some(2));
        assert_eq!(h.log, [r#"send 2 "hi" false"#]);
        let timers = s.take_new_timers();
        assert_eq!(timers.len(), 2);
        assert_eq!((timers[0].after, timers[0].repeat), (Duration::from_millis(500), true));
        assert_eq!((timers[1].after, timers[1].repeat), (Duration::from_millis(10), false));
        assert!(s.timer_fired(&mut h, timers[0].id));
        assert!(!s.timer_fired(&mut h, timers[0].id)); // stopped itself
        assert!(!s.timer_fired(&mut h, timers[0].id));
        assert!(!s.timer_fired(&mut h, timers[1].id));
        assert_eq!(h.log.last().unwrap(), "notify once false None");
        assert!(!s.timer_fired(&mut h, timers[1].id));
    }

    #[test]
    fn type_stubs_are_lua() {
        let stubs = Lua::new();
        let p: Table = stubs.load(TYPES).set_name("=proteus.lua").eval().unwrap();
        // Every function of the real module is declared.
        let s = Script::new(None).unwrap();
        let real = s.module().unwrap();
        for pair in real.pairs::<String, Value>() {
            let (k, v) = pair.unwrap();
            if k.starts_with('_') {
                continue;
            }
            let stub: Value = p.get(k.as_str()).unwrap();
            assert!(!stub.is_nil() || k == "version", "`{k}` missing from proteus.lua");
            if let (Value::Table(t), Value::Table(st)) = (v, stub) {
                for f in t.pairs::<String, Value>() {
                    let f = f.unwrap().0;
                    assert!(!st.get::<Value>(f.as_str()).unwrap().is_nil(), "`{k}.{f}` missing from proteus.lua");
                }
            }
        }
    }

    #[test]
    fn runaway_and_slow_callbacks() {
        let s = script(
            r#"
            local p = require("proteus")
            p.on("bell", function() while true do end end)
            p.on("pane_opened", function() local t = os.clock() while os.clock() - t < 0.02 do end end)
        "#,
        );
        let mut h = mock();
        let start = Instant::now();
        s.emit(&mut h, &ScriptEvent::new("bell"));
        assert!(start.elapsed() < RUNAWAY + Duration::from_secs(1));
        let m = s.take_messages();
        assert!(m[0].error && m[0].text.contains("took too long"), "{m:?}");
        s.emit(&mut h, &ScriptEvent::new("pane_opened"));
        s.emit(&mut h, &ScriptEvent::new("pane_opened"));
        let m = s.take_messages();
        assert_eq!(m.len(), 1, "warned once: {m:?}");
        assert!(!m[0].error && m[0].text.contains("slow Lua callback (init.lua:4)"), "{m:?}");
        // The config itself is limited too.
        let s = Script::new(None).unwrap();
        let e = s.exec("while true do end", Path::new("/x/init.lua")).unwrap_err();
        assert!(e.contains("took too long"), "{e}");
    }
}
