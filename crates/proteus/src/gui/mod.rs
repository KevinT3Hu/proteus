//! The Wayland frontend: one xdg-toplevel window with tabs of split panes,
//! each pane a terminal session, rendered with wgpu.
//!
//! Event loop: calloop drives the Wayland connection, a channel fed by the PTY
//! reader thread, and timers. Frames are drawn only when something changed
//! and only after the compositor's frame callback (no busy loop).

mod chrome;
mod copy;
mod font;
mod ime;
mod keys;
mod pane;
mod render;
mod scroll;
mod textinput;
mod workspace;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use proteus_core::event::{ClipboardKind, Terminator};
use proteus_core::input::{self, KeyAction, MouseAction, MouseButton, MouseEvent};
use proteus_core::selection::SelectionKind;
use proteus_core::{ClipboardRead, Event, Point, TermMode};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle,
    WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::primary_selection::device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler};
use smithay_client_toolkit::primary_selection::selection::{PrimarySelectionSource, PrimarySelectionSourceHandler};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device::WlDataDevice;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device_manager::DndAction;
use smithay_client_toolkit::reexports::client::protocol::wl_data_source::WlDataSource;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{Transform, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1;
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use smithay_client_toolkit::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec,
    ThemedPointer,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};

use crate::config::{Blink, Config, HintBar, TabBar};
use crate::layout::{self, Axis, Dir, Layout, PaneId, Rect, Tab};
use crate::modes::{Action, Lookup, Mode, Scroll};
use crate::spec::{self, LayoutSpec};
use textinput::{Input, Purpose};
use workspace::SessionStore;
use crate::session::SpawnOptions;
use font::Fonts;
use pane::Pane;
use render::{FrameInfo, Overlay, PaneFrame, Renderer, Theme};

const APP_ID: &str = "dev.proteus.Proteus";
const MIMES: [&str; 4] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING"];
/// Padding around the grid, logical pixels.
const PAD: f64 = 4.0;
/// Longest time a synchronized update (mode 2026) may hold a frame.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Cursor blink half-period.
const BLINK: Duration = Duration::from_millis(530);
/// Stop blinking (cursor stays on) after this long without input.
const BLINK_IDLE: Duration = Duration::from_secs(30);
/// How often the session file is brought up to date.
const AUTOSAVE: Duration = Duration::from_secs(2);

/// What clipboard contents are read for.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ClipTarget {
    /// Paste into a pane (subject to paste protection).
    Paste(PaneId),
    /// Answer an approved OSC 52 read from a pane.
    Osc52(PaneId, ClipboardKind, Terminator),
}

/// A question (or notice) shown at the bottom of the window. While one is
/// up, keys answer it instead of going to the application.
#[derive(Debug)]
enum Prompt {
    ClipboardRead(PaneId, ClipboardKind, Terminator),
    Paste(PaneId, String),
    Notice(String),
    /// Close these panes (something is running in them).
    Close(Vec<PaneId>, String),
    /// Close the window (something is running).
    Quit(String),
    /// Restore the session saved before an unclean exit.
    Restore(LayoutSpec),
}

pub(crate) enum Msg {
    Pty(PaneId, Vec<u8>),
    PtyClosed(PaneId),
    Clipboard(ClipTarget, String),
}

/// Lets wgpu's GL backend reach the Wayland display.
#[derive(Debug)]
struct Display(Connection);

impl HasDisplayHandle for Display {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let ptr = NonNull::new(self.0.backend().display_ptr().cast()).ok_or(HandleError::Unavailable)?;
        let raw = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(ptr));
        // SAFETY: the display stays alive as long as the connection we own.
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

/// User data for fractional-scale and viewporter objects.
struct ScaleData;

impl Dispatch2<WpFractionalScaleManagerV1, App> for ScaleData {
    fn event(&self, _: &mut App, _: &WpFractionalScaleManagerV1, _: <WpFractionalScaleManagerV1 as Proxy>::Event, _: &Connection, _: &QueueHandle<App>) {}
}
impl Dispatch2<WpViewporter, App> for ScaleData {
    fn event(&self, _: &mut App, _: &WpViewporter, _: <WpViewporter as Proxy>::Event, _: &Connection, _: &QueueHandle<App>) {}
}
impl Dispatch2<WpViewport, App> for ScaleData {
    fn event(&self, _: &mut App, _: &WpViewport, _: <WpViewport as Proxy>::Event, _: &Connection, _: &QueueHandle<App>) {}
}
impl Dispatch2<WpFractionalScaleV1, App> for ScaleData {
    fn event(&self, app: &mut App, _: &WpFractionalScaleV1, ev: wp_fractional_scale_v1::Event, _: &Connection, _: &QueueHandle<App>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = ev {
            app.set_scale(scale as f64 / 120.0);
        }
    }
}

#[derive(Default)]
struct Mouse {
    /// Surface-local logical position.
    pos: (f64, f64),
    /// Button reported to a pane's application while held.
    reported: Option<(PaneId, MouseButton)>,
    /// Left button held for a local selection in this pane.
    selecting: Option<PaneId>,
    /// Anchor of a single click; becomes a selection once the pointer moves.
    pending: Option<Point>,
    last_click: Option<(Instant, PaneId, Point, u8)>,
    scroll_acc: f64,
    drag: Option<Drag>,
    icon: Option<CursorIcon>,
}

enum Drag {
    Divider(layout::Divider),
    /// Tab being dragged, by index.
    Tab(usize),
}

/// One pane of a frame, without borrows (see [`FrameParts`]).
struct PanePart {
    id: PaneId,
    x: f32,
    y: f32,
    focused: bool,
    preedit: Option<ime::Preedit>,
    copy_cursor: Option<(usize, usize)>,
    marks: Vec<render::Mark>,
    skip: usize,
}

/// Frame description without borrows of the panes (see [`App::draw`]).
struct FrameParts {
    panes: Vec<PanePart>,
    info: FrameInfo<'static>,
}

impl FrameParts {
    fn resolve<'a>(self, panes: &'a HashMap<PaneId, Pane>) -> FrameInfo<'a> {
        let mut info: FrameInfo<'a> = self.info;
        info.panes = self
            .panes
            .into_iter()
            .filter_map(|p| {
                Some(PaneFrame {
                    term: &panes.get(&p.id)?.term,
                    x: p.x,
                    y: p.y,
                    focused: p.focused,
                    preedit: p.preedit,
                    copy_cursor: p.copy_cursor,
                    marks: p.marks,
                    skip: p.skip,
                })
            })
            .collect();
        info
    }
}

struct App {
    // Drop order matters: the wgpu surface must go before the window.
    renderer: Renderer,
    window: Window,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    data_manager: Option<DataDeviceManagerState>,
    primary_manager: Option<PrimarySelectionManagerState>,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    text_input_manager: Option<ZwpTextInputManagerV3>,
    ime: Option<ime::Ime>,
    conn: Connection,
    qh: QueueHandle<App>,
    handle: LoopHandle<'static, App>,
    sender: Sender<Msg>,

    cfg: Config,
    keymap: crate::modes::Keymap,
    mode: Mode,
    fonts: Fonts,
    font_size: f32,
    prompts: VecDeque<Prompt>,
    /// OSC 52 reads allowed for the rest of the session ("always").
    osc52_always: bool,
    /// Blink phase: cursor hidden.
    blink_off: bool,
    blink_timer: Option<RegistrationToken>,
    last_input: Instant,

    panes: HashMap<PaneId, Pane>,
    /// Never empty while running: the last pane closing ends the program.
    tabs: Vec<Tab>,
    active: usize,
    next_id: PaneId,
    hostname: String,
    window_title: String,
    exit_code: i32,
    /// The bottom-bar text input, when open; it takes the keys.
    input: Option<Input>,
    copy: Option<scroll::CopyState>,
    search: Option<scroll::SearchState>,
    session: Option<SessionStore>,
    /// The window was closed by the user (not by the last pane ending).
    quit_requested: bool,
    /// A divider is being dragged: PTY resizes wait for the release.
    defer_pty_resize: bool,
    /// The timer delivering layouts' `run` lines is active.
    run_timer: bool,

    scale: f64,
    /// Logical window size.
    size: (u32, u32),
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    sync_since: Option<Instant>,
    retry_scheduled: bool,
    focused: bool,

    keyboard: Option<WlKeyboard>,
    pointer: Option<ThemedPointer>,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    mods: Modifiers,
    levels: keys::Levels,
    /// Keys whose press the multiplexer consumed; their release is dropped too.
    consumed: Vec<u32>,
    serial: u32,
    clipboard: Option<(CopyPasteSource, String)>,
    primary: Option<(PrimarySelectionSource, String)>,
    mouse: Mouse,
    exit: bool,
}

/// Run the GUI. `command` runs in the first pane instead of the shell.
/// Run the GUI. `command` runs in the first pane instead of the shell;
/// `layout` opens a layout file instead.
pub fn run(
    cfg: Config,
    config_error: Option<String>,
    command: Option<Vec<OsString>>,
    layout: Option<LayoutSpec>,
) -> io::Result<i32> {
    let err = |e: String| io::Error::other(e);
    let conn = Connection::connect_to_env().map_err(|e| err(format!("cannot connect to Wayland: {e}")))?;
    let (globals, queue) = registry_queue_init::<App>(&conn).map_err(|e| err(format!("Wayland registry: {e}")))?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), queue).insert(handle.clone()).map_err(|e| err(e.to_string()))?;

    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| err(format!("wl_compositor: {e}")))?;
    let xdg = XdgShell::bind(&globals, &qh).map_err(|e| err(format!("xdg_wm_base: {e}")))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| err(format!("wl_shm: {e}")))?;
    let fractional_manager = globals.bind::<WpFractionalScaleManagerV1, App, _>(&qh, 1..=1, ScaleData).ok();
    let viewporter = globals.bind::<WpViewporter, App, _>(&qh, 1..=1, ScaleData).ok();
    let text_input_manager = globals.bind::<ZwpTextInputManagerV3, App, _>(&qh, 1..=1, ime::ImeData).ok();

    let surface = compositor.create_surface(&qh);
    let (viewport, fractional) = match (&viewporter, &fractional_manager) {
        (Some(vp), Some(fm)) => {
            (Some(vp.get_viewport(&surface, &qh, ScaleData)), Some(fm.get_fractional_scale(&surface, &qh, ScaleData)))
        }
        _ => (None, None),
    };
    let window = xdg.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("proteus");
    window.set_app_id(APP_ID);
    window.set_min_size(Some((100, 60)));
    window.commit();

    let fonts = Fonts::new(&cfg.font_family, cfg.font_size, 1.0).map_err(err)?;
    let m = fonts.metrics();
    let pad = (PAD * 2.0) as u32;
    let size = (80 * m.cell_w + pad, 24 * m.cell_h + pad);

    let display = Display(conn.clone());
    let mut desc = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display)).with_env();
    if wgpu::Backends::from_env().is_none() {
        desc.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
    }
    let instance = wgpu::Instance::new(desc);
    let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(conn.backend().display_ptr().cast()).ok_or_else(|| err("no Wayland display".into()))?,
    ));
    let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(window.wl_surface().id().as_ptr().cast()).ok_or_else(|| err("no surface".into()))?,
    ));
    // SAFETY: the display (kept alive by `conn`) and the surface (owned by
    // `window`) outlive the wgpu surface, which App drops first.
    let wsurface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_display),
            raw_window_handle: raw_window,
        })
    }
    .map_err(|e| err(format!("cannot create GPU surface: {e}")))?;
    let renderer = Renderer::new(&instance, wsurface, size).map_err(err)?;
    if std::env::var_os("PROTEUS_DEBUG").is_some() {
        eprintln!("proteus: rendering with {}", renderer.adapter_info);
        eprintln!("proteus: text-input-v3 {}", if text_input_manager.is_some() { "available" } else { "unavailable" });
    }

    let (sender, rx) = channel::channel();
    handle
        .insert_source(rx, |ev, _, app: &mut App| match ev {
            channel::Event::Msg(Msg::Pty(id, bytes)) => app.pty_output(id, &bytes),
            channel::Event::Msg(Msg::PtyClosed(id)) => app.close_pane(id),
            channel::Event::Msg(Msg::Clipboard(target, text)) => app.clipboard_received(target, text),
            channel::Event::Closed => app.exit = true,
        })
        .map_err(|e| err(e.to_string()))?;

    let mut app = App {
        renderer,
        window,
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        shm,
        data_manager: DataDeviceManagerState::bind(&globals, &qh).ok(),
        primary_manager: PrimarySelectionManagerState::bind(&globals, &qh).ok(),
        viewport,
        fractional,
        text_input_manager,
        ime: None,
        conn: conn.clone(),
        qh,
        handle,
        sender,
        keymap: cfg.keymap(),
        mode: Mode::Normal,
        fonts,
        font_size: cfg.font_size,
        cfg,
        prompts: config_error.map(|e| Prompt::Notice(format!("config error: {e}"))).into_iter().collect(),
        osc52_always: false,
        blink_off: false,
        blink_timer: None,
        last_input: Instant::now(),
        panes: HashMap::new(),
        tabs: Vec::new(),
        active: 0,
        next_id: 1,
        hostname: rustix::system::uname().nodename().to_string_lossy().into_owned(),
        window_title: String::new(),
        exit_code: 0,
        input: None,
        copy: None,
        search: None,
        session: None,
        quit_requested: false,
        defer_pty_resize: false,
        run_timer: false,
        scale: 1.0,
        size,
        configured: false,
        frame_pending: false,
        dirty: true,
        sync_since: None,
        retry_scheduled: false,
        focused: false,
        keyboard: None,
        pointer: None,
        data_device: None,
        primary_device: None,
        mods: Modifiers::default(),
        levels: keys::Levels::new(),
        consumed: Vec::new(),
        serial: 0,
        clipboard: None,
        primary: None,
        mouse: Mouse::default(),
        exit: false,
    };

    // What to open: a layout, the saved session, or one pane.
    use crate::config::Restore;
    if app.cfg.restore != Restore::Never {
        app.session = SessionStore::acquire();
    }
    let saved = app.session.as_ref().and_then(SessionStore::read);
    let explicit = command.is_some() || layout.is_some();
    match (layout, saved) {
        (Some(l), _) => app.open_layout(&l)?,
        (None, Some(s)) if !explicit && app.cfg.restore == Restore::Always => {
            if let Err(e) = app.open_layout(&s) {
                app.notice(format!("cannot restore session: {e}"));
            }
        }
        (None, saved) => {
            if let Some(s) = saved.filter(|s| !explicit && app.cfg.restore == Restore::Ask && s.clean == Some(false)) {
                app.prompts.push_back(Prompt::Restore(s));
            }
        }
    }
    if app.tabs.is_empty() {
        app.open_default(command)?;
    }
    if app.session.is_some() {
        let _ = app.handle.insert_source(Timer::from_duration(AUTOSAVE), |_, _, app| {
            app.autosave();
            TimeoutAction::ToDuration(AUTOSAVE)
        });
    }

    // Debug aid: PROTEUS_SNAPSHOT_AFTER=secs ends the session after that
    // long, so the snapshot shows a running (e.g. full-screen) program.
    if let Some(secs) = std::env::var("PROTEUS_SNAPSHOT_AFTER").ok().and_then(|s| s.parse::<f64>().ok()) {
        let _ = app.handle.insert_source(Timer::from_duration(Duration::from_secs_f64(secs)), |_, _, app| {
            app.exit = true;
            TimeoutAction::Drop
        });
    }
    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
        app.draw();
        let _ = app.conn.flush();
    }
    // Debug aid: PROTEUS_SNAPSHOT=file.png saves the final frame.
    if let Some(path) = std::env::var_os("PROTEUS_SNAPSHOT") {
        let mut parts = app.frame_parts();
        // PROTEUS_PREEDIT=text[@byte] previews a composition (testing aid).
        if let Some(pre) = std::env::var("PROTEUS_PREEDIT").ok().filter(|s| !s.is_empty()) {
            let (text, cursor) = match pre.rsplit_once('@') {
                Some((t, c)) => (t.to_string(), c.parse().ok().map(|c: usize| (c, c))),
                None => (pre.clone(), Some((pre.len(), pre.len()))),
            };
            let fid = app.focused_id();
            for p in parts.panes.iter_mut().filter(|p| p.id == fid) {
                p.focused = true;
                p.preedit = Some(ime::Preedit { text: text.clone(), cursor });
            }
        }
        let info = parts.resolve(&app.panes);
        let (w, h, px) = app.renderer.snapshot(&mut app.fonts, &info);
        drop(info);
        if let Err(e) = image::save_buffer(&path, &px, w, h, image::ColorType::Rgba8) {
            eprintln!("proteus: snapshot: {e}");
        }
    }
    app.finish_session();
    // A single remaining session reports its exit status, as before.
    if app.panes.len() == 1
        && let Some(p) = app.panes.values_mut().next()
    {
        p.pty.hangup();
        app.exit_code = p.pty.wait()?.code().unwrap_or(0);
    }
    let code = app.exit_code;
    drop(app);
    Ok(code)
}

impl App {
    // --- geometry ------------------------------------------------------------

    fn pad(&self) -> u32 {
        (PAD * self.scale).round() as u32
    }

    fn physical(&self) -> (u32, u32) {
        ((self.size.0 as f64 * self.scale).round() as u32, (self.size.1 as f64 * self.scale).round() as u32)
    }

    fn cell(&self) -> (u32, u32) {
        let m = self.fonts.metrics();
        (m.cell_w, m.cell_h)
    }

    fn layout_metrics(&self) -> layout::Metrics {
        let (w, h) = self.cell();
        layout::Metrics { cell_w: w as i32, cell_h: h as i32, gap: w as i32 }
    }

    fn tab_bar_visible(&self) -> bool {
        match self.cfg.tab_bar {
            TabBar::Always => true,
            TabBar::Never => false,
            TabBar::Auto => self.tabs.len() > 1,
        }
    }

    fn hint_bar_visible(&self) -> bool {
        if self.input.is_some() {
            return true;
        }
        match self.cfg.hint_bar {
            HintBar::Always => true,
            HintBar::Never => false,
            HintBar::InMode => self.mode != Mode::Normal,
        }
    }

    /// The part of the window given to panes, in physical pixels.
    fn area(&self) -> Rect {
        let (pw, ph) = self.physical();
        let p = self.pad() as i32;
        let ch = self.cell().1 as i32;
        let mut top = p;
        let mut bottom = ph as i32 - p;
        if self.tab_bar_visible() {
            top += ch + p;
        }
        if self.cfg.hint_bar == HintBar::Always {
            bottom -= ch + p;
        }
        Rect::new(p, top, (pw as i32 - 2 * p).max(1), (bottom - top).max(1))
    }

    /// Visible panes of the active tab.
    fn view(&self) -> Layout {
        self.tabs[self.active].layout(self.area(), self.layout_metrics())
    }

    fn focused_id(&self) -> PaneId {
        self.tabs[self.active].focus
    }

    fn focused_pane(&self) -> &Pane {
        &self.panes[&self.focused_id()]
    }

    fn focused_pane_mut(&mut self) -> &mut Pane {
        let id = self.focused_id();
        self.panes.get_mut(&id).expect("focused pane exists")
    }

    /// Recompute surface size and grids after a size, scale or font change.
    fn apply_size(&mut self) {
        let (pw, ph) = self.physical();
        if let Some(vp) = &self.viewport {
            vp.set_destination(self.size.0 as i32, self.size.1 as i32);
        }
        self.renderer.resize(pw, ph);
        self.relayout();
    }

    /// Fit every pane of every tab to its rectangle.
    fn relayout(&mut self) {
        let (area, m, cell) = (self.area(), self.layout_metrics(), self.cell());
        for t in &self.tabs {
            for (id, r) in t.layout(area, m).panes {
                if let Some(p) = self.panes.get_mut(&id) {
                    let cols = (r.w / m.cell_w).max(2) as u16;
                    let rows = (r.h / m.cell_h).max(1) as u16;
                    p.resize(cols, rows, cell, !self.defer_pty_resize);
                }
            }
        }
        self.dirty = true;
    }

    fn set_scale(&mut self, scale: f64) {
        if (scale - self.scale).abs() < 1e-6 || scale <= 0.0 {
            return;
        }
        self.scale = scale;
        if self.fonts.set_size(self.font_size, scale) {
            self.renderer.clear_glyphs();
        }
        self.apply_size();
    }

    fn set_font_size(&mut self, size: f32) {
        self.font_size = size.clamp(4.0, 96.0);
        if self.fonts.set_size(self.font_size, self.scale) {
            self.renderer.clear_glyphs();
            self.apply_size();
        }
    }

    /// Physical position of a surface-local logical position.
    fn phys(&self, pos: (f64, f64)) -> (i32, i32) {
        ((pos.0 * self.scale) as i32, (pos.1 * self.scale) as i32)
    }

    fn pane_at(&self, pos: (f64, f64)) -> Option<PaneId> {
        let (x, y) = self.phys(pos);
        self.view().pane_at(x, y)
    }

    /// Cell (col, viewport row) of pane `id` under a logical position, and
    /// the pixel offset inside the grid; clamped to the grid.
    fn cell_in(&self, id: PaneId, pos: (f64, f64)) -> Option<(usize, usize, (u32, u32))> {
        let r = self.view().rect(id)?;
        let t = &self.panes.get(&id)?.term;
        let (cw, ch) = self.cell();
        let px = (pos.0 * self.scale - r.x as f64).max(0.0);
        let py = (pos.1 * self.scale - r.y as f64).max(0.0);
        let col = ((px / cw as f64) as usize).min(t.cols() - 1);
        let row = ((py / ch as f64) as usize + self.row_skip(id)).min(t.lines() - 1);
        Some((col, row, (px as u32, py as u32)))
    }

    /// Top of the bottom bar when it is drawn over the panes (not reserved).
    fn overlay_top(&self) -> Option<i32> {
        if !self.hint_bar_visible() || self.cfg.hint_bar == HintBar::Always {
            return None;
        }
        let (_, ph) = self.physical();
        let (p, ch) = (self.pad() as f32, self.cell().1 as f32);
        Some((ph as f32 - p - ch - p / 2.0).floor() as i32)
    }

    /// Rows to draw pane `id` shifted up by (see [`render::PaneFrame::skip`]):
    /// one while the Scroll-mode cursor, or the search match being typed
    /// for, is on a last row that the bottom bar covers.
    fn row_skip(&self, id: PaneId) -> usize {
        let Some(bar) = self.overlay_top() else { return 0 };
        let (Some(r), Some(p)) = (self.view().rect(id), self.panes.get(&id)) else { return 0 };
        let t = &p.term;
        let ch = self.cell().1 as i32;
        if t.lines() < 2 || r.y + t.lines() as i32 * ch <= bar {
            return 0;
        }
        let top = t.grid().viewport_line(0);
        let last = top + t.lines() as u64 - 1;
        let at_last = |l: u64| l == last;
        let cursor = self.copy.as_ref().filter(|c| c.pane == id).is_some_and(|c| at_last(c.cursor.line));
        let current = self
            .search
            .as_ref()
            .filter(|s| s.pane == id)
            .and_then(|s| s.matches.get(s.current?))
            .is_some_and(|m| at_last(m.start.line) || at_last(m.end.line));
        usize::from(cursor || (current && self.input.is_some()))
    }

    fn point_in(&self, id: PaneId, pos: (f64, f64)) -> Option<Point> {
        let (col, row, _) = self.cell_in(id, pos)?;
        Some(Point::new(self.panes[&id].term.grid().viewport_line(row), col))
    }

    // --- drawing -------------------------------------------------------------

    fn theme(&self) -> Theme {
        let pal = self.focused_pane().term.palette();
        let (fg, bg) = (pal.foreground(), pal.background());
        Theme { fg, bg, accent: pal.indexed(4), bar: chrome::mix(bg, fg, 0.10), dim: chrome::mix(bg, fg, 0.35) }
    }

    fn tab_labels(&self, theme: &Theme) -> Vec<render::Label> {
        let (pw, _) = self.physical();
        let p = self.pad() as f32;
        let cw = self.cell().0 as f32;
        let titles: Vec<String> = self
            .tabs
            .iter()
            .map(|t| {
                let title = t.name.as_deref().unwrap_or_else(|| self.panes.get(&t.focus).map_or("", |p| p.title()));
                let n = t.panes().len();
                match (n, t.zoomed) {
                    (1, _) => title.to_string(),
                    (n, true) => format!("{title} [{n}, zoom]"),
                    (n, false) => format!("{title} [{n}]"),
                }
            })
            .collect();
        let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
        chrome::tab_bar(&titles, self.active, p, p, cols, cw, theme)
    }

    fn frame_parts(&self) -> FrameParts {
        let view = self.view();
        let fid = self.focused_id();
        let theme = self.theme();
        let (pw, ph) = self.physical();
        let (cw, ch) = self.cell();
        let (cw, ch) = (cw as f32, ch as f32);
        let p = self.pad() as f32;
        let preedit = self.ime.as_ref().and_then(|i| i.preedit.clone());
        let panes = view
            .panes
            .iter()
            .map(|&(id, r)| {
                let focused = id == fid;
                PanePart {
                    id,
                    x: r.x as f32,
                    y: r.y as f32,
                    focused: focused && self.focused,
                    preedit: if focused && self.input.is_none() { preedit.clone() } else { None },
                    copy_cursor: self.copy_cursor_cell(id),
                    marks: self.search_marks(id),
                    skip: self.row_skip(id),
                }
            })
            .collect();

        let mut rects = Vec::new();
        let thick = (self.scale.round() as f32).max(1.0);
        for d in &view.dividers {
            let g = &d.gap;
            let r = match d.axis {
                Axis::X => [(g.x as f32 + g.w as f32 / 2.0 - thick / 2.0).round(), g.y as f32, thick, g.h as f32],
                Axis::Y => [g.x as f32, (g.y as f32 + g.h as f32 / 2.0 - thick / 2.0).round(), g.w as f32, thick],
            };
            rects.push((r, theme.dim));
        }
        // Focused pane: accent the divider lines around it.
        if view.panes.len() > 1
            && let Some(r) = view.rect(fid)
        {
            let area = self.area();
            let half = self.layout_metrics().gap as f32 / 2.0;
            let (x0, y0) = ((r.x as f32 - half).max(area.x as f32), (r.y as f32 - half).max(area.y as f32));
            let x1 = (r.right() as f32 + half).min(area.right() as f32);
            let y1 = (r.bottom() as f32 + half).min(area.bottom() as f32);
            let mid = |v: f32| (v - thick / 2.0).round();
            if r.x > area.x {
                rects.push(([mid(r.x as f32 - half), y0, thick, y1 - y0], theme.accent));
            }
            if r.right() < area.right() {
                rects.push(([mid(r.right() as f32 + half), y0, thick, y1 - y0], theme.accent));
            }
            if r.y > area.y {
                rects.push(([x0, mid(r.y as f32 - half), x1 - x0, thick], theme.accent));
            }
            if r.bottom() < area.bottom() {
                rects.push(([x0, mid(r.bottom() as f32 + half), x1 - x0, thick], theme.accent));
            }
        }

        let mut labels = Vec::new();
        if self.tab_bar_visible() {
            rects.push(([0.0, 0.0, pw as f32, 1.5 * p + ch], theme.bar));
            labels.extend(self.tab_labels(&theme));
        }
        if self.hint_bar_visible() {
            let y = ph as f32 - p - ch;
            rects.push(([0.0, y - p / 2.0, pw as f32, ph as f32 - y + p / 2.0], theme.bar));
            let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
            if let Some(i) = &self.input {
                use unicode_segmentation::UnicodeSegmentation;
                use unicode_width::UnicodeWidthStr;
                let caret_cols: usize = i.before_cursor().graphemes(true).map(|g| g.width().clamp(1, 2)).sum();
                let (l, x) = chrome::input_bar(i.purpose.badge(), &i.text, caret_cols, &i.status, p, y, cols, cw, &theme);
                labels.extend(l);
                rects.push(([x, y, (cw / 8.0).round().max(1.0), ch], theme.fg));
            } else {
                let mut name = self.mode.name().to_string();
                if self.tabs[self.active].zoomed {
                    name.push_str(" (zoom)");
                }
                if let Some(s) = &self.search {
                    name = format!("{name} {}", s.status());
                }
                labels.extend(chrome::hint_bar(&name, &self.keymap.hints(self.mode), p, y, cols, cw, &theme));
            }
        }

        FrameParts {
            panes,
            info: FrameInfo {
                panes: Vec::new(),
                pad: p,
                theme,
                cursor_blink_off: self.blink_off,
                ligatures: self.cfg.ligatures,
                rects,
                labels,
                overlay: self.prompts.front().map(Prompt::overlay),
            },
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn window_surface(&self) -> &WlSurface {
        self.window.wl_surface()
    }

    fn update_title(&mut self) {
        let t = self.focused_pane().title();
        let t = if t.is_empty() { "proteus" } else { t }.to_string();
        if t != self.window_title {
            self.window.set_title(t.clone());
            self.window_title = t;
        }
    }

    fn notice(&mut self, msg: String) {
        self.prompts.push_back(Prompt::Notice(msg));
        self.dirty = true;
    }

    // --- input method ----------------------------------------------------------

    /// Text committed by the input method is sent as if typed.
    pub(crate) fn commit_text(&mut self, text: &str) {
        self.restart_blink();
        if self.input.is_some() {
            return self.input_text(text);
        }
        let p = self.focused_pane_mut();
        p.term.scroll_to_bottom();
        p.write(text.as_bytes());
    }

    pub(crate) fn scroll_focused_to_bottom(&mut self) {
        self.focused_pane_mut().term.scroll_to_bottom();
    }

    /// Tell the input method where the cursor (or the preedit cursor) is.
    pub(crate) fn update_ime_rect(&mut self) {
        let (cw, ch) = self.cell();
        let Some(r) = self.view().rect(self.focused_id()) else { return };
        let t = &self.focused_pane().term;
        let (cols, x, y) = (t.cols(), t.cursor().x, t.cursor().y);
        let s = self.scale;
        let Some(ime) = self.ime.as_mut() else { return };
        let col = match &ime.preedit {
            Some(p) => render::preedit_start(x, p.width(), cols) + p.cursor_col().unwrap_or(0),
            None => x,
        }
        .min(cols.saturating_sub(1));
        let lx = ((r.x as f64 + col as f64 * cw as f64) / s).round() as i32;
        let ly = ((r.y as f64 + y as f64 * ch as f64) / s).round() as i32;
        let w = (cw as f64 / s).ceil() as i32;
        let h = (ch as f64 / s).ceil() as i32;
        ime.set_cursor_rect((lx, ly, w.max(1), h.max(1)));
    }

    fn schedule(&mut self, after: Duration) {
        if self.retry_scheduled {
            return;
        }
        self.retry_scheduled = true;
        let _ = self.handle.insert_source(Timer::from_duration(after), |_, _, app| {
            app.retry_scheduled = false;
            app.dirty = true;
            TimeoutAction::Drop
        });
    }

    fn draw(&mut self) {
        if !self.configured || self.frame_pending || self.exit {
            return;
        }
        let visible: Vec<PaneId> = self.view().panes.iter().map(|p| p.0).collect();
        // Mode 2026: hold the frame while an application is mid-update.
        if visible.iter().any(|id| self.panes.get(id).is_some_and(|p| p.term.is_synchronized())) {
            let since = *self.sync_since.get_or_insert_with(Instant::now);
            let held = since.elapsed();
            if held < SYNC_TIMEOUT {
                self.schedule(SYNC_TIMEOUT - held);
                return;
            }
        } else {
            self.sync_since = None;
        }
        let mut changed = std::mem::take(&mut self.dirty);
        for (id, p) in self.panes.iter_mut() {
            // Hidden panes' damage is dropped: they redraw fully when shown.
            if !p.term.take_damage().is_empty() && visible.contains(id) {
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let surface = self.window.wl_surface().clone();
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let info = self.frame_parts().resolve(&self.panes);
        let ok = self.renderer.draw(&mut self.fonts, &info);
        drop(info);
        if ok {
            self.frame_pending = true;
        } else {
            // No buffer available (resize race): try again shortly.
            self.dirty = true;
            self.schedule(Duration::from_millis(16));
        }
        if std::mem::take(&mut self.renderer.redraw_needed) {
            self.dirty = true;
        }
        self.update_ime_rect();
    }

    // --- panes and tabs -------------------------------------------------------

    /// Start a shell in a new pane (not yet placed in a tab).
    fn spawn_pane(
        &mut self,
        cols: u16,
        rows: u16,
        cwd: Option<PathBuf>,
        command: Option<Vec<OsString>>,
    ) -> io::Result<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        let (cw, ch) = self.cell();
        let opts = SpawnOptions {
            command: command.or_else(|| self.cfg.command.clone()),
            integration: self.cfg.shell_integration,
            cwd,
            cols: cols.max(2),
            rows: rows.max(1),
            cell: Some((cw as u16, ch as u16)),
        };
        let pane = Pane::spawn(id, &opts, self.cfg.core(), (cw, ch), self.sender.clone())?;
        self.panes.insert(id, pane);
        Ok(id)
    }

    /// Split the focused pane (`None`: along its longer side).
    fn split(&mut self, axis: Option<Axis>) {
        let (area, m) = (self.area(), self.layout_metrics());
        let id = self.focused_id();
        let Some(r) = self.tabs[self.active].full_layout(area, m).rect(id) else { return };
        let (cols, rows) = (r.w / m.cell_w, r.h / m.cell_h);
        // Cells are about twice as tall as wide.
        let axis = axis.unwrap_or(if cols >= 2 * rows { Axis::X } else { Axis::Y });
        if !Tab::can_split(r, axis, m) {
            return;
        }
        let (c, rw) = match axis {
            Axis::X => (cols / 2, rows),
            Axis::Y => (cols, rows / 2),
        };
        let cwd = self.focused_pane().cwd(&self.hostname);
        match self.spawn_pane(c as u16, rw as u16, cwd, None) {
            Ok(new) => {
                self.tabs[self.active].split(new, axis);
                self.relayout();
            }
            Err(e) => self.notice(format!("cannot start shell: {e}")),
        }
    }

    fn new_tab(&mut self) {
        let cwd = self.focused_pane().cwd(&self.hostname);
        let (cols, rows) = {
            let t = &self.focused_pane().term;
            (t.cols() as u16, t.lines() as u16)
        };
        match self.spawn_pane(cols, rows, cwd, None) {
            Ok(id) => {
                self.tabs.insert(self.active + 1, Tab::new(id));
                self.active += 1;
                self.relayout();
            }
            Err(e) => self.notice(format!("cannot start shell: {e}")),
        }
    }

    /// Names of programs that closing these panes would kill.
    fn busy(&self, ids: &[PaneId]) -> Vec<String> {
        if !self.cfg.confirm_close {
            return Vec::new();
        }
        ids.iter().filter_map(|id| self.panes.get(id)?.busy(&self.cfg.confirm_close_ignore)).collect()
    }

    fn request_close(&mut self, ids: Vec<PaneId>, what: &str) {
        let busy = self.busy(&ids);
        if busy.is_empty() {
            for id in ids {
                self.close_pane(id);
            }
        } else {
            self.prompts.push_back(Prompt::Close(ids, format!("Close {what}? Running: {}", busy.join(", "))));
            self.dirty = true;
        }
    }

    fn request_quit(&mut self) {
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        let busy = self.busy(&ids);
        if busy.is_empty() {
            self.quit_requested = true;
            self.exit = true;
        } else if !self.prompts.iter().any(|p| matches!(p, Prompt::Quit(_))) {
            self.prompts.push_back(Prompt::Quit(format!("Close the window? Running: {}", busy.join(", "))));
            self.dirty = true;
        }
    }

    /// Remove a pane (its process exited or the user closed it). The last
    /// pane in a tab closes the tab; the last pane overall ends the program.
    fn close_pane(&mut self, id: PaneId) {
        let Some(ti) = self.tabs.iter().position(|t| t.root.contains(id)) else { return };
        if self.panes.len() == 1 {
            if let Some(p) = self.panes.get_mut(&id) {
                self.exit_code = p.finish();
            }
            self.exit = true;
            return;
        }
        let old = self.focused_id();
        if !self.tabs[ti].remove(id) {
            self.tabs.remove(ti);
            if self.active > ti {
                self.active -= 1;
            }
            self.active = self.active.min(self.tabs.len() - 1);
        }
        self.panes.remove(&id);
        self.prompts.retain(|p| match p {
            Prompt::ClipboardRead(p, ..) | Prompt::Paste(p, _) => *p != id,
            _ => true,
        });
        if self.mouse.selecting == Some(id) || self.mouse.reported.is_some_and(|r| r.0 == id) {
            self.mouse = Mouse { pos: self.mouse.pos, ..Mouse::default() };
        }
        self.relayout();
        self.after_focus(old);
    }

    /// Bookkeeping after the focused pane may have changed (`old` was
    /// focused before): focus reports, window title, cursor.
    fn after_focus(&mut self, old: PaneId) {
        let new = self.focused_id();
        if self.copy.as_ref().is_some_and(|c| c.pane != new) || (self.mode == Mode::Scroll && self.copy.is_none()) {
            self.set_mode(Mode::Normal);
        }
        if new != old && self.focused {
            if let Some(p) = self.panes.get_mut(&old)
                && let Some(b) = input::encode_focus(false, p.term.mode())
            {
                p.write(b);
            }
            let p = self.focused_pane_mut();
            if let Some(b) = input::encode_focus(true, p.term.mode()) {
                p.write(b);
            }
        }
        self.update_title();
        self.restart_blink();
        self.dirty = true;
    }

    fn run_action(&mut self, action: Action) {
        let (area, m) = (self.area(), self.layout_metrics());
        let old = self.focused_id();
        let n = self.tabs.len();
        let tab = &mut self.tabs[self.active];
        match action {
            // The key itself is sent by `key()`.
            Action::Mode(m) => self.set_mode(m),
            Action::Passthrough => {}
            Action::Focus(d) => {
                tab.focus_dir(area, m, d);
            }
            Action::FocusOrTab(d) => {
                if !tab.focus_dir(area, m, d) {
                    match d {
                        Dir::Left if self.active > 0 => self.active -= 1,
                        Dir::Right if self.active + 1 < n => self.active += 1,
                        _ => {}
                    }
                }
            }
            Action::Split(axis) => self.split(Some(axis)),
            Action::NewPane => self.split(None),
            Action::ClosePane => self.request_close(vec![old], "this pane"),
            Action::Zoom => {
                if tab.panes().len() > 1 {
                    tab.zoomed = !tab.zoomed;
                    self.relayout();
                }
            }
            Action::PaneToNewTab => {
                if tab.remove(old) {
                    self.tabs.insert(self.active + 1, Tab::new(old));
                    self.active += 1;
                    self.relayout();
                }
            }
            Action::Resize(d, cells) => {
                if tab.resize(area, m, d, cells) {
                    self.relayout();
                }
            }
            Action::Equalize => {
                tab.equalize();
                self.relayout();
            }
            Action::Swap(d) => {
                if tab.swap(area, m, d) {
                    self.relayout();
                }
            }
            Action::NewTab => self.new_tab(),
            Action::GotoTab(i) if (i as usize) < n => self.active = i as usize,
            Action::GotoTab(_) => {}
            Action::PrevTab => self.active = (self.active + n - 1) % n,
            Action::NextTab => self.active = (self.active + 1) % n,
            Action::CloseTab => {
                let ids = tab.panes();
                self.request_close(ids, "this tab");
            }
            Action::Scroll(s) => self.scroll(s),
            Action::Copy => {
                if let Some(text) = self.focused_pane().term.selection_text().filter(|t| !t.is_empty()) {
                    self.set_selection(ClipboardKind::Clipboard, text);
                }
            }
            Action::Paste => self.paste(ClipboardKind::Clipboard),
            Action::PastePrimary => self.paste(ClipboardKind::Primary),
            Action::Font(0) => self.set_font_size(self.cfg.font_size),
            Action::Font(d) => self.set_font_size(self.font_size + d as f32),
            Action::Quit => self.request_quit(),
            Action::CopyMove(m) => self.copy_move(m),
            Action::CopySelect(k) => self.copy_select(k),
            Action::CopyYank => self.copy_yank(),
            Action::CopyEscape => self.copy_escape(),
            Action::SearchStart => self.search_start(),
            Action::SearchUp => self.search_step(true),
            Action::SearchDown => self.search_step(false),
            Action::RenameTab => {
                let current = self.tabs[self.active].name.clone().unwrap_or_default();
                self.open_input(Purpose::RenameTab(old), &current, "empty: automatic title".into());
            }
            Action::RenamePane => {
                let current = self.focused_pane().name.clone().unwrap_or_default();
                self.open_input(Purpose::RenamePane(old), &current, "empty: automatic title".into());
            }
            Action::SaveLayout => {
                let hint = spec::layouts_dir().map(|d| format!("→ {}/NAME.lua", d.display())).unwrap_or_default();
                self.open_input(Purpose::SaveLayout, "", hint);
            }
            Action::LoadLayout => {
                let names = Self::layout_names();
                let hint = if names.is_empty() { "no saved layouts".into() } else { names.join("  ") };
                self.open_input(Purpose::LoadLayout, "", hint);
            }
        }
        if !self.exit {
            self.after_focus(old);
        }
    }

    fn scroll(&mut self, s: Scroll) {
        let t = &mut self.focused_pane_mut().term;
        let page = t.lines() as isize;
        match s {
            Scroll::Lines(n) => t.scroll_display(-n as isize),
            Scroll::HalfPages(n) => t.scroll_display(-(n as isize) * (page / 2).max(1)),
            Scroll::Pages(n) => t.scroll_display(-(n as isize) * (page - 1).max(1)),
            Scroll::Top => t.scroll_display(isize::MAX / 2),
            Scroll::Bottom => t.scroll_to_bottom(),
            Scroll::Prompt(forward) => {
                t.scroll_to_prompt(forward);
            }
        }
    }

    /// Debug aid: `PROTEUS_ACTIONS=pane.split_right,tab.new,...` runs
    /// actions once the window is up (snapshots of layouts).
    /// Debug aid: `PROTEUS_ACTIONS=pane.split_right,tab.new,...` runs
    /// actions once the window is up (`PROTEUS_ACTIONS_AFTER=secs` waits
    /// first). `type:TEXT` types into the input line (or the pane),
    /// `key:enter` / `key:esc` press a key in the input line, `key:y`
    /// answers a prompt.
    fn run_env_actions(&mut self) {
        let Ok(list) = std::env::var("PROTEUS_ACTIONS") else { return };
        let after = std::env::var("PROTEUS_ACTIONS_AFTER").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let run = move |app: &mut App| {
            for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if let Some(text) = name.strip_prefix("type:") {
                    // `\n` stands for Enter.
                    let text = &text.replace("\\n", "\r");
                    if app.input.is_some() {
                        app.input_text(text);
                    } else {
                        app.focused_pane_mut().write(text.as_bytes());
                    }
                    continue;
                }
                match (name, Action::from_name(name)) {
                    ("key:enter", _) => app.input_key(input::Key::Enter, input::Mods::empty(), ""),
                    ("key:esc", _) => app.input_key(input::Key::Escape, input::Mods::empty(), ""),
                    ("key:y", _) => app.answer_prompt(input::Key::Char('y')),
                    (_, Some(a)) => app.dispatch(a),
                    (_, None) => eprintln!("proteus: PROTEUS_ACTIONS: unknown action `{name}`"),
                }
            }
        };
        if after <= 0.0 {
            return run(self);
        }
        let mut run = Some(run);
        let _ = self.handle.insert_source(Timer::from_duration(Duration::from_secs_f64(after)), move |_, _, app| {
            if let Some(f) = run.take() {
                f(app);
            }
            TimeoutAction::Drop
        });
    }

    // --- PTY -----------------------------------------------------------------

    fn pty_output(&mut self, id: PaneId, bytes: &[u8]) {
        let focused = id == self.focused_id();
        let Some(p) = self.panes.get_mut(&id) else { return };
        p.term.advance(bytes);
        p.deliver_run(workspace::RUN_TIMEOUT);
        let reply = p.term.take_pty_output();
        if !reply.is_empty() {
            p.write(&reply);
        }
        let mut title = p.refresh_proc_title();
        for ev in p.term.take_events() {
            match ev {
                Event::TitleChanged(_) => title = true,
                Event::ClipboardStore { kind, text } => self.set_selection(kind, text),
                Event::ClipboardLoad { kind, terminator } => self.clipboard_load(id, kind, terminator),
                _ => {}
            }
        }
        if title {
            self.update_title();
            if self.tab_bar_visible() {
                self.dirty = true;
            }
        }
        if focused {
            self.ensure_blink();
        }
    }

    // --- keyboard ------------------------------------------------------------

    fn key(&mut self, event: KeyEvent, action: KeyAction) {
        if keys::is_modifier(event.keysym) {
            return;
        }
        let mods = keys::mods(&self.mods);
        let Some((key, shifted)) = self.levels.key(event.keysym, event.raw_code) else { return };
        if action == KeyAction::Release {
            // The press went to the multiplexer: so does the release.
            if let Some(i) = self.consumed.iter().position(|&c| c == event.raw_code) {
                self.consumed.swap_remove(i);
                return;
            }
        } else {
            self.restart_blink();
            if !self.prompts.is_empty() {
                self.consume(event.raw_code);
                if action == KeyAction::Press {
                    self.answer_prompt(key);
                }
                return;
            }
            if self.input.is_some() {
                self.consume(event.raw_code);
                self.input_key(key, mods, event.utf8.as_deref().unwrap_or(""));
                return;
            }
            match self.keymap.lookup_shifted(self.mode, key, mods, shifted) {
                Lookup::Pass => {}
                Lookup::Swallow => {
                    self.consume(event.raw_code);
                    return;
                }
                Lookup::Do(Action::Passthrough) => self.set_mode(self.keymap.after(self.mode, Action::Passthrough)),
                // Not meaningful here (Shift+PgUp in full-screen apps): the app gets the key.
                Lookup::Do(a) if !self.applies(a) => {}
                Lookup::Do(a) => {
                    self.consume(event.raw_code);
                    // Holding a key repeats movement, not structural changes.
                    let repeatable = matches!(
                        a,
                        Action::Focus(_)
                            | Action::FocusOrTab(_)
                            | Action::Resize(..)
                            | Action::Swap(_)
                            | Action::Scroll(_)
                            | Action::Font(_)
                            | Action::CopyMove(_)
                            | Action::SearchUp
                            | Action::SearchDown
                    );
                    if action == KeyAction::Press || repeatable {
                        self.dispatch(a);
                    }
                    return;
                }
            }
        }
        let text = event.utf8.as_deref().unwrap_or("");
        let ev = input::KeyEvent { key, mods, action, text, shifted };
        let p = self.focused_pane_mut();
        if let Some(bytes) = input::encode_key(&ev, p.term.mode(), p.term.keyboard_flags()) {
            if action != KeyAction::Release {
                p.term.scroll_to_bottom();
            }
            p.write(&bytes);
        }
    }

    /// Run a bound action and move to the mode that follows it (unless the
    /// action switched modes itself).
    fn dispatch(&mut self, a: Action) {
        let before = self.mode;
        let next = self.keymap.after(before, a);
        self.run_action(a);
        if self.mode == before && !self.exit {
            self.set_mode(next);
        }
        self.dirty = true;
    }

    fn consume(&mut self, raw_code: u32) {
        if !self.consumed.contains(&raw_code) {
            self.consumed.push(raw_code);
        }
    }

    /// Scrolling bound in Normal/Locked (Shift+PgUp, ...) applies to the
    /// scrollback only; full-screen applications get those keys.
    fn applies(&self, a: Action) -> bool {
        !(matches!(a, Action::Scroll(_)) && self.mode.passes_keys() && self.focused_pane().term.is_alt_screen())
    }

    // --- clipboard -------------------------------------------------------------

    fn set_selection(&mut self, kind: ClipboardKind, text: String) {
        match kind {
            ClipboardKind::Clipboard => {
                let (Some(mgr), Some(dev)) = (&self.data_manager, &self.data_device) else { return };
                let src = mgr.create_copy_paste_source(&self.qh, MIMES);
                src.set_selection(dev, self.serial);
                self.clipboard = Some((src, text));
            }
            ClipboardKind::Primary => {
                let (Some(mgr), Some(dev)) = (&self.primary_manager, &self.primary_device) else { return };
                let src = mgr.create_selection_source(&self.qh, MIMES);
                src.set_selection(dev, self.serial);
                self.primary = Some((src, text));
            }
        }
    }

    fn paste(&mut self, kind: ClipboardKind) {
        let id = self.focused_id();
        self.read_clipboard(kind, ClipTarget::Paste(id));
    }

    /// Fetch a selection's text; the result arrives as `Msg::Clipboard`
    /// (immediately if we own the selection).
    fn read_clipboard(&mut self, kind: ClipboardKind, target: ClipTarget) {
        let own = match kind {
            ClipboardKind::Clipboard => self.clipboard.as_ref().map(|c| c.1.clone()),
            ClipboardKind::Primary => self.primary.as_ref().map(|c| c.1.clone()),
        };
        if let Some(text) = own {
            self.clipboard_received(target, text);
            return;
        }
        let pick = |mimes: &[String]| MIMES.iter().find(|m| mimes.iter().any(|x| x == *m)).map(|m| m.to_string());
        let pipe = match kind {
            ClipboardKind::Clipboard => self.data_device.as_ref().and_then(|d| d.data().selection_offer()).and_then(|o| {
                let mime = o.with_mime_types(pick)?;
                o.receive(mime).ok().map(OwnedFd::from)
            }),
            ClipboardKind::Primary => self.primary_device.as_ref().and_then(|d| d.data().selection_offer()).and_then(|o| {
                let mime = o.with_mime_types(pick)?;
                o.receive(mime).ok().map(OwnedFd::from)
            }),
        };
        let Some(fd) = pipe else {
            // Nothing to read: an OSC 52 query still gets an (empty) answer.
            self.clipboard_received(target, String::new());
            return;
        };
        let tx = self.sender.clone();
        // Read off the main thread: the source may be slow (or be us).
        let _ = std::thread::Builder::new().name("paste".into()).spawn(move || {
            let mut data = Vec::new();
            let _ = File::from(fd).take(64 << 20).read_to_end(&mut data);
            let _ = tx.send(Msg::Clipboard(target, String::from_utf8_lossy(&data).into_owned()));
        });
    }

    fn clipboard_received(&mut self, target: ClipTarget, text: String) {
        match target {
            ClipTarget::Paste(id) => self.paste_text(id, text),
            ClipTarget::Osc52(id, kind, terminator) => {
                let Some(p) = self.panes.get_mut(&id) else { return };
                p.term.respond_clipboard(kind, &text, terminator);
                let reply = p.term.take_pty_output();
                p.write(&reply);
            }
        }
    }

    /// Paste, asking first if the text could run commands unseen.
    fn paste_text(&mut self, id: PaneId, text: String) {
        let Some(p) = self.panes.get(&id) else { return };
        if text.is_empty() {
            return;
        }
        if self.cfg.paste_protection && needs_paste_confirmation(&text, p.term.mode()) {
            if !self.prompts.iter().any(|p| matches!(p, Prompt::Paste(..))) {
                self.prompts.push_back(Prompt::Paste(id, text));
                self.dirty = true;
            }
            return;
        }
        self.paste_now(id, &text);
    }

    fn paste_now(&mut self, id: PaneId, text: &str) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        let bytes = input::encode_paste(text, p.term.mode());
        p.term.scroll_to_bottom();
        p.write(&bytes);
    }

    // --- prompts ---------------------------------------------------------------

    fn clipboard_load(&mut self, id: PaneId, kind: ClipboardKind, terminator: Terminator) {
        if self.cfg.osc52_read == ClipboardRead::Allow || self.osc52_always {
            self.read_clipboard(kind, ClipTarget::Osc52(id, kind, terminator));
        } else if !self.prompts.iter().any(|p| matches!(p, Prompt::ClipboardRead(..))) {
            // One question at a time; repeated queries while it is up are dropped.
            self.prompts.push_back(Prompt::ClipboardRead(id, kind, terminator));
            self.dirty = true;
        }
    }

    fn answer_prompt(&mut self, key: input::Key) {
        use input::Key;
        let Some(p) = self.prompts.pop_front() else { return };
        let yes = matches!(key, Key::Char('y' | 'Y'));
        let no = matches!(key, Key::Char('n' | 'N') | Key::Escape);
        match p {
            Prompt::ClipboardRead(id, kind, terminator) => match key {
                Key::Char('a' | 'A') => {
                    self.osc52_always = true;
                    self.read_clipboard(kind, ClipTarget::Osc52(id, kind, terminator));
                }
                _ if yes => self.read_clipboard(kind, ClipTarget::Osc52(id, kind, terminator)),
                Key::Enter => {}
                _ if no => {}
                _ => self.prompts.push_front(Prompt::ClipboardRead(id, kind, terminator)),
            },
            Prompt::Paste(id, text) => match key {
                Key::Enter => self.paste_now(id, &text),
                _ if yes => self.paste_now(id, &text),
                _ if no => {}
                _ => self.prompts.push_front(Prompt::Paste(id, text)),
            },
            Prompt::Close(ids, msg) => match key {
                Key::Enter => ids.into_iter().for_each(|id| self.close_pane(id)),
                _ if yes => ids.into_iter().for_each(|id| self.close_pane(id)),
                _ if no => {}
                _ => self.prompts.push_front(Prompt::Close(ids, msg)),
            },
            Prompt::Quit(msg) => match key {
                Key::Enter | Key::Char('y' | 'Y') => {
                    self.quit_requested = true;
                    self.exit = true;
                }
                _ if no => {}
                _ => self.prompts.push_front(Prompt::Quit(msg)),
            },
            Prompt::Restore(spec) => match key {
                Key::Enter => self.restore(&spec),
                _ if yes => self.restore(&spec),
                _ if no => {}
                _ => self.prompts.push_front(Prompt::Restore(spec)),
            },
            Prompt::Notice(_) => {}
        }
        self.dirty = true;
    }

    // --- cursor blink ----------------------------------------------------------

    fn wants_blink(&self) -> bool {
        let t = &self.focused_pane().term;
        let app = match self.cfg.cursor_blink {
            Blink::Never => false,
            Blink::Always => true,
            Blink::App => t.mode().contains(TermMode::CURSOR_BLINK),
        };
        app && self.focused && t.cursor_visible() && self.last_input.elapsed() < BLINK_IDLE
    }

    /// Input happened: show the cursor and restart the blink cycle.
    fn restart_blink(&mut self) {
        self.last_input = Instant::now();
        if self.blink_off {
            self.blink_off = false;
            self.dirty = true;
        }
        if let Some(t) = self.blink_timer.take() {
            self.handle.remove(t);
        }
        self.ensure_blink();
    }

    /// Run the blink timer while blinking is wanted; it stops itself (with
    /// the cursor on) when it no longer is, so an idle window never wakes.
    fn ensure_blink(&mut self) {
        if self.blink_timer.is_some() || !self.wants_blink() {
            return;
        }
        self.blink_timer = self
            .handle
            .insert_source(Timer::from_duration(BLINK), |_, _, app: &mut App| {
                if !app.exit && app.wants_blink() {
                    app.blink_off = !app.blink_off;
                    app.dirty = true;
                    TimeoutAction::ToDuration(BLINK)
                } else {
                    app.blink_timer = None;
                    if app.blink_off {
                        app.blink_off = false;
                        app.dirty = true;
                    }
                    TimeoutAction::Drop
                }
            })
            .ok();
    }

    fn send_selection(text: &str, fd: OwnedFd) {
        let text = text.to_string();
        let _ = std::thread::Builder::new().name("copy".into()).spawn(move || {
            let _ = File::from(fd).write_all(text.as_bytes());
        });
    }

    // --- mouse -----------------------------------------------------------------

    fn mouse_reporting(&self, id: PaneId) -> bool {
        self.panes.get(&id).is_some_and(|p| p.term.mode().intersects(TermMode::MOUSE_ANY)) && !self.mods.shift
    }

    fn report_mouse(&mut self, id: PaneId, action: MouseAction, button: Option<MouseButton>) -> bool {
        let Some((col, line, pixel)) = self.cell_in(id, self.mouse.pos) else { return false };
        let ev = MouseEvent { action, button, col, line, pixel: Some(pixel), mods: keys::mods(&self.mods) };
        let Some(p) = self.panes.get_mut(&id) else { return false };
        match input::encode_mouse(&ev, p.term.mode()) {
            Some(bytes) => {
                p.write(&bytes);
                true
            }
            None => false,
        }
    }

    /// Tab under a position in the tab bar.
    fn tab_at(&self, pos: (f64, f64)) -> Option<Option<usize>> {
        if !self.tab_bar_visible() {
            return None;
        }
        let (x, y) = self.phys(pos);
        if y >= self.area().y {
            return None;
        }
        let labels = self.tab_labels(&self.theme());
        Some(chrome::tab_at(&labels, x as f32, self.cell().0 as f32))
    }

    fn pointer_event(&mut self, ev: &PointerEvent) {
        self.mouse.pos = ev.position;
        match ev.kind {
            PointerEventKind::Enter { .. } => {
                self.mouse.icon = None;
                self.update_pointer_icon();
            }
            PointerEventKind::Leave { .. } => {}
            PointerEventKind::Motion { .. } => {
                if self.mouse.drag.is_some() {
                    self.drag_to(ev.position);
                    return;
                }
                self.update_pointer_icon();
                if let Some(id) = self.mouse.selecting {
                    let Some(p) = self.point_in(id, ev.position) else { return };
                    let block = self.mods.alt;
                    let Some(pane) = self.panes.get_mut(&id) else { return };
                    if let Some(anchor) = self.mouse.pending.take() {
                        if anchor != p {
                            let kind = if block { SelectionKind::Block } else { SelectionKind::Simple };
                            pane.term.start_selection(kind, anchor);
                        } else {
                            self.mouse.pending = Some(anchor);
                        }
                    }
                    if self.mouse.pending.is_none() {
                        pane.term.update_selection(p);
                    }
                } else if let Some((id, b)) = self.mouse.reported {
                    self.report_mouse(id, MouseAction::Motion, Some(b));
                } else if let Some(id) = self.pane_at(ev.position).filter(|&id| id == self.focused_id())
                    && self.mouse_reporting(id)
                {
                    self.report_mouse(id, MouseAction::Motion, None);
                }
            }
            PointerEventKind::Press { button, serial, .. } => {
                self.serial = serial;
                let b = match button {
                    BTN_LEFT => MouseButton::Left,
                    BTN_MIDDLE => MouseButton::Middle,
                    BTN_RIGHT => MouseButton::Right,
                    0x116 => MouseButton::Back,
                    0x115 => MouseButton::Forward,
                    _ => return,
                };
                if let Some(tab) = self.tab_at(ev.position) {
                    match (b, tab) {
                        (MouseButton::Left, Some(i)) => {
                            self.run_action(Action::GotoTab(i as u8));
                            self.mouse.drag = Some(Drag::Tab(i));
                        }
                        (MouseButton::Middle, Some(i)) => {
                            let ids = self.tabs[i].panes();
                            self.request_close(ids, "this tab");
                        }
                        _ => {}
                    }
                    return;
                }
                if b == MouseButton::Left
                    && let Some(d) = self.divider_at(ev.position)
                {
                    self.mouse.drag = Some(Drag::Divider(d));
                    return;
                }
                let Some(id) = self.pane_at(ev.position) else { return };
                if id != self.focused_id() {
                    let old = self.focused_id();
                    self.tabs[self.active].set_focus(id);
                    self.after_focus(old);
                }
                if self.mouse_reporting(id) && self.report_mouse(id, MouseAction::Press, Some(b)) {
                    self.mouse.reported = Some((id, b));
                    return;
                }
                match b {
                    MouseButton::Left => self.click(id, ev.position),
                    MouseButton::Middle => self.paste(ClipboardKind::Primary),
                    _ => {}
                }
            }
            PointerEventKind::Release { button, .. } => {
                if button == BTN_LEFT
                    && let Some(d) = self.mouse.drag.take()
                {
                    if matches!(d, Drag::Divider(_)) {
                        // The PTYs hear the final size only.
                        self.defer_pty_resize = false;
                        self.relayout();
                    }
                    return;
                }
                if let Some((id, b)) = self.mouse.reported.take() {
                    self.report_mouse(id, MouseAction::Release, Some(b));
                    return;
                }
                if button == BTN_LEFT
                    && let Some(id) = self.mouse.selecting.take()
                {
                    self.mouse.pending = None;
                    if let Some(text) = self.panes.get(&id).and_then(|p| p.term.selection_text()).filter(|t| !t.is_empty()) {
                        self.set_selection(ClipboardKind::Primary, text);
                    }
                }
            }
            PointerEventKind::Axis { vertical, .. } => {
                let notches = if vertical.value120 != 0 {
                    vertical.value120 as f64 / 120.0
                } else if vertical.discrete != 0 {
                    vertical.discrete as f64
                } else {
                    // Touchpad: pixels to lines.
                    let line_h = self.cell().1 as f64 / self.scale;
                    vertical.absolute / line_h / 3.0
                };
                self.mouse.scroll_acc += notches;
                let n = self.mouse.scroll_acc.trunc();
                self.mouse.scroll_acc -= n;
                if n == 0.0 {
                    return;
                }
                let n = n as i32;
                if self.tab_at(ev.position).is_some() {
                    self.run_action(if n < 0 { Action::PrevTab } else { Action::NextTab });
                    return;
                }
                let Some(id) = self.pane_at(ev.position) else { return };
                if self.mouse_reporting(id) {
                    let b = if n < 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
                    for _ in 0..n.unsigned_abs().min(10) {
                        self.report_mouse(id, MouseAction::Press, Some(b));
                    }
                    return;
                }
                let Some(p) = self.panes.get_mut(&id) else { return };
                if let Some(bytes) = input::alternate_scroll(-n * 3, p.term.mode()) {
                    p.write(&bytes);
                } else {
                    p.term.scroll_display((-n * 3) as isize);
                }
            }
        }
    }

    fn divider_at(&self, pos: (f64, f64)) -> Option<layout::Divider> {
        let (x, y) = self.phys(pos);
        let slop = (3.0 * self.scale).round() as i32;
        self.view().divider_at(x, y, slop).cloned()
    }

    /// Mouse moved while dragging a divider or a tab.
    fn drag_to(&mut self, pos: (f64, f64)) {
        let (x, y) = self.phys(pos);
        match &self.mouse.drag {
            Some(Drag::Divider(d)) => {
                let at = if d.axis == Axis::X { x } else { y };
                let m = self.layout_metrics();
                let d = d.clone();
                if self.tabs[self.active].drag_divider(&d, at, m) {
                    self.defer_pty_resize = true;
                    self.relayout();
                }
            }
            Some(Drag::Tab(i)) => {
                let i = *i;
                if let Some(Some(j)) = self.tab_at(pos)
                    && j != i
                    && i < self.tabs.len()
                {
                    let t = self.tabs.remove(i);
                    self.tabs.insert(j, t);
                    self.active = j;
                    self.mouse.drag = Some(Drag::Tab(j));
                    self.dirty = true;
                }
            }
            None => {}
        }
    }

    /// Resize arrows over dividers, an arrow over the tab bar, else a text cursor.
    fn update_pointer_icon(&mut self) {
        let icon = match self.divider_at(self.mouse.pos) {
            Some(d) if d.axis == Axis::X => CursorIcon::ColResize,
            Some(_) => CursorIcon::RowResize,
            None if self.tab_at(self.mouse.pos).is_some() => CursorIcon::Default,
            None if self.pane_at(self.mouse.pos).is_none() => CursorIcon::Default,
            None => CursorIcon::Text,
        };
        if self.mouse.icon != Some(icon) {
            self.mouse.icon = Some(icon);
            if let Some(p) = &self.pointer {
                let _ = p.set_cursor(&self.conn, icon);
            }
        }
    }

    fn click(&mut self, id: PaneId, pos: (f64, f64)) {
        let Some(p) = self.point_in(id, pos) else { return };
        let now = Instant::now();
        let count = match self.mouse.last_click {
            Some((t, i, q, n)) if now.duration_since(t) < DOUBLE_CLICK && i == id && q == p => n % 3 + 1,
            _ => 1,
        };
        self.mouse.last_click = Some((now, id, p, count));
        self.mouse.selecting = Some(id);
        let Some(pane) = self.panes.get_mut(&id) else { return };
        match count {
            1 => {
                pane.term.clear_selection();
                self.mouse.pending = Some(p);
            }
            n => {
                self.mouse.pending = None;
                let kind = if n == 2 { SelectionKind::Word } else { SelectionKind::Line };
                pane.term.start_selection(kind, p);
            }
        }
    }
}

// --- Wayland handlers ------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, factor: i32) {
        // Integer scale is only used without fractional-scale support.
        if self.fractional.is_none() {
            surface.set_buffer_scale(factor);
            self.set_scale(factor as f64);
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {
        self.frame_pending = false;
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.request_quit();
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window, cfg: WindowConfigure, _: u32) {
        let (w, h) = cfg.new_size;
        let default = if self.configured {
            self.size
        } else {
            // First configure without a size: 80x24 cells at the current scale.
            let m = self.fonts.metrics();
            let pad = self.pad() * 2;
            (
                ((80 * m.cell_w + pad) as f64 / self.scale).round() as u32,
                ((24 * m.cell_h + pad) as f64 / self.scale).round() as u32,
            )
        };
        self.size = (w.map_or(default.0, |v| v.get()), h.map_or(default.1, |v| v.get()));
        let focused = cfg.is_activated();
        if focused != self.focused && self.keyboard.is_none() {
            self.focused = focused;
        }
        let first = !self.configured;
        self.configured = true;
        self.apply_size();
        if first {
            self.run_env_actions();
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat) {
        if self.data_device.is_none()
            && let Some(m) = &self.data_manager
        {
            self.data_device = Some(m.get_data_device(qh, &seat));
        }
        if self.primary_device.is_none()
            && let Some(m) = &self.primary_manager
        {
            self.primary_device = Some(m.get_selection_device(qh, &seat));
        }
    }
    fn new_capability(&mut self, conn: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, cap: Capability) {
        self.new_seat(conn, qh, seat.clone());
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            let repeat = Box::new(|app: &mut App, _: &WlKeyboard, ev: KeyEvent| app.key(ev, KeyAction::Repeat));
            match self.seat_state.get_keyboard_with_repeat(qh, &seat, None, self.handle.clone(), repeat) {
                Ok(k) => self.keyboard = Some(k),
                Err(e) => eprintln!("proteus: keyboard: {e}"),
            }
            if self.ime.is_none()
                && let Some(m) = &self.text_input_manager
            {
                self.ime = Some(ime::Ime::new(m.get_text_input(&seat, qh, ime::ImeData)));
            }
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            let surface = self.compositor.create_surface(qh);
            match self.seat_state.get_pointer_with_theme::<App, ()>(qh, &seat, self.shm.wl_shm(), surface, ThemeSpec::default()) {
                Ok(p) => self.pointer = Some(p),
                Err(e) => eprintln!("proteus: pointer: {e}"),
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, cap: Capability) {
        if cap == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
            if let Some(i) = self.ime.take() {
                i.destroy();
                self.dirty = true;
            }
        }
        if cap == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.pointer().release();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, s: &WlSurface, serial: u32, _: &[u32], _: &[Keysym]) {
        if s == self.window.wl_surface() {
            self.serial = serial;
            self.focused = true;
            self.dirty = true;
            self.restart_blink();
            let p = self.focused_pane_mut();
            if let Some(b) = input::encode_focus(true, p.term.mode()) {
                p.write(b);
            }
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, s: &WlSurface, _: u32) {
        if s == self.window.wl_surface() {
            self.focused = false;
            self.dirty = true;
            let p = self.focused_pane_mut();
            if let Some(b) = input::encode_focus(false, p.term.mode()) {
                p.write(b);
            }
        }
    }
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, serial: u32, ev: KeyEvent) {
        self.serial = serial;
        self.key(ev, KeyAction::Press);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, ev: KeyEvent) {
        self.key(ev, KeyAction::Repeat);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, ev: KeyEvent) {
        self.key(ev, KeyAction::Release);
    }
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, m: Modifiers, _: RawModifiers, layout: u32) {
        self.mods = m;
        self.levels.layout = layout;
    }
    fn update_keymap(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, keymap: Keymap<'_>) {
        self.levels.set_keymap(keymap.as_string());
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlPointer, events: &[PointerEvent]) {
        for ev in events {
            if &ev.surface == self.window.wl_surface() {
                self.pointer_event(ev);
            }
        }
    }
}

impl DataDeviceHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64, _: &WlSurface) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for App {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl DataSourceHandler for App {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, src: &WlDataSource, mime: String, fd: WritePipe) {
        if let Some((_, text)) = self.clipboard.as_ref().filter(|(s, _)| s.inner() == src)
            && MIMES.contains(&mime.as_str())
        {
            Self::send_selection(text, fd.into());
        }
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, src: &WlDataSource) {
        if self.clipboard.as_ref().is_some_and(|(s, _)| s.inner() == src) {
            self.clipboard = None;
        }
        src.destroy();
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl PrimarySelectionDeviceHandler for App {
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ZwpPrimarySelectionDeviceV1) {}
}

impl PrimarySelectionSourceHandler for App {
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, src: &ZwpPrimarySelectionSourceV1, mime: String, fd: WritePipe) {
        if let Some((_, text)) = self.primary.as_ref().filter(|(s, _)| s.inner() == src)
            && MIMES.contains(&mime.as_str())
        {
            Self::send_selection(text, fd.into());
        }
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, src: &ZwpPrimarySelectionSourceV1) {
        if self.primary.as_ref().is_some_and(|(s, _)| s.inner() == src) {
            self.primary = None;
        }
        src.destroy();
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
delegate_dispatch2!(App);

/// Whether pasting `text` should be confirmed: outside bracketed paste a
/// newline runs whatever precedes it, and control characters can drive the
/// application.
fn needs_paste_confirmation(text: &str, mode: TermMode) -> bool {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        return false;
    }
    text.chars().any(|c| c.is_control() && c != '\t')
}

impl Prompt {
    fn overlay(&self) -> Overlay {
        match self {
            Prompt::ClipboardRead(_, kind, _) => {
                let which = if matches!(kind, ClipboardKind::Primary) { "primary selection" } else { "clipboard" };
                Overlay {
                    title: format!("The application wants to read the {which}."),
                    detail: None,
                    keys: "[y] allow once   [a] always (this session)   [n] deny".into(),
                }
            }
            Prompt::Paste(_, text) => {
                let lines = text.trim_end_matches('\n').split('\n').count();
                let control = text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'));
                let title = match (lines, control) {
                    (1, false) => "Paste and run this line?".to_string(),
                    (1, true) => "Paste text containing control characters?".to_string(),
                    (n, false) => format!("Paste {n} lines? Each newline may run a command."),
                    (n, true) => format!("Paste {n} lines containing control characters?"),
                };
                let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                let preview: String = first.chars().map(|c| if c.is_control() { '\u{fffd}' } else { c }).take(200).collect();
                Overlay { title, detail: Some(preview), keys: "[Enter] paste   [Esc] cancel".into() }
            }
            Prompt::Restore(spec) => Overlay {
                title: format!(
                    "Restore the previous session? ({} tabs, {} panes; it did not exit normally)",
                    spec.tabs.len(),
                    spec.pane_count()
                ),
                detail: None,
                keys: "[y] restore   [n] start fresh".into(),
            },
            Prompt::Close(_, msg) | Prompt::Quit(msg) => {
                Overlay { title: msg.clone(), detail: None, keys: "[y] close   [n] cancel".into() }
            }
            Prompt::Notice(msg) => Overlay { title: msg.clone(), detail: None, keys: "press any key to dismiss".into() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_protection_rules() {
        let plain = TermMode::default();
        assert!(!needs_paste_confirmation("ls -la", plain));
        // Even one trailing newline would run the command.
        assert!(needs_paste_confirmation("ls -la\n", plain));
        assert!(!needs_paste_confirmation("a\tb", plain));
        assert!(needs_paste_confirmation("rm -rf ~\nls\n", plain));
        assert!(needs_paste_confirmation("x\x1b[31m", plain));
        assert!(needs_paste_confirmation("x\ry", plain));
        assert!(!needs_paste_confirmation("a\nb", plain | TermMode::BRACKETED_PASTE));
    }

    #[test]
    fn prompt_texts() {
        let o = Prompt::Paste(1, "echo 1\necho 2\n".into()).overlay();
        assert!(o.title.contains("2 lines"), "{o:?}");
        assert_eq!(o.detail.as_deref(), Some("echo 1"));
        let o = Prompt::Paste(1, "a\x1bb".into()).overlay();
        assert!(o.title.contains("control"), "{o:?}");
        assert!(o.detail.unwrap().contains('\u{fffd}'));
    }
}
