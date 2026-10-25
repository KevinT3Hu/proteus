//! Running plugins: each instance in its own thread with its own store,
//! memory limit and time limit. Calls into the terminal are checked
//! against the granted permissions here and then sent to the host (the
//! GUI) as [`Call`]s; calls with a result wait for its [`Answer`]. What
//! needs no terminal (storage, processes, HTTP, the keyring, files) is
//! done here, in the plugin's thread or helper threads; results that come
//! later arrive as events.
//!
//! Time spent in the host (waiting for the GUI, a program, the network or
//! a keyring prompt) does not count towards a call's time limit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::limits::Limits;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, UpdateDeadline};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::files::{self, Access, Watcher};
use crate::manifest::Permissions;
use crate::net::{self, Streams};
use crate::process::{self, Processes};
use crate::secrets::{self, Keyring};
use crate::storage::Storage;
use types::DirEntry;

wasmtime::component::bindgen!({
    path: "../proteus-plugin/wit",
    world: "plugin",
});

pub use proteus::plugin::types;
use types::*;

/// Guest allocation budget (all linear memories and table slots) per instance.
pub const MEMORY: usize = 256 << 20;
/// Longest a single call into a plugin may run.
pub const CALL_LIMIT: Duration = Duration::from_secs(5);
/// Epoch tick: how quickly runaway plugins and stop requests are noticed.
const TICK: Duration = Duration::from_millis(10);
/// Timers of one plugin.
const MAX_TIMERS: usize = 64;
const MIN_TIMER: Duration = Duration::from_millis(10);
/// Decoded image pixels one plugin may hold.
pub const MAX_IMAGE_BYTES: usize = 64 << 20;
const MAX_TASKS: usize = 16;
/// Output (processes, HTTP bodies) queued for a plugin before the
/// producers wait for it.
const MAX_BACKLOG: usize = 1 << 20;

/// One running instance of a plugin (`instance` counts restarts and
/// reloads, so messages of an old instance can be told apart).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub id: String,
    pub instance: u64,
}

/// A decoded image (RGBA, 8 bits per channel).
#[derive(Debug)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A call from a plugin into the terminal (already permitted).
#[derive(Debug, Clone)]
pub enum Call {
    Locale,
    Log(String),
    Notify {
        text: String,
        error: bool,
    },
    ListPanes,
    GetPane(PaneId),
    /// Answered with `Reply::Remote`.
    PaneRemote(PaneId),
    FocusedPane,
    FocusPane(PaneId),
    SendText {
        pane: PaneId,
        text: String,
        paste: bool,
    },
    ReadScreen {
        pane: PaneId,
        scrollback: u32,
    },
    ReadBlocks(PaneId),
    OpenPane(Placement, Spawn),
    OpenSurface(SurfaceOptions),
    Draw(SurfaceId, Frame),
    CloseSurface(SurfaceId),
    FocusSurface(SurfaceId),
    SurfaceSize(SurfaceId),
    SetSurfaceTitle(SurfaceId, String),
    StartDrag(SurfaceId, Vec<String>),
    SetStatus(StatusSide, String),
    /// Stable local ID, display label and optional default key.
    RegisterAction {
        id: String,
        name: String,
        key: Option<String>,
    },
    LoadImage(ImageId, Arc<Pixels>),
    DropImage(ImageId),
    /// Answer with `Event::Answered(id, ..)`.
    Prompt {
        id: RequestId,
        question: String,
        initial: String,
    },
    Confirm {
        id: RequestId,
        question: String,
    },
    TaskStart {
        id: TaskId,
        title: String,
    },
    TaskUpdate {
        id: TaskId,
        progress: Option<f32>,
        message: String,
    },
    TaskEnd(TaskId),
    /// Answered with `Reply::Text`, possibly later (the clipboard is read
    /// from another program).
    ClipboardRead,
    ClipboardWrite(String),
    /// Open a (checked) path or URL with the desktop's default application.
    Open(String, Option<Arc<std::fs::File>>),
}

#[derive(Debug, Clone)]
pub enum Reply {
    Unit,
    Panes(Vec<PaneInfo>),
    Pane(Option<PaneInfo>),
    Id(Option<PaneId>),
    Text(String),
    Remote(Option<String>),
    Blocks(Vec<BlockInfo>),
    Surface(SurfaceId),
    Size(u32, u32),
}

pub type Answer = Result<Reply, String>;

/// Where a call's answer goes.
pub type Responder = mpsc::Sender<Answer>;

/// What plugins tell the host.
pub enum FromPlugin {
    Call(Call, Option<Responder>),
    /// `init` returned fine.
    Started,
    /// Trapped, ran too long, ran out of memory, or `init` failed. The
    /// instance is gone.
    Failed(String),
    /// Stopped as asked.
    Exited,
}

/// Receives everything plugins send; called from their threads.
pub type Sink = Arc<dyn Fn(Key, FromPlugin) + Send + Sync>;

pub(crate) enum ToPlugin {
    Event(Event),
    Ui(SurfaceId, UiEvent),
}

/// Sends events to a plugin from helper threads (processes, HTTP, file
/// watches).
#[derive(Clone)]
pub struct Poster {
    tx: mpsc::SyncSender<ToPlugin>,
    backlog: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Poster {
    /// False once the plugin is gone.
    pub fn event(&self, e: Event) -> bool {
        if self.stop.load(Ordering::Relaxed) {
            return false;
        }
        if self.tx.try_send(ToPlugin::Event(e)).is_err() {
            self.stop.store(true, Ordering::Release);
            return false;
        }
        true
    }

    /// An event carrying `n` bytes of output: waits while the plugin is
    /// behind.
    pub fn data(&self, n: usize, e: Event) -> bool {
        self.data_until(n, e, &AtomicBool::new(false))
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub(crate) fn data_until(&self, n: usize, e: Event, cancel: &AtomicBool) -> bool {
        while self.backlog.load(Ordering::Relaxed) > MAX_BACKLOG {
            if self.stopped() || cancel.load(Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        self.backlog.fetch_add(n, Ordering::Relaxed);
        self.event(e)
    }

    #[cfg(test)]
    pub(crate) fn testing() -> (Self, mpsc::Receiver<ToPlugin>) {
        let (tx, rx) = mpsc::sync_channel(256);
        (Self { tx, backlog: Arc::default(), stop: Arc::default() }, rx)
    }
}

/// What to run.
pub struct Spec {
    pub config: serde_json::Value,
    pub key: Key,
    pub wasm: Vec<u8>,
    /// Compiled components are cached by this.
    pub hash: String,
    pub granted: Permissions,
    /// The plugin's own directory, `/data` in the sandbox.
    pub data_dir: PathBuf,
    /// Its key-value store.
    pub storage: PathBuf,
    pub memory: usize,
    /// Pass the plugin's stderr through (debugging).
    pub stderr: bool,
    /// Reserved for API compatibility. Native disk caches are intentionally ignored.
    pub compiled: Option<PathBuf>,
}

/// Compiles and starts plugins. One per terminal process.
pub struct Runtime {
    engine: Engine,
    cache: Arc<Mutex<HashMap<String, Component>>>,
}

impl Runtime {
    pub fn new() -> Result<Runtime, String> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        config.wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|e| e.to_string())?;
        let weak = engine.weak();
        std::thread::Builder::new()
            .name("plugin-epoch".into())
            .spawn(move || {
                while let Some(e) = weak.upgrade() {
                    e.increment_epoch();
                    drop(e);
                    std::thread::sleep(TICK);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Runtime { engine, cache: Arc::default() })
    }

    /// Start an instance in a new thread. `init` runs there; the outcome
    /// arrives at `sink` as [`FromPlugin::Started`] or `Failed`.
    pub fn start(&self, spec: Spec, sink: Sink) -> Result<Handle, String> {
        let (tx, rx) = mpsc::sync_channel(256);
        let stop = Arc::new(AtomicBool::new(false));
        let key = spec.key.clone();
        let poster = Poster { tx: tx.clone(), backlog: Arc::default(), stop: stop.clone() };
        let (engine, cache) = (self.engine.clone(), self.cache.clone());
        std::thread::Builder::new()
            .name(format!("plugin {}", spec.key.id))
            .spawn(move || {
                let key = spec.key.clone();
                let msg = match run(&engine, &cache, spec, rx, &sink, poster) {
                    Ok(()) => FromPlugin::Exited,
                    Err(e) => FromPlugin::Failed(e),
                };
                sink(key, msg);
            })
            .map_err(|e| e.to_string())?;
        Ok(Handle { key, tx, stop })
    }
}

/// A running instance. Dropping it stops the instance too.
pub struct Handle {
    pub key: Key,
    tx: mpsc::SyncSender<ToPlugin>,
    stop: Arc<AtomicBool>,
}

impl Handle {
    pub fn event(&self, e: Event) {
        if self.tx.try_send(ToPlugin::Event(e)).is_err() {
            self.stop();
        }
    }

    pub fn ui(&self, s: SurfaceId, e: UiEvent) {
        if self.tx.try_send(ToPlugin::Ui(s, e)).is_err() {
            self.stop();
        }
    }

    /// Stop it, interrupting a call in progress within a tick.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct State {
    config: String,
    key: Key,
    granted: Permissions,
    sink: Sink,
    wasi: WasiCtx,
    table: ResourceTable,
    limits: Limits,
    /// When the current call into the plugin started (moved forward by the
    /// time spent in the host).
    call_start: Instant,
    stop: Arc<AtomicBool>,
    /// id → (due, repeat interval).
    timers: BTreeMap<TimerId, (Instant, Option<Duration>)>,
    next_timer: TimerId,
    post: Poster,
    access: Access,
    storage: Storage,
    processes: Processes,
    streams: Streams,
    watcher: Option<Watcher>,
    keyring: Option<Arc<Keyring>>,
    /// Image id → bytes of pixels.
    images: HashMap<ImageId, usize>,
    next_image: ImageId,
    tasks: HashSet<TaskId>,
    next_task: TaskId,
    /// Streams and questions.
    next_request: RequestId,
    rate_window: Instant,
    rate_calls: usize,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

fn denied(name: &str) -> String {
    format!("permission denied: {name}")
}

impl State {
    fn need(&self, ok: bool, name: &str) -> Result<(), String> {
        if ok { Ok(()) } else { Err(denied(name)) }
    }

    /// Run `f` without charging its time to the plugin.
    fn excluded<T>(&mut self, f: impl FnOnce(&mut State) -> T) -> T {
        let t = Instant::now();
        let r = f(self);
        self.call_start += t.elapsed();
        r
    }

    /// Run `f` in a helper thread and wait for it, giving up when the
    /// plugin is stopped. Not charged to the plugin.
    fn blocking<T: Send + 'static>(&mut self, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
        self.excluded(|s| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("plugin-call".into())
                .spawn(move || {
                    let _ = tx.send(f());
                })
                .map_err(|e| e.to_string())?;
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(v) => return Ok(v),
                    Err(RecvTimeoutError::Timeout) if !s.stop.load(Ordering::Relaxed) => {}
                    Err(_) => return Err("stopped".into()),
                }
            }
        })
    }

    /// Send one bounded call and wait for its consumption.
    fn rpc(&mut self, call: Call) -> Answer {
        if self.stop.load(Ordering::Acquire) {
            return Err("stopped".into());
        }
        if self.rate_window.elapsed() >= Duration::from_secs(1) {
            self.rate_window = Instant::now();
            self.rate_calls = 0;
        }
        self.rate_calls += 1;
        if self.rate_calls > 2000 || call_bytes(&call) > 4 << 20 {
            self.stop.store(true, Ordering::Release);
            return Err("plugin exceeded host call rate or message size limit".into());
        }
        // Every call is acknowledged after consumption, including drawing and
        // logging. A plugin can have only one host message in flight.
        let (tx, rx) = mpsc::channel();
        (self.sink)(self.key.clone(), FromPlugin::Call(call, Some(tx)));
        self.excluded(|s| {
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(answer) => break answer,
                    Err(RecvTimeoutError::Timeout) if !s.stop.load(Ordering::Relaxed) => {}
                    Err(RecvTimeoutError::Timeout) => break Err("stopped".into()),
                    Err(RecvTimeoutError::Disconnected) => break Err("the terminal is shutting down".into()),
                }
            }
        })
    }

    fn request_id(&mut self) -> RequestId {
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.next_request
    }

    /// The program and directory for `cmd`.
    fn command(&self, cmd: &Command) -> Result<(PathBuf, files::Pinned), String> {
        let name = cmd.argv.first().ok_or("the command is empty")?;
        let program = process::resolve(&self.granted.run_command, name)?;
        let cwd = match &cmd.cwd {
            Some(c) => self.access.pin(c, false, true)?,
            None => self.access.pin("/data", false, true)?,
        };
        Ok((program, cwd))
    }

    fn keyring(&mut self) -> Result<Arc<Keyring>, String> {
        if let Some(k) = &self.keyring {
            return Ok(k.clone());
        }
        let k = Arc::new(self.blocking(Keyring::open)??);
        self.keyring = Some(k.clone());
        Ok(k)
    }

    /// Run `f` with the keyring in a helper thread.
    fn with_keyring<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&Keyring, &str) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        self.need(self.granted.secrets, "secrets")?;
        let k = self.keyring()?;
        let id = self.key.id.clone();
        let r = self.blocking(move || f(&k, &id))?;
        if r.is_err() {
            // The connection may have gone (the keyring restarted).
            self.keyring = None;
        }
        r
    }
}

fn call_bytes(call: &Call) -> usize {
    match call {
        Call::Log(s)
        | Call::Notify { text: s, .. }
        | Call::SetStatus(_, s)
        | Call::ClipboardWrite(s)
        | Call::SendText { text: s, .. } => s.len(),
        Call::OpenPane(_, spawn) => {
            spawn.argv.iter().map(|s| std::mem::size_of::<String>() + s.len()).sum::<usize>()
                + spawn.cwd.as_ref().map_or(0, String::len)
        }
        Call::OpenSurface(o) => o.title.len(),
        Call::StartDrag(_, paths) => paths.iter().map(|s| std::mem::size_of::<String>() + s.len()).sum(),
        Call::Open(target, _) => target.len(),
        Call::LoadImage(..) => 0, // Separately bounded by MAX_IMAGE_BYTES.
        Call::Draw(_, f) => {
            f.lines.len().saturating_mul(std::mem::size_of::<Vec<Span>>())
                + f.lines
                    .iter()
                    .flatten()
                    .map(|s| std::mem::size_of::<Span>() + s.text.len() + s.link.as_ref().map_or(0, String::len))
                    .sum::<usize>()
                + f.images.len() * std::mem::size_of::<ImagePlace>()
                + f.accessibility
                    .iter()
                    .map(|n| {
                        std::mem::size_of_val(n)
                            + n.id.len()
                            + n.label.len()
                            + n.parent.as_ref().map_or(0, String::len)
                            + n.description.as_ref().map_or(0, String::len)
                            + n.value.as_ref().map_or(0, String::len)
                            + n.actions.len() * std::mem::size_of::<AccessibilityAction>()
                    })
                    .sum::<usize>()
        }
        _ => 0,
    }
}

fn bad_reply<T>() -> Result<T, String> {
    Err("internal error: unexpected reply".into())
}

/// Plugin text: at most `max` characters.
fn short(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn decode_image(data: &[u8]) -> Result<Pixels, String> {
    let mut r = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64 * 2);
    r.limits(limits);
    let img = r.decode().map_err(|e| format!("load-image: {e}"))?.into_rgba8();
    Ok(Pixels { width: img.width(), height: img.height(), rgba: img.into_raw() })
}

impl types::Host for State {}

impl proteus::plugin::host::Host for State {
    fn plugin_config(&mut self) -> Result<String, String> {
        Ok(self.config.clone())
    }

    fn locale(&mut self) -> Result<String, String> {
        match self.rpc(Call::Locale)? {
            Reply::Text(s) => Ok(s),
            _ => bad_reply(),
        }
    }

    fn log(&mut self, message: String) {
        let _ = self.rpc(Call::Log(short(&message, 4096)));
    }

    fn notify(&mut self, text: String, error: bool) {
        let _ = self.rpc(Call::Notify { text: short(&text, 1000), error });
    }

    fn list_panes(&mut self) -> Result<Vec<PaneInfo>, String> {
        self.need(self.granted.read_panes, "read-panes")?;
        match self.rpc(Call::ListPanes)? {
            Reply::Panes(p) => Ok(p),
            _ => bad_reply(),
        }
    }

    fn get_pane(&mut self, id: PaneId) -> Result<Option<PaneInfo>, String> {
        self.need(self.granted.read_panes, "read-panes")?;
        match self.rpc(Call::GetPane(id))? {
            Reply::Pane(p) => Ok(p),
            _ => bad_reply(),
        }
    }

    fn pane_remote(&mut self, id: PaneId) -> Result<Option<String>, String> {
        self.need(self.granted.read_panes, "read-panes")?;
        match self.rpc(Call::PaneRemote(id))? {
            Reply::Remote(r) => Ok(r),
            _ => bad_reply(),
        }
    }

    fn focused_pane(&mut self) -> Result<Option<PaneId>, String> {
        self.need(self.granted.read_panes, "read-panes")?;
        match self.rpc(Call::FocusedPane)? {
            Reply::Id(p) => Ok(p),
            _ => bad_reply(),
        }
    }

    fn focus_pane(&mut self, id: PaneId) -> Result<(), String> {
        self.need(self.granted.read_panes, "read-panes")?;
        self.rpc(Call::FocusPane(id)).map(|_| ())
    }

    fn send_text(&mut self, pane: PaneId, text: String, paste: bool) -> Result<(), String> {
        self.need(self.granted.write_pty, "write-pty")?;
        self.rpc(Call::SendText { pane, text, paste }).map(|_| ())
    }

    fn read_screen(&mut self, pane: PaneId, scrollback: u32) -> Result<String, String> {
        self.need(self.granted.read_screen, "read-screen")?;
        match self.rpc(Call::ReadScreen { pane, scrollback })? {
            Reply::Text(t) => Ok(t),
            _ => bad_reply(),
        }
    }

    fn read_blocks(&mut self, pane: PaneId) -> Result<Vec<BlockInfo>, String> {
        self.need(self.granted.read_screen, "read-screen")?;
        match self.rpc(Call::ReadBlocks(pane))? {
            Reply::Blocks(b) => Ok(b),
            _ => bad_reply(),
        }
    }

    fn open_pane(&mut self, at: Placement, what: Spawn) -> Result<PaneId, String> {
        self.need(self.granted.open_pane, "open-pane")?;
        match self.rpc(Call::OpenPane(at, what))? {
            Reply::Id(Some(p)) => Ok(p),
            _ => bad_reply(),
        }
    }

    fn start_drag(&mut self, surface: SurfaceId, paths: Vec<String>) -> Result<(), String> {
        self.need(self.granted.ui, "ui")?;
        if paths.len() > 10_000 || paths.iter().map(String::len).sum::<usize>() > 1 << 20 {
            return Err("start-drag: too many paths".into());
        }
        self.rpc(Call::StartDrag(surface, paths)).map(|_| ())
    }

    fn open_surface(&mut self, options: SurfaceOptions) -> Result<SurfaceId, String> {
        self.need(self.granted.ui, "ui")?;
        match self.rpc(Call::OpenSurface(options))? {
            Reply::Surface(s) => Ok(s),
            _ => bad_reply(),
        }
    }

    fn set_surface_title(&mut self, surface: SurfaceId, title: String) -> Result<(), String> {
        self.need(self.granted.ui, "ui")?;
        self.rpc(Call::SetSurfaceTitle(surface, short(&title, 100))).map(|_| ())
    }

    fn draw(&mut self, surface: SurfaceId, frame: Frame) -> Result<(), String> {
        crate::accessibility::validate(&frame.accessibility)?;
        self.need(self.granted.ui, "ui")?;
        if let Some(i) = frame.images.iter().find(|i| !self.images.contains_key(&i.image)) {
            return Err(format!("no image {}", i.image));
        }
        self.rpc(Call::Draw(surface, frame)).map(|_| ())
    }

    fn close_surface(&mut self, surface: SurfaceId) {
        if self.granted.ui {
            let _ = self.rpc(Call::CloseSurface(surface));
        }
    }

    fn focus_surface(&mut self, surface: SurfaceId) -> Result<(), String> {
        self.need(self.granted.ui, "ui")?;
        self.rpc(Call::FocusSurface(surface)).map(|_| ())
    }

    fn surface_size(&mut self, surface: SurfaceId) -> Result<(u32, u32), String> {
        self.need(self.granted.ui, "ui")?;
        match self.rpc(Call::SurfaceSize(surface))? {
            Reply::Size(c, r) => Ok((c, r)),
            _ => bad_reply(),
        }
    }

    fn set_status(&mut self, side: StatusSide, text: String) -> Result<(), String> {
        self.need(self.granted.ui, "ui")?;
        self.rpc(Call::SetStatus(side, short(&text, 4096))).map(|_| ())
    }

    fn register_action(&mut self, name: String, key: Option<String>) -> Result<(), String> {
        self.need(self.granted.ui, "ui")?;
        if name.trim().is_empty() || name.len() > 100 {
            return Err("register-action: the name must be 1 to 100 bytes".into());
        }
        self.rpc(Call::RegisterAction { id: name.clone(), name, key: key.map(|k| short(&k, 40)) }).map(|_| ())
    }

    fn register_action_id(&mut self, id: String, name: String, default_key: Option<String>) -> Result<(), String> {
        self.need(self.granted.ui, "ui")?;
        if !crate::manifest::valid_action_id(&id) {
            return Err(
                "register-action: invalid action ID (1–100 ASCII letters, digits, dots, underscores or hyphens)".into(),
            );
        }
        if name.trim().is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
            return Err("register-action: label must be 1–100 bytes without control characters".into());
        }
        if default_key.as_ref().is_some_and(|key| key.is_empty() || key.len() > 40 || key.chars().any(char::is_control))
        {
            return Err("register-action: invalid default key".into());
        }
        self.rpc(Call::RegisterAction { id, name, key: default_key }).map(|_| ())
    }

    fn load_image(&mut self, data: Vec<u8>) -> Result<ImageId, String> {
        self.need(self.granted.ui, "ui")?;
        let px = self.excluded(|_| decode_image(&data))?;
        let used: usize = self.images.values().sum();
        if used + px.rgba.len() > MAX_IMAGE_BYTES {
            return Err(format!("load-image: images take more than {} MiB; drop some", MAX_IMAGE_BYTES >> 20));
        }
        self.next_image = self.next_image.wrapping_add(1).max(1);
        let id = self.next_image;
        self.images.insert(id, px.rgba.len());
        self.rpc(Call::LoadImage(id, Arc::new(px))).map(|_| id)
    }

    fn drop_image(&mut self, id: ImageId) {
        if self.images.remove(&id).is_some() {
            let _ = self.rpc(Call::DropImage(id));
        }
    }

    fn prompt(&mut self, question: String, initial: String) -> Result<RequestId, String> {
        self.need(self.granted.ui, "ui")?;
        let id = self.request_id();
        let (question, initial) = (short(&question, 200), short(&initial, 4000));
        self.rpc(Call::Prompt { id, question, initial }).map(|_| id)
    }

    fn confirm(&mut self, question: String) -> Result<RequestId, String> {
        self.need(self.granted.ui, "ui")?;
        let id = self.request_id();
        self.rpc(Call::Confirm { id, question: short(&question, 200) }).map(|_| id)
    }

    fn task_start(&mut self, title: String) -> Result<TaskId, String> {
        self.need(self.granted.ui, "ui")?;
        if self.tasks.len() >= MAX_TASKS {
            return Err(format!("at most {MAX_TASKS} tasks at a time"));
        }
        self.next_task = self.next_task.wrapping_add(1).max(1);
        let id = self.next_task;
        self.tasks.insert(id);
        self.rpc(Call::TaskStart { id, title: short(&title, 100) }).map(|_| id)
    }

    fn task_update(&mut self, id: TaskId, progress: Option<f32>, message: String) {
        if self.tasks.contains(&id) {
            let progress = progress.filter(|p| p.is_finite()).map(|p| p.clamp(0.0, 1.0));
            let _ = self.rpc(Call::TaskUpdate { id, progress, message: short(&message, 100) });
        }
    }

    fn task_end(&mut self, id: TaskId) {
        if self.tasks.remove(&id) {
            let _ = self.rpc(Call::TaskEnd(id));
        }
    }

    fn storage_get(&mut self, key: String) -> Option<String> {
        match self.excluded(|s| s.storage.get(&key)) {
            Ok(value) => value,
            Err(e) => {
                let _ = self.rpc(Call::Notify { text: e, error: true });
                None
            }
        }
    }

    fn storage_set(&mut self, key: String, value: String) -> Result<(), String> {
        self.excluded(|s| s.storage.set(key, value))
    }

    fn storage_delete(&mut self, key: String) {
        if let Err(e) = self.excluded(|s| s.storage.delete(&key)) {
            let _ = self.rpc(Call::Notify { text: e, error: true });
        }
    }

    fn storage_keys(&mut self) -> Vec<String> {
        match self.excluded(|s| s.storage.keys()) {
            Ok(keys) => keys,
            Err(e) => {
                let _ = self.rpc(Call::Notify { text: e, error: true });
                Vec::new()
            }
        }
    }

    fn run(&mut self, cmd: Command, input: Vec<u8>, timeout_ms: u32) -> Result<Output, String> {
        let (program, cwd) = self.command(&cmd)?;
        let timeout = if timeout_ms == 0 { process::DEFAULT_TIMEOUT } else { Duration::from_millis(timeout_ms as u64) };
        let stop = self.stop.clone();
        self.blocking(move || process::run(&program, &cmd, &cwd, input, timeout, &stop))?
    }

    fn spawn_process(&mut self, cmd: Command) -> Result<ProcessId, String> {
        let (program, cwd) = self.command(&cmd)?;
        let post = self.post.clone();
        self.excluded(|s| s.processes.spawn(&program, &cmd, &cwd, post))
    }

    fn process_write(&mut self, id: ProcessId, data: Vec<u8>) -> Result<(), String> {
        self.excluded(|s| s.processes.write(id, &data))
    }

    fn process_close_stdin(&mut self, id: ProcessId) {
        self.processes.close_stdin(id);
    }

    fn process_kill(&mut self, id: ProcessId) {
        self.processes.kill(id);
    }

    fn http(&mut self, request: HttpRequest) -> Result<HttpResponse, String> {
        if self.granted.network.is_empty() {
            return Err(denied("network"));
        }
        let uri = net::check(&self.granted.network, &request.url)?;
        self.blocking(move || net::fetch(uri, request))?
    }

    fn http_stream(&mut self, request: HttpRequest) -> Result<RequestId, String> {
        if self.granted.network.is_empty() {
            return Err(denied("network"));
        }
        let uri = net::check(&self.granted.network, &request.url)?;
        let id = self.request_id();
        let post = self.post.clone();
        self.streams.start(id, uri, request, post).map(|_| id)
    }

    fn http_cancel(&mut self, id: RequestId) {
        self.streams.cancel(id);
    }

    fn secret_get(&mut self, key: String) -> Result<Option<String>, String> {
        secrets::check(&key, None)?;
        self.with_keyring(move |k, id| k.get(id, &key))
    }

    fn secret_set(&mut self, key: String, value: String) -> Result<(), String> {
        secrets::check(&key, Some(&value))?;
        self.with_keyring(move |k, id| k.set(id, &key, &value))
    }

    fn secret_delete(&mut self, key: String) -> Result<(), String> {
        secrets::check(&key, None)?;
        self.with_keyring(move |k, id| k.delete(id, &key))
    }

    fn clipboard_read(&mut self) -> Result<String, String> {
        self.need(self.granted.clipboard_read, "clipboard-read")?;
        match self.rpc(Call::ClipboardRead)? {
            Reply::Text(t) => Ok(t),
            _ => bad_reply(),
        }
    }

    fn clipboard_write(&mut self, text: String) -> Result<(), String> {
        self.need(self.granted.clipboard_write, "clipboard-write")?;
        if text.len() > 16 << 20 {
            return Err("clipboard-write: at most 16 MiB".into());
        }
        self.rpc(Call::ClipboardWrite(text)).map(|_| ())
    }

    fn watch(&mut self, path: String) -> Result<WatchId, String> {
        let host = self.access.pin(&path, false, true)?;
        if self.watcher.is_none() {
            let post = self.post.clone();
            let on_change: files::OnChange = Box::new(move |watch, path, change| {
                let kind = match change {
                    files::Change::Created => FsChangeKind::Created,
                    files::Change::Removed => FsChangeKind::Removed,
                    files::Change::Modified => FsChangeKind::Modified,
                    files::Change::Moved => FsChangeKind::Moved,
                };
                post.event(Event::FsChanged(FsChange { watch, path, kind }))
            });
            self.watcher = Some(Watcher::new(on_change, Arc::new(AtomicBool::new(false)))?);
        }
        self.watcher.as_mut().expect("just made").add(&host, &path)
    }

    fn unwatch(&mut self, id: WatchId) {
        if let Some(w) = &mut self.watcher {
            w.remove(id);
        }
    }

    fn trash(&mut self, path: String) -> Result<(), String> {
        let host = self.access.pin(&path, true, false)?;
        if host.original == self.access.data_dir.canonicalize().unwrap_or_default() {
            return Err("trash: not the data directory itself".into());
        }
        self.excluded(|_| files::trash_named(&host, &host.original))
    }

    fn open(&mut self, path_or_url: String) -> Result<(), String> {
        let lower = path_or_url.to_ascii_lowercase();
        let target = if ["http://", "https://", "mailto:"].iter().any(|p| lower.starts_with(p)) {
            self.need(self.granted.ui, "ui")?;
            if path_or_url.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err("open: not a valid URL".into());
            }
            (path_or_url, None)
        } else {
            use std::os::fd::AsRawFd;
            let pinned = self.access.pin(&path_or_url, false, true)?;
            (format!("/proc/{}/fd/{}", std::process::id(), pinned.file.as_raw_fd()), Some(pinned.file))
        };
        self.rpc(Call::Open(target.0, target.1)).map(|_| ())
    }

    fn list_dir(&mut self, path: String) -> Result<Vec<DirEntry>, String> {
        let host = self.access.pin(&path, false, true)?;
        let access = self.access.clone();
        let guest = PathBuf::from(&path);
        self.blocking(move || files::list_dir(&host, &guest, &access))?
    }

    fn mime_type(&mut self, path: String) -> Result<String, String> {
        let host = self.access.pin(&path, false, true)?;
        self.excluded(|_| files::mime_type_named(&host, &host.original))
    }

    fn file_mode(&mut self, path: String) -> Result<u32, String> {
        use std::os::unix::fs::PermissionsExt;
        let host = self.access.pin(&path, false, true)?;
        std::fs::metadata(&host).map(|m| m.permissions().mode() & 0o777).map_err(|e| e.to_string())
    }

    fn set_file_mode(&mut self, path: String, mode: u32) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        let host = self.access.pin(&path, true, false)?;
        std::fs::File::options()
            .read(true)
            .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
            .open(&host)
            .and_then(|f| f.set_permissions(std::fs::Permissions::from_mode(mode & 0o777)))
            .map_err(|e| e.to_string())
    }

    fn create_private(&mut self, path: String, directory: bool) -> Result<(), String> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let host = self.access.pin(&path, true, false)?;
        if directory {
            std::fs::DirBuilder::new().mode(0o700).create(&host).map_err(|e| e.to_string())
        } else {
            std::fs::File::options()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&host)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }

    fn publish_file(&mut self, staged: String, destination: String, replace: bool) -> Result<(), String> {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        let staged = self.access.pin(&staged, true, false)?;
        let destination = self.access.pin(&destination, true, false)?;
        let flags = if replace { RenameFlags::empty() } else { RenameFlags::NOREPLACE };
        renameat_with(CWD, &*staged, CWD, &*destination, flags).map_err(|e| e.to_string())
    }

    fn set_timer(&mut self, ms: u32, repeat: bool) -> TimerId {
        // Over the limit: the oldest timer goes.
        if self.timers.len() >= MAX_TIMERS
            && let Some(&first) = self.timers.keys().next()
        {
            self.timers.remove(&first);
        }
        self.next_timer = self.next_timer.wrapping_add(1).max(1);
        let every = Duration::from_millis(ms as u64).max(MIN_TIMER);
        self.timers.insert(self.next_timer, (Instant::now() + every, repeat.then_some(every)));
        self.next_timer
    }

    fn cancel_timer(&mut self, id: TimerId) {
        self.timers.remove(&id);
    }
}

/// Events a plugin may see with its permissions.
fn allowed(e: &Event, p: &Permissions) -> bool {
    match e {
        Event::PaneOpened(_)
        | Event::PaneClosed(_)
        | Event::FocusChanged(_)
        | Event::CwdChanged(_)
        | Event::TitleChanged(_)
        | Event::CommandStarted(_)
        | Event::CommandFinished(_)
        | Event::Bell(_)
        | Event::BlockStateChanged(_) => p.read_panes,
        Event::SurfaceOpened(_) => p.ui,
        _ => true,
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A trap's message: the reason first, without the wasm backtrace.
fn trap_message(e: &wasmtime::Error, state: &State) -> String {
    if state.stop.load(Ordering::Relaxed) {
        return "stopped".into();
    }
    if let Some(t) = e.downcast_ref::<wasmtime::Trap>() {
        return match t {
            wasmtime::Trap::Interrupt => format!("ran longer than {} s and was stopped", CALL_LIMIT.as_secs()),
            t => format!("crashed: {t}"),
        };
    }
    let s = format!("{e:#}");
    format!("crashed: {}", s.lines().next().unwrap_or(""))
}

enum Wake<T> {
    Message(T),
    Timers(Vec<TimerId>),
    Idle,
    Disconnected,
}

/// Due timers take precedence over queued traffic; idle waits remain bounded
/// so stopping an instance also works when its next timer is far in the future.
fn next_wake<T>(rx: &mpsc::Receiver<T>, timers: &BTreeMap<TimerId, (Instant, Option<Duration>)>) -> Wake<T> {
    let due = || {
        let now = Instant::now();
        timers.iter().filter(|(_, t)| t.0 <= now).map(|(id, _)| *id).collect::<Vec<_>>()
    };
    let ready = due();
    if !ready.is_empty() {
        return Wake::Timers(ready);
    }
    let idle = Duration::from_millis(250);
    let wait =
        timers.values().map(|t| t.0).min().map_or(idle, |at| at.saturating_duration_since(Instant::now()).min(idle));
    match rx.recv_timeout(wait) {
        Ok(message) => Wake::Message(message),
        Err(RecvTimeoutError::Disconnected) => Wake::Disconnected,
        Err(RecvTimeoutError::Timeout) => {
            let ready = due();
            if ready.is_empty() { Wake::Idle } else { Wake::Timers(ready) }
        }
    }
}

fn run(
    engine: &Engine,
    cache: &Mutex<HashMap<String, Component>>,
    spec: Spec,
    rx: mpsc::Receiver<ToPlugin>,
    sink: &Sink,
    post: Poster,
) -> Result<(), String> {
    let cached = cache.lock().map_err(err)?.get(&spec.hash).cloned();
    let component = match cached {
        Some(c) => c,
        None => {
            // Only compile verified WASM bytes. Serialized native components
            // cannot be authenticated by pathname ownership/mode checks.
            let c = Component::new(engine, &spec.wasm).map_err(|e| format!("not a valid plugin: {e}"))?;
            let mut cache = cache.lock().map_err(err)?;
            if cache.len() >= 16 {
                cache.clear();
            }
            cache.insert(spec.hash.clone(), c.clone());
            c
        }
    };
    let mut linker: Linker<State> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(err)?;
    Plugin::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s).map_err(err)?;

    let mut wasi = WasiCtxBuilder::new();
    // Paths the plugin sees (pane directories) are shown relative to it.
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) {
        wasi.env("HOME", home);
    }
    if spec.stderr {
        wasi.inherit_stderr();
    }
    std::fs::create_dir_all(&spec.data_dir).map_err(|e| format!("{}: {e}", spec.data_dir.display()))?;
    wasi.preopened_dir(&spec.data_dir, "/data", FsPerms::ReadWrite)
        .map_err(|e| format!("{}: {e}", spec.data_dir.display()))?;
    for f in &spec.granted.fs {
        f.validate_directory()?;
        let perms = if f.write { FsPerms::ReadWrite } else { FsPerms::ReadOnly };
        let host = f.host_path();
        // Seen at its real path. Missing directories are left out (the
        // plugin gets "not found").
        if host.is_dir() {
            let guest = host.display().to_string();
            wasi.preopened_dir(&host, &guest, perms).map_err(|e| format!("{}: {e}", host.display()))?;
        }
    }
    let backlog = post.backlog.clone();
    let state = State {
        config: spec.config.to_string(),
        key: spec.key.clone(),
        granted: spec.granted.clone(),
        sink: sink.clone(),
        wasi: wasi.build(),
        table: ResourceTable::new(),
        limits: Limits::new(spec.memory),
        call_start: Instant::now(),
        stop: post.stop.clone(),
        timers: BTreeMap::new(),
        next_timer: 0,
        access: Access::new(spec.data_dir.clone(), &spec.granted.fs)?,
        post,
        storage: Storage::new(spec.storage.clone()),
        processes: Processes::default(),
        streams: Streams::default(),
        watcher: None,
        keyring: None,
        images: HashMap::new(),
        next_image: 0,
        tasks: HashSet::new(),
        next_task: 0,
        next_request: 0,
        rate_window: Instant::now(),
        rate_calls: 0,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|s| &mut s.limits);
    store.epoch_deadline_callback(|cx| {
        let s = cx.data();
        if s.stop.load(Ordering::Relaxed) || s.call_start.elapsed() > CALL_LIMIT {
            Ok(UpdateDeadline::Interrupt)
        } else {
            Ok(UpdateDeadline::Continue(1))
        }
    });
    store.set_epoch_deadline(1);

    macro_rules! guarded {
        ($store:ident, $call:expr) => {{
            $store.data_mut().call_start = Instant::now();
            $store.set_epoch_deadline(1);
            match $call {
                Ok(v) => v,
                Err(e) => return Err(trap_message(&e, $store.data())),
            }
        }};
    }

    let plugin = guarded!(store, Plugin::instantiate(&mut store, &component, &linker));
    if let Err(e) = guarded!(store, plugin.call_init(&mut store)) {
        return Err(format!("init failed: {e}"));
    }
    sink(spec.key.clone(), FromPlugin::Started);

    loop {
        if store.data().stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match next_wake(&rx, &store.data().timers) {
            Wake::Message(ToPlugin::Event(e)) => {
                if let Event::ProcessOutput(ProcessOutput { data, .. }) | Event::HttpBody((_, data)) = &e {
                    backlog.fetch_sub(data.len(), Ordering::Relaxed);
                }
                if allowed(&e, &spec.granted) {
                    guarded!(store, plugin.call_on_event(&mut store, &e));
                }
            }
            Wake::Message(ToPlugin::Ui(s, e)) => guarded!(store, plugin.call_on_ui(&mut store, s, &e)),
            Wake::Idle => {}
            Wake::Disconnected => return Ok(()),
            Wake::Timers(due) => {
                let now = Instant::now();
                for id in due {
                    let timers = &mut store.data_mut().timers;
                    // An earlier callback may have cancelled another due timer.
                    let Some((_, every)) = timers.remove(&id) else { continue };
                    if let Some(every) = every {
                        timers.insert(id, (now + every, Some(every)));
                    }
                    guarded!(store, plugin.call_on_event(&mut store, &Event::Timer(id)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_messages_cannot_starve_due_timers() {
        let (tx, rx) = mpsc::channel();
        for message in 0..100 {
            tx.send(message).unwrap();
        }
        let mut timers = BTreeMap::new();
        timers.insert(1, (Instant::now(), None));
        assert!(matches!(next_wake(&rx, &timers), Wake::Timers(ids) if ids == [1]));
        timers.clear();
        assert!(matches!(next_wake(&rx, &timers), Wake::Message(0)));
        timers.insert(2, (Instant::now(), Some(Duration::from_millis(10))));
        assert!(matches!(next_wake(&rx, &timers), Wake::Timers(ids) if ids == [2]));
    }

    #[test]
    fn private_staging_and_modes_enforce_filesystem_grants() {
        use super::proteus::plugin::host::Host;
        let root = std::env::temp_dir().join(format!("proteus-mode-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        let read = root.join("read");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&read).unwrap();
        std::fs::write(read.join("file"), "outside").unwrap();
        let (post, _events) = Poster::testing();
        let mut state = State {
            config: "null".into(),
            key: Key { id: "test".into(), instance: 1 },
            granted: Permissions::default(),
            sink: Arc::new(|_, _| {}),
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limits: Limits::new(MEMORY),
            call_start: Instant::now(),
            stop: post.stop.clone(),
            timers: BTreeMap::new(),
            next_timer: 0,
            post,
            access: Access::new(data.clone(), &[crate::FsGrant { path: read.display().to_string(), write: false }])
                .unwrap(),
            storage: Storage::new(root.join("storage.toml")),
            processes: Processes::default(),
            streams: Streams::default(),
            watcher: None,
            keyring: None,
            images: HashMap::new(),
            next_image: 0,
            tasks: HashSet::new(),
            next_task: 0,
            next_request: 0,
            rate_window: Instant::now(),
            rate_calls: 0,
        };
        state.create_private("/data/staged".into(), false).unwrap();
        assert_eq!(state.file_mode("/data/staged".into()).unwrap(), 0o600);
        state.create_private("/data/directory".into(), true).unwrap();
        assert_eq!(state.file_mode("/data/directory".into()).unwrap(), 0o700);
        state.set_file_mode("/data/staged".into(), 0o700).unwrap();
        std::fs::write(data.join("staged"), "staged").unwrap();
        std::fs::write(data.join("destination"), "existing").unwrap();
        assert!(state.publish_file("/data/staged".into(), "/data/destination".into(), false).is_err());
        assert_eq!(std::fs::read_to_string(data.join("destination")).unwrap(), "existing");
        state.publish_file("/data/staged".into(), "/data/destination".into(), true).unwrap();
        assert_eq!(state.file_mode("/data/destination".into()).unwrap(), 0o700);
        assert_eq!(std::fs::read_to_string(data.join("destination")).unwrap(), "staged");
        assert!(state.set_file_mode(read.join("file").display().to_string(), 0o600).is_err());
        assert!(state.create_private(read.join("new").display().to_string(), false).is_err());
        assert!(state.publish_file("/data/destination".into(), read.join("new").display().to_string(), false).is_err());
        std::os::unix::fs::symlink(read.join("file"), data.join("link")).unwrap();
        assert!(state.set_file_mode("/data/link".into(), 0o600).is_err());
        assert_eq!(std::fs::read_to_string(read.join("file")).unwrap(), "outside");
        state.rate_window = Instant::now();
        state.rate_calls = 2000;
        assert!(state.rpc(Call::Log("over rate".into())).is_err());
        assert!(state.stop.load(Ordering::Acquire));
        state.stop.store(false, Ordering::Release);
        state.rate_calls = 0;
        assert!(state.rpc(Call::SendText { pane: 1, text: "x".repeat((4 << 20) + 1), paste: false }).is_err());
        assert!(state.stop.load(Ordering::Acquire));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn event_flood_stops_instead_of_growing_or_blocking_the_gui() {
        let (post, _events) = Poster::testing();
        for i in 0..256 {
            assert!(post.event(Event::Timer(i)));
        }
        assert!(!post.event(Event::Timer(257)));
        assert!(post.stopped());
    }

    #[test]
    fn stream_cancellation_interrupts_backlog_wait() {
        let (post, _events) = Poster::testing();
        post.backlog.store(MAX_BACKLOG + 1, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let worker = std::thread::spawn(move || post.data_until(1, Event::HttpBody((1, vec![0])), &c));
        std::thread::sleep(Duration::from_millis(20));
        cancel.store(true, Ordering::Relaxed);
        let end = Instant::now() + Duration::from_secs(1);
        while !worker.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(worker.is_finished());
        assert!(!worker.join().unwrap());
    }
}
