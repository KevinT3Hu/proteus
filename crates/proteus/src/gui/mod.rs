//! The Wayland frontend: one xdg-toplevel window with tabs of split panes,
//! each pane a terminal session, rendered with wgpu.
//!
//! Event loop: calloop drives the Wayland connection, a channel fed by the PTY
//! reader thread, and timers. Frames are drawn only when something changed
//! and only after the compositor's frame callback (no busy loop).

mod blocks;
mod chrome;
mod copy;
mod font;
mod hint;
mod images;
mod ime;
mod keys;
mod notify;
mod pane;
mod render;
mod rows;
mod scroll;
mod textinput;
mod viewer;
mod workspace;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
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
use smithay_client_toolkit::activation::{ActivationHandler, ActivationState, RequestData};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler, DragSource};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler};
use smithay_client_toolkit::primary_selection::selection::{PrimarySelectionSource, PrimarySelectionSourceHandler};
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

use crate::config::{Blink, Config, HintBar, PaneTitles, TabBar};
use crate::layout::{self, Axis, Dir, Layout, PaneId, Rect, Tab};
use crate::modes::{Action, Lookup, Mode, Scroll};
use crate::session::SpawnOptions;
use crate::spec::{self, LayoutSpec};
use font::Fonts;
use pane::Pane;
use render::{FrameInfo, Overlay, PaneFrame, Renderer, Theme};
use textinput::{Input, Purpose};
use workspace::SessionStore;

const APP_ID: &str = "dev.proteus.Proteus";
const APP_NAME: &str = "Proteus";
/// How long the visual bell tints a pane.
const FLASH: Duration = Duration::from_millis(120);
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
    /// A desktop notification about this pane was clicked (with an
    /// xdg-activation token, if the daemon sent one).
    NotifyActivated(PaneId, Option<String>),
    /// Images of this pane finished decoding in the background.
    ImagesDecoded(PaneId),
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
    fn event(
        &self,
        _: &mut App,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}
impl Dispatch2<WpViewporter, App> for ScaleData {
    fn event(
        &self,
        _: &mut App,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}
impl Dispatch2<WpViewport, App> for ScaleData {
    fn event(
        &self,
        _: &mut App,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}
/// User data of a window's fractional-scale object.
struct WinScale(WinId);

impl Dispatch2<WpFractionalScaleV1, App> for WinScale {
    fn event(
        &self,
        app: &mut App,
        _: &WpFractionalScaleV1,
        ev: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = ev
            && app.activate(self.0)
        {
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
    /// Floating pane moved by its frame; the grab point relative to its grid.
    FloatMove(PaneId, (i32, i32)),
    /// Floating pane resized by its corner.
    FloatResize(PaneId),
    /// Image in the viewer panned; the last pointer position.
    Viewer((f64, f64)),
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
    floating: bool,
    backdrop: Vec<([f32; 4], proteus_core::color::Rgb)>,
    labels: Vec<render::Label>,
    rows: rows::Rows,
    flash: bool,
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
                    floating: p.floating,
                    backdrop: p.backdrop,
                    labels: p.labels,
                    rows: p.rows,
                    flash: p.flash,
                })
            })
            .collect();
        info
    }
}

/// Identifies a window across swaps (see [`App::activate`]).
pub(crate) type WinId = u32;

/// Everything that belongs to one window. The window being worked on lives
/// in [`App::w`]; the others are parked in [`App::parked`] and swapped in
/// when an event concerns them.
struct Win {
    // Drop order matters: the wgpu surface must go before the window.
    renderer: Renderer,
    window: Window,
    id: WinId,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    fonts: Fonts,
    font_size: f32,
    scale: f64,
    /// Logical window size.
    size: (u32, u32),
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    sync_since: Option<Instant>,
    /// When the pending redraw timer fires (see [`App::schedule`]).
    wake_at: Option<Instant>,
    /// Has keyboard focus.
    focused: bool,
    /// Never empty while the window is open.
    tabs: Vec<Tab>,
    active: usize,
    mode: Mode,
    prompts: VecDeque<Prompt>,
    /// The bottom-bar text input, when open; it takes the keys.
    input: Option<Input>,
    copy: Option<scroll::CopyState>,
    search: Option<scroll::SearchState>,
    window_title: String,
    mouse: Mouse,
    /// A divider is being dragged: PTY resizes wait for the release.
    defer_pty_resize: bool,
    /// Block whose gutter mark the pointer is over.
    hover: Option<(PaneId, proteus_core::blocks::BlockId)>,
    /// Hint mode: labels are shown and keys pick one.
    hints: Option<hint::HintState>,
    /// The image viewer, over everything; it takes keys and the mouse.
    viewer: Option<viewer::Viewer>,
}

/// A tab being dragged to another window (Wayland drag and drop).
struct TabDnd {
    source: DragSource,
    from: WinId,
    /// A pane of the tab (tab indices shift; pane ids do not).
    pane: PaneId,
    /// Dropped on one of our windows (else: a new window).
    done: bool,
    /// Window and logical position the drag is over.
    at: Option<(WinId, (f64, f64))>,
}

const TAB_MIME: &str = "application/x-proteus-tab";

struct App {
    /// The current window.
    w: Win,
    parked: Vec<Win>,
    next_win: WinId,
    /// Window with keyboard focus (keys and IME go there).
    kbd_win: Option<WinId>,
    instance: wgpu::Instance,
    xdg: XdgShell,
    viewporter: Option<WpViewporter>,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    data_manager: Option<DataDeviceManagerState>,
    primary_manager: Option<PrimarySelectionManagerState>,
    text_input_manager: Option<ZwpTextInputManagerV3>,
    ime: Option<ime::Ime>,
    conn: Connection,
    qh: QueueHandle<App>,
    handle: LoopHandle<'static, App>,
    sender: Sender<Msg>,

    cfg: Config,
    keymap: crate::modes::Keymap,
    /// OSC 52 reads allowed for the rest of the session ("always").
    osc52_always: bool,
    /// Blink phase: cursor hidden.
    blink_off: bool,
    blink_timer: Option<RegistrationToken>,
    last_input: Instant,

    panes: HashMap<PaneId, Pane>,
    next_id: PaneId,
    hostname: String,
    exit_code: i32,
    session: Option<SessionStore>,
    /// The window was closed by the user (not by the last pane ending).
    quit_requested: bool,
    /// The timer delivering layouts' `run` lines is active.
    run_timer: bool,
    tab_dnd: Option<TabDnd>,
    notifier: notify::Notifier,
    activation: Option<ActivationState>,
    hint_patterns: Vec<proteus_core::hints::HintPattern>,
    /// Palette entries used recently, most recent first.
    recent: Vec<String>,
    /// Decoded image memory shared by all panes (`graphics.total_memory`).
    image_budget: std::sync::Arc<proteus_core::ImageBudget>,

    keyboard: Option<WlKeyboard>,
    pointer: Option<ThemedPointer>,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    mods: Modifiers,
    levels: keys::Levels,
    /// Keys whose press the multiplexer consumed; their release is dropped too.
    consumed: Vec<u32>,
    serial: u32,
    /// Serial of the last pointer button press (starts drags).
    press_serial: u32,
    clipboard: Option<(CopyPasteSource, String)>,
    /// An image (PNG) we offer on the clipboard instead of text.
    clipboard_image: Option<(CopyPasteSource, std::sync::Arc<Vec<u8>>)>,
    primary: Option<(PrimarySelectionSource, String)>,
    exit: bool,
}

/// Wayland and GPU objects of a new window.
#[allow(clippy::too_many_arguments)]
fn make_win(
    id: WinId,
    conn: &Connection,
    qh: &QueueHandle<App>,
    compositor: &CompositorState,
    xdg: &XdgShell,
    viewporter: Option<&WpViewporter>,
    fractional_manager: Option<&WpFractionalScaleManagerV1>,
    instance: &wgpu::Instance,
    cfg: &Config,
    logical: Option<(u32, u32)>,
) -> io::Result<Win> {
    let err = |e: String| io::Error::other(e);
    let surface = compositor.create_surface(qh);
    let (viewport, fractional) = match (viewporter, fractional_manager) {
        (Some(vp), Some(fm)) => {
            (Some(vp.get_viewport(&surface, qh, ScaleData)), Some(fm.get_fractional_scale(&surface, qh, WinScale(id))))
        }
        _ => (None, None),
    };
    let window = xdg.create_window(surface, WindowDecorations::RequestServer, qh);
    window.set_title("proteus");
    window.set_app_id(APP_ID);
    window.set_min_size(Some((100, 60)));
    window.commit();

    let fonts = Fonts::new(&cfg.font_family, cfg.font_size, 1.0).map_err(err)?;
    let m = fonts.metrics();
    let pad = (PAD * 2.0) as u32;
    let size = logical.unwrap_or((80 * m.cell_w + pad, 24 * m.cell_h + pad));
    let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(conn.backend().display_ptr().cast()).ok_or_else(|| err("no Wayland display".into()))?,
    ));
    let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(window.wl_surface().id().as_ptr().cast()).ok_or_else(|| err("no surface".into()))?,
    ));
    // SAFETY: the display (kept alive by `conn`) and the surface (owned by
    // `window`) outlive the wgpu surface, which `Win` drops first.
    let wsurface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_display),
            raw_window_handle: raw_window,
        })
    }
    .map_err(|e| err(format!("cannot create GPU surface: {e}")))?;
    let mut renderer = Renderer::new(instance, wsurface, size).map_err(err)?;
    renderer.images.budget = cfg.graphics.gpu_memory;
    Ok(Win {
        renderer,
        window,
        id,
        viewport,
        fractional,
        fonts,
        font_size: cfg.font_size,
        scale: 1.0,
        size,
        configured: false,
        frame_pending: false,
        dirty: true,
        sync_since: None,
        wake_at: None,
        focused: false,
        tabs: Vec::new(),
        active: 0,
        mode: Mode::Normal,
        prompts: VecDeque::new(),
        input: None,
        copy: None,
        search: None,
        window_title: String::new(),
        mouse: Mouse::default(),
        defer_pty_resize: false,
        hover: None,
        hints: None,
        viewer: None,
    })
}

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

    let display = Display(conn.clone());
    let mut desc = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display)).with_env();
    if wgpu::Backends::from_env().is_none() {
        desc.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
    }
    let instance = wgpu::Instance::new(desc);
    let first = make_win(
        1,
        &conn,
        &qh,
        &compositor,
        &xdg,
        viewporter.as_ref(),
        fractional_manager.as_ref(),
        &instance,
        &cfg,
        None,
    )?;
    if std::env::var_os("PROTEUS_DEBUG").is_some() {
        eprintln!("proteus: rendering with {}", first.renderer.adapter_info);
        eprintln!("proteus: text-input-v3 {}", if text_input_manager.is_some() { "available" } else { "unavailable" });
    }

    let (sender, rx) = channel::channel();
    handle
        .insert_source(rx, |ev, _, app: &mut App| match ev {
            channel::Event::Msg(Msg::Pty(id, bytes)) => app.pty_output(id, &bytes),
            channel::Event::Msg(Msg::PtyClosed(id)) => app.close_pane(id),
            channel::Event::Msg(Msg::Clipboard(target, text)) => app.clipboard_received(target, text),
            channel::Event::Msg(Msg::NotifyActivated(pane, token)) => app.notification_clicked(pane, token),
            channel::Event::Msg(Msg::ImagesDecoded(id)) => app.pty_output(id, &[]),
            channel::Event::Closed => app.exit = true,
        })
        .map_err(|e| err(e.to_string()))?;

    let notifier = notify::Notifier::new(sender.clone());
    let activation = ActivationState::bind(&globals, &qh).ok();
    let hint_patterns = hint::patterns(&cfg.hint_patterns);
    let cfg_total_memory = cfg.graphics.total_memory;
    let mut app = App {
        w: first,
        parked: Vec::new(),
        next_win: 2,
        kbd_win: None,
        instance,
        xdg,
        viewporter,
        fractional_manager,
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        shm,
        data_manager: DataDeviceManagerState::bind(&globals, &qh).ok(),
        primary_manager: PrimarySelectionManagerState::bind(&globals, &qh).ok(),
        text_input_manager,
        ime: None,
        conn: conn.clone(),
        qh,
        handle,
        sender,
        keymap: cfg.keymap(),
        cfg,
        osc52_always: false,
        blink_off: false,
        blink_timer: None,
        last_input: Instant::now(),
        panes: HashMap::new(),
        next_id: 1,
        hostname: rustix::system::uname().nodename().to_string_lossy().into_owned(),
        exit_code: 0,
        session: None,
        quit_requested: false,
        run_timer: false,
        tab_dnd: None,
        notifier,
        activation,
        hint_patterns,
        recent: scroll::load_recent(),
        image_budget: proteus_core::ImageBudget::new(cfg_total_memory),
        keyboard: None,
        pointer: None,
        data_device: None,
        primary_device: None,
        mods: Modifiers::default(),
        levels: keys::Levels::new(),
        consumed: Vec::new(),
        serial: 0,
        press_serial: 0,
        clipboard: None,
        clipboard_image: None,
        primary: None,
        exit: false,
    };
    if let Some(e) = config_error {
        app.notice(format!("config error: {e}"));
    }

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
                app.w.prompts.push_back(Prompt::Restore(s));
            }
        }
    }
    if app.w.tabs.is_empty() {
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
        app.draw_all();
        let _ = app.conn.flush();
    }
    // Debug aid: PROTEUS_SNAPSHOT=file.png saves the final frame of every
    // window (`file.png`, `file-2.png`, ...).
    if let Some(path) = std::env::var_os("PROTEUS_SNAPSHOT") {
        let path = PathBuf::from(path);
        let mut ids: Vec<WinId> = std::iter::once(app.w.id).chain(app.parked.iter().map(|w| w.id)).collect();
        ids.sort();
        for (i, id) in ids.into_iter().enumerate() {
            app.activate(id);
            let target = if i == 0 {
                path.clone()
            } else {
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                path.with_file_name(format!("{stem}-{}.png", i + 1))
            };
            app.snapshot(&target);
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
    // --- windows -------------------------------------------------------------

    /// Make window `id` the current one ([`App::w`]).
    fn activate(&mut self, id: WinId) -> bool {
        if self.w.id == id {
            return true;
        }
        match self.parked.iter().position(|w| w.id == id) {
            Some(i) => {
                std::mem::swap(&mut self.w, &mut self.parked[i]);
                true
            }
            None => false,
        }
    }

    fn win_of_surface(&self, s: &WlSurface) -> Option<WinId> {
        std::iter::once(&self.w).chain(&self.parked).find(|w| w.window.wl_surface() == s).map(|w| w.id)
    }

    fn activate_surface(&mut self, s: &WlSurface) -> bool {
        match self.win_of_surface(s) {
            Some(id) => self.activate(id),
            None => false,
        }
    }

    /// Window holding a pane.
    fn win_of_pane(&self, pane: PaneId) -> Option<WinId> {
        std::iter::once(&self.w).chain(&self.parked).find(|w| w.tabs.iter().any(|t| t.contains(pane))).map(|w| w.id)
    }

    /// Make the window holding `pane` current. Returns false if no window does.
    fn activate_pane(&mut self, pane: PaneId) -> bool {
        match self.win_of_pane(pane) {
            Some(id) => self.activate(id),
            None => false,
        }
    }

    /// Back to the keyboard window (where keys go).
    fn activate_kbd(&mut self) {
        if let Some(id) = self.kbd_win {
            self.activate(id);
        }
    }

    fn window_ids(&self) -> Vec<WinId> {
        let mut v: Vec<WinId> = std::iter::once(self.w.id).chain(self.parked.iter().map(|w| w.id)).collect();
        v.sort();
        v
    }

    fn mark_all_dirty(&mut self) {
        self.w.dirty = true;
        for w in &mut self.parked {
            w.dirty = true;
        }
    }

    /// Open a new (empty) window and make it current. Its size follows the
    /// current window.
    fn new_window(&mut self) -> io::Result<WinId> {
        let id = self.next_win;
        self.next_win += 1;
        let win = make_win(
            id,
            &self.conn,
            &self.qh,
            &self.compositor,
            &self.xdg,
            self.viewporter.as_ref(),
            self.fractional_manager.as_ref(),
            &self.instance,
            &self.cfg,
            Some(self.w.size),
        )?;
        self.parked.push(win);
        self.activate(id);
        Ok(id)
    }

    /// Close the current window (it must have no tabs left). The last
    /// window ends the program.
    fn destroy_window(&mut self) {
        if self.parked.is_empty() {
            self.exit = true;
            return;
        }
        let next = self.kbd_win.filter(|&k| k != self.w.id && self.parked.iter().any(|w| w.id == k));
        let i = match next {
            Some(k) => self.parked.iter().position(|w| w.id == k).unwrap_or(0),
            None => 0,
        };
        let old = std::mem::replace(&mut self.w, self.parked.remove(i));
        if self.kbd_win == Some(old.id) {
            self.kbd_win = None;
        }
        drop(old);
        self.mark_all_dirty();
    }

    /// Draw every window that needs it, then return to the keyboard window.
    fn draw_all(&mut self) {
        for id in self.window_ids() {
            if self.activate(id) {
                self.draw();
            }
        }
        self.activate_kbd();
    }

    fn snapshot(&mut self, path: &std::path::Path) {
        let mut parts = self.frame_parts();
        // PROTEUS_PREEDIT=text[@byte] previews a composition (testing aid).
        if let Some(pre) = std::env::var("PROTEUS_PREEDIT").ok().filter(|s| !s.is_empty()) {
            let (text, cursor) = match pre.rsplit_once('@') {
                Some((t, c)) => (t.to_string(), c.parse().ok().map(|c: usize| (c, c))),
                None => (pre.clone(), Some((pre.len(), pre.len()))),
            };
            let fid = self.focused_id();
            for p in parts.panes.iter_mut().filter(|p| p.id == fid) {
                p.focused = true;
                p.preedit = Some(ime::Preedit { text: text.clone(), cursor });
            }
        }
        let area = self.viewer_area();
        let mut info = parts.resolve(&self.panes);
        info.viewer = self.w.viewer.as_ref().map(|v| v.frame(area));
        let (w, h, px) = self.w.renderer.snapshot(&mut self.w.fonts, &info);
        drop(info);
        if let Err(e) = image::save_buffer(path, &px, w, h, image::ColorType::Rgba8) {
            eprintln!("proteus: snapshot: {e}");
        }
    }

    // --- geometry ------------------------------------------------------------

    fn pad(&self) -> u32 {
        (PAD * self.w.scale).round() as u32
    }

    fn physical(&self) -> (u32, u32) {
        ((self.w.size.0 as f64 * self.w.scale).round() as u32, (self.w.size.1 as f64 * self.w.scale).round() as u32)
    }

    fn cell(&self) -> (u32, u32) {
        let m = self.w.fonts.metrics();
        (m.cell_w, m.cell_h)
    }

    /// Layout sizes for the active tab.
    fn layout_metrics(&self) -> layout::Metrics {
        match self.w.tabs.get(self.w.active) {
            Some(t) => self.tab_metrics(t),
            None => {
                let (w, h) = self.cell();
                layout::Metrics { cell_w: w as i32, cell_h: h as i32, gap: w as i32, titles: false }
            }
        }
    }

    fn tab_metrics(&self, t: &Tab) -> layout::Metrics {
        let (w, h) = self.cell();
        let titles = match self.cfg.pane_titles {
            PaneTitles::Always => true,
            PaneTitles::Never => false,
            PaneTitles::Auto => matches!(t.root, layout::Node::Split { .. }),
        };
        layout::Metrics { cell_w: w as i32, cell_h: h as i32, gap: w as i32, titles }
    }

    fn tab_bar_visible(&self) -> bool {
        match self.cfg.tab_bar {
            TabBar::Always => true,
            TabBar::Never => false,
            TabBar::Auto => self.w.tabs.len() > 1,
        }
    }

    fn hint_bar_visible(&self) -> bool {
        if self.w.input.is_some() || self.w.hints.is_some() || self.w.viewer.is_some() {
            return true;
        }
        match self.cfg.hint_bar {
            HintBar::Always => true,
            HintBar::Never => false,
            HintBar::InMode => self.w.mode != Mode::Normal,
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
        // The block gutter of the leftmost panes lives in the padding.
        let left = p.max(self.gutter_px() + 1);
        Rect::new(left, top, (pw as i32 - left - p).max(1), (bottom - top).max(1))
    }

    /// Visible panes of the active tab.
    fn view(&self) -> Layout {
        self.w.tabs[self.w.active].layout(self.area(), self.layout_metrics())
    }

    fn focused_id(&self) -> PaneId {
        self.w.tabs[self.w.active].focus
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
        if let Some(vp) = &self.w.viewport {
            vp.set_destination(self.w.size.0 as i32, self.w.size.1 as i32);
        }
        self.w.renderer.resize(pw, ph);
        self.relayout();
    }

    /// Fit every pane of every tab to its rectangle.
    fn relayout(&mut self) {
        let (area, cell) = (self.area(), self.cell());
        for t in &self.w.tabs {
            let m = self.tab_metrics(t);
            let l = t.layout(area, m);
            for (id, r) in l.panes.into_iter().chain(l.floats.iter().map(|f| (f.id, f.content))) {
                if let Some(p) = self.panes.get_mut(&id) {
                    let cols = (r.w / m.cell_w).max(2) as u16;
                    let rows = (r.h / m.cell_h).max(1) as u16;
                    p.resize(cols, rows, cell, !self.w.defer_pty_resize);
                }
            }
        }
        self.w.dirty = true;
    }

    fn set_scale(&mut self, scale: f64) {
        if (scale - self.w.scale).abs() < 1e-6 || scale <= 0.0 {
            return;
        }
        self.w.scale = scale;
        if self.w.fonts.set_size(self.w.font_size, scale) {
            self.w.renderer.clear_glyphs();
        }
        self.apply_size();
    }

    fn set_font_size(&mut self, size: f32) {
        self.w.font_size = size.clamp(4.0, 96.0);
        if self.w.fonts.set_size(self.w.font_size, self.w.scale) {
            self.w.renderer.clear_glyphs();
            self.apply_size();
        }
    }

    /// Physical position of a surface-local logical position.
    fn phys(&self, pos: (f64, f64)) -> (i32, i32) {
        ((pos.0 * self.w.scale) as i32, (pos.1 * self.w.scale) as i32)
    }

    fn pane_at(&self, pos: (f64, f64)) -> Option<PaneId> {
        self.hit_at(pos).map(|h| h.0)
    }

    fn hit_at(&self, pos: (f64, f64)) -> Option<(PaneId, layout::Part)> {
        let (x, y) = self.phys(pos);
        self.view().hit(x, y, self.layout_metrics())
    }

    /// Cell (col, viewport row) of pane `id` under a logical position, and
    /// the pixel offset inside the grid; clamped to the grid.
    fn cell_in(&self, id: PaneId, pos: (f64, f64)) -> Option<(usize, usize, (u32, u32))> {
        let r = self.view().rect(id)?;
        let t = &self.panes.get(&id)?.term;
        let (cw, ch) = self.cell();
        let px = (pos.0 * self.w.scale - r.x as f64).max(0.0);
        let py = (pos.1 * self.w.scale - r.y as f64).max(0.0);
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
        let rows = rows::Rows::of(t);
        let last = t.lines() - 1;
        let at_last = |l: u64| rows.row_of(l) == Some(last);
        let cursor = self.w.copy.as_ref().filter(|c| c.pane == id).is_some_and(|c| at_last(c.cursor.line));
        let current = self
            .w
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.current?))
            .filter(|(p, _)| *p == id)
            .is_some_and(|(_, m)| at_last(m.start.line) || at_last(m.end.line));
        usize::from(cursor || (current && self.w.input.is_some()))
    }

    fn point_in(&self, id: PaneId, pos: (f64, f64)) -> Option<Point> {
        let (col, row, _) = self.cell_in(id, pos)?;
        let rows = rows::Rows::of(&self.panes.get(&id)?.term);
        Some(Point::new(rows.line(row).or_else(|| rows.last_line())?, col))
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
            .w
            .tabs
            .iter()
            .map(|t| {
                let title = t.name.as_deref().unwrap_or_else(|| self.panes.get(&t.focus).map_or("", |p| p.title()));
                let n = t.panes().len();
                // A bell rang in one of its panes.
                let bell =
                    if t.panes().iter().any(|id| self.panes.get(id).is_some_and(|p| p.bell)) { "• " } else { "" };
                match (n, t.zoomed) {
                    (1, _) => format!("{bell}{title}"),
                    (n, true) => format!("{bell}{title} [{n}, zoom]"),
                    (n, false) => format!("{bell}{title} [{n}]"),
                }
            })
            .collect();
        let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
        chrome::tab_bar(&titles, self.w.active, p, p, cols, cw, theme)
    }

    fn pane_part(&self, id: PaneId, r: Rect, fid: PaneId, floating: bool, theme: &Theme) -> PanePart {
        let focused = id == fid;
        let preedit = self.ime.as_ref().and_then(|i| i.preedit.clone());
        let pane = self.panes.get(&id);
        let rows = pane.map_or(rows::Rows(Vec::new()), |p| rows::Rows::of(&p.term));
        let skip = self.row_skip(id);
        let mut marks = self.search_marks(id);
        marks.extend(self.hint_marks(id));
        PanePart {
            id,
            x: r.x as f32,
            y: r.y as f32,
            focused: focused && self.w.focused,
            preedit: if focused && self.w.input.is_none() && self.w.hints.is_none() { preedit } else { None },
            copy_cursor: self.copy_cursor_cell(id),
            marks,
            skip,
            floating,
            backdrop: self.gutter_marks(id, r, &rows, skip, theme),
            labels: Vec::new(),
            rows,
            flash: pane.and_then(|p| p.flash_until).is_some_and(|t| Instant::now() < t),
        }
    }

    /// Title text of a pane's title row: name, and the mode on the focused one.
    fn title_text(&self, id: PaneId, focused: bool) -> String {
        let p = self.panes.get(&id);
        let t = p.map_or("", |p| p.title());
        let t = if p.is_some_and(|p| p.bell) { format!("• {t}") } else { t.to_string() };
        if focused && self.w.mode != Mode::Normal { format!(" {t}  [{}]", self.w.mode.name()) } else { format!(" {t}") }
    }

    fn frame_parts(&self) -> FrameParts {
        let view = self.view();
        let m = self.layout_metrics();
        let fid = self.focused_id();
        let theme = self.theme();
        let (pw, ph) = self.physical();
        let (cw, ch) = self.cell();
        let (cw, ch) = (cw as f32, ch as f32);
        let p = self.pad() as f32;
        let thick = (self.w.scale.round() as f32).max(1.0);
        let float_focus = self.w.tabs[self.w.active].focus_is_float();
        let mut panes: Vec<PanePart> =
            view.panes.iter().map(|&(id, r)| self.pane_part(id, r, fid, false, &theme)).collect();

        // Tiled chrome: dividers, the focused pane's accent, title rows.
        let mut tiled_rects = Vec::new();
        let mut tiled_labels = Vec::new();
        for d in &view.dividers {
            let g = &d.gap;
            let r = match d.axis {
                Axis::X => [(g.x as f32 + g.w as f32 / 2.0 - thick / 2.0).round(), g.y as f32, thick, g.h as f32],
                Axis::Y => [g.x as f32, (g.y as f32 + g.h as f32 / 2.0 - thick / 2.0).round(), g.w as f32, thick],
            };
            tiled_rects.push((r, theme.dim));
        }
        let tiled_focus = if float_focus { None } else { Some(fid) };
        if view.panes.len() > 1
            && let Some(r) = tiled_focus.and_then(|f| view.panes.iter().find(|p| p.0 == f).map(|p| p.1))
        {
            // Include the pane's title row in the outline.
            let r = match view.titles.iter().find(|t| Some(t.id) == tiled_focus && !t.collapsed) {
                Some(t) => Rect::new(r.x, t.rect.y, r.w, r.bottom() - t.rect.y),
                None => r,
            };
            let area = self.area();
            let half = m.gap as f32 / 2.0;
            let (x0, y0) = ((r.x as f32 - half).max(area.x as f32), (r.y as f32 - half).max(area.y as f32));
            let x1 = (r.right() as f32 + half).min(area.right() as f32);
            let y1 = (r.bottom() as f32 + half).min(area.bottom() as f32);
            let mid = |v: f32| (v - thick / 2.0).round();
            if r.x > area.x {
                tiled_rects.push(([mid(r.x as f32 - half), y0, thick, y1 - y0], theme.accent));
            }
            if r.right() < area.right() {
                tiled_rects.push(([mid(r.right() as f32 + half), y0, thick, y1 - y0], theme.accent));
            }
            if r.y > area.y {
                tiled_rects.push(([x0, mid(r.y as f32 - half), x1 - x0, thick], theme.accent));
            }
            if r.bottom() < area.bottom() {
                tiled_rects.push(([x0, mid(r.bottom() as f32 + half), x1 - x0, thick], theme.accent));
            }
        }
        for t in &view.titles {
            let on = Some(t.id) == tiled_focus;
            let (fg, bg) = match (on, t.collapsed) {
                (true, _) => (theme.bg, theme.accent),
                (false, true) => (theme.dim, theme.bar),
                (false, false) => (theme.fg, theme.bar),
            };
            let cols = (t.rect.w as f32 / cw).max(1.0) as usize;
            tiled_labels.push(render::Label {
                x: t.rect.x as f32,
                y: t.rect.y as f32,
                cols,
                text: self.title_text(t.id, on),
                fg,
                bg: Some(bg),
                bold: on,
            });
        }

        // Floating panes, each with its frame.
        for f in &view.floats {
            let on = f.id == fid;
            let mut part = self.pane_part(f.id, f.content, fid, true, &theme);
            let bg = self.panes.get(&f.id).map_or(theme.bg, |p| p.term.palette().background());
            let line = if on { theme.accent } else { theme.dim };
            let o = f.outer;
            let (x, y, w, h) = (o.x as f32, o.y as f32, o.w as f32, o.h as f32);
            let mut frame = vec![
                ([x, y, w, h], bg),
                ([x, f.title.y as f32, w, ch], if on { theme.accent } else { theme.bar }),
                ([x, y, thick, h], line),
                ([x + w - thick, y, thick, h], line),
                ([x, y + h - thick, w, thick], line),
            ];
            frame.append(&mut part.backdrop);
            part.backdrop = frame;
            let cols = (f.title.w as f32 / cw).max(1.0) as usize;
            part.labels.push(render::Label {
                x: f.title.x as f32,
                y: f.title.y as f32,
                cols,
                text: self.title_text(f.id, on),
                fg: if on { theme.bg } else { theme.fg },
                bg: None,
                bold: on,
            });
            panes.push(part);
        }

        let mut rects = Vec::new();
        let mut labels = Vec::new();
        if self.tab_bar_visible() {
            rects.push(([0.0, 0.0, pw as f32, 1.5 * p + ch], theme.bar));
            labels.extend(self.tab_labels(&theme));
        }
        if self.hint_bar_visible() {
            let y = ph as f32 - p - ch;
            rects.push(([0.0, y - p / 2.0, pw as f32, ph as f32 - y + p / 2.0], theme.bar));
            let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
            if let Some(i) = &self.w.input {
                use unicode_segmentation::UnicodeSegmentation;
                use unicode_width::UnicodeWidthStr;
                let caret_cols: usize = i.before_cursor().graphemes(true).map(|g| g.width().clamp(1, 2)).sum();
                let (l, x) =
                    chrome::input_bar(i.purpose.badge(), &i.text, caret_cols, &i.status, p, y, cols, cw, &theme);
                labels.extend(l);
                rects.push(([x, y, (cw / 8.0).round().max(1.0), ch], theme.fg));
            } else if let Some(v) = &self.w.viewer {
                let hints = [
                    ("wheel +-", "zoom"),
                    ("0", "fit"),
                    ("1", "1:1"),
                    ("drag ←→", "pan"),
                    ("c", "copy"),
                    ("s", "save"),
                    ("esc", "close"),
                ]
                .map(|(k, a)| (k.to_string(), a))
                .to_vec();
                labels.extend(chrome::hint_bar(&v.title(self.viewer_area()), &hints, p, y, cols, cw, &theme));
            } else if let Some(h) = &self.w.hints {
                let hints = vec![
                    ("a-z".to_string(), h.action.key()),
                    ("shift".to_string(), "copy"),
                    ("ctrl".to_string(), "insert"),
                    ("bksp".to_string(), "undo"),
                    ("esc".to_string(), "cancel"),
                ];
                labels.extend(chrome::hint_bar(&format!("HINTS {}", h.items.len()), &hints, p, y, cols, cw, &theme));
            } else {
                let mut name = self.w.mode.name().to_string();
                if self.w.tabs[self.w.active].zoomed {
                    name.push_str(" (zoom)");
                }
                if let Some(s) = &self.w.search {
                    name = format!("{name} {}", s.status());
                }
                labels.extend(chrome::hint_bar(&name, &self.keymap.hints(self.w.mode), p, y, cols, cw, &theme));
            }
        }

        labels.extend(self.hint_labels(&theme));
        labels.extend(self.hover_label(&theme));

        FrameParts {
            panes,
            info: FrameInfo {
                panes: Vec::new(),
                pad: p,
                theme,
                cursor_blink_off: self.blink_off,
                ligatures: self.cfg.ligatures,
                tiled_rects,
                tiled_labels,
                rects,
                labels,
                overlay: self.w.prompts.front().map(Prompt::overlay),
                viewer: None,
            },
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.w.dirty = true;
    }

    fn update_title(&mut self) {
        let t = self.focused_pane().title();
        let t = if t.is_empty() { "proteus" } else { t }.to_string();
        if t != self.w.window_title {
            self.w.window.set_title(t.clone());
            self.w.window_title = t;
        }
    }

    fn notice(&mut self, msg: String) {
        self.w.prompts.push_back(Prompt::Notice(msg));
        self.w.dirty = true;
    }

    // --- input method ----------------------------------------------------------

    /// Text committed by the input method is sent as if typed.
    pub(crate) fn commit_text(&mut self, text: &str) {
        self.restart_blink();
        if self.w.input.is_some() {
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
        let (cols, x) = (t.cols(), t.cursor().x);
        let skip = self.row_skip(self.focused_id());
        let y = rows::Rows::of(t).row_of(t.grid().abs_line(t.cursor().y)).unwrap_or(t.cursor().y).saturating_sub(skip);
        let s = self.w.scale;
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

    /// Redraw the window after `after`, unless a redraw is already due
    /// sooner. Only the earliest timer counts; superseded ones do nothing.
    fn schedule(&mut self, after: Duration) {
        let at = Instant::now() + after;
        if self.w.wake_at.is_some_and(|w| w <= at) {
            return;
        }
        self.w.wake_at = Some(at);
        let id = self.w.id;
        let _ = self.handle.insert_source(Timer::from_deadline(at), move |_, _, app| {
            if app.activate(id) && app.w.wake_at == Some(at) {
                app.w.wake_at = None;
                app.w.dirty = true;
            }
            TimeoutAction::Drop
        });
    }

    fn draw(&mut self) {
        if !self.w.configured || self.w.frame_pending || self.exit {
            return;
        }
        let view = self.view();
        let visible: Vec<PaneId> = view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)).collect();
        let mine: Vec<PaneId> = self.w.tabs.iter().flat_map(|t| t.panes()).collect();
        // Mode 2026: hold the frame while an application is mid-update.
        if visible.iter().any(|id| self.panes.get(id).is_some_and(|p| p.term.is_synchronized())) {
            let since = *self.w.sync_since.get_or_insert_with(Instant::now);
            let held = since.elapsed();
            if held < SYNC_TIMEOUT {
                self.schedule(SYNC_TIMEOUT - held);
                return;
            }
        } else {
            self.w.sync_since = None;
        }
        let mut changed = std::mem::take(&mut self.w.dirty);
        // Animated images advance only while their pane is on screen.
        let now = Instant::now();
        let mut next_frame: Option<Instant> = None;
        for id in &visible {
            if let Some(p) = self.panes.get_mut(id)
                && let Some(t) = p.term.tick_images(now)
            {
                next_frame = Some(next_frame.map_or(t, |n| n.min(t)));
            }
        }
        for id in &mine {
            // Hidden panes' damage is dropped: they redraw fully when shown.
            if let Some(p) = self.panes.get_mut(id)
                && !p.term.take_damage().is_empty()
                && visible.contains(id)
            {
                changed = true;
            }
        }
        if let Some(t) = next_frame {
            self.schedule(t.saturating_duration_since(now));
        }
        if !changed {
            return;
        }
        let surface = self.w.window.wl_surface().clone();
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let area = self.viewer_area();
        let mut info = self.frame_parts().resolve(&self.panes);
        info.viewer = self.w.viewer.as_ref().map(|v| v.frame(area));
        let ok = self.w.renderer.draw(&mut self.w.fonts, &info);
        drop(info);
        if ok {
            self.w.frame_pending = true;
        } else {
            // No buffer available (resize race): try again shortly.
            self.w.dirty = true;
            self.schedule(Duration::from_millis(16));
        }
        if std::mem::take(&mut self.w.renderer.redraw_needed) {
            self.w.dirty = true;
        }
        // Animations: the spinner of running commands, the visual bell.
        let flash_end = visible.iter().filter_map(|id| self.panes.get(id)?.flash_until).max();
        match flash_end {
            Some(t) if t > Instant::now() => self.schedule(t - Instant::now()),
            _ if self.spinner_visible() => self.schedule(blocks::SPINNER),
            _ => {}
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
        let mut core = self.cfg.core();
        core.graphics.window_budget = Some(self.image_budget.clone());
        let tx = std::sync::Mutex::new(self.sender.clone());
        core.graphics.waker = Some(proteus_core::graphics::DecodeWaker(std::sync::Arc::new(move || {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(Msg::ImagesDecoded(id));
            }
        })));
        let pane = Pane::spawn(id, &opts, core, (cw, ch), self.sender.clone())?;
        self.panes.insert(id, pane);
        Ok(id)
    }

    /// Split the focused pane (`None`: along its longer side). With a
    /// floating pane focused, `None` opens another floating pane.
    fn split(&mut self, axis: Option<Axis>) {
        self.split_with(axis, None, None);
    }

    /// Split, running `command` (else the shell) in `cwd` (else the
    /// focused pane's directory).
    fn split_with(&mut self, axis: Option<Axis>, cwd: Option<PathBuf>, command: Option<Vec<OsString>>) {
        let (area, m) = (self.area(), self.layout_metrics());
        let tab = &self.w.tabs[self.w.active];
        if axis.is_none() && tab.focus_is_float() && command.is_none() {
            return self.new_floating();
        }
        let id = tab.last_tiled;
        let Some(r) = tab.full_layout(area, m).rect(id) else { return };
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
        let cwd = cwd.or_else(|| self.focused_pane().cwd(&self.hostname));
        let old = self.focused_id();
        match self.spawn_pane(c as u16, rw as u16, cwd, command) {
            Ok(new) => {
                self.w.tabs[self.w.active].split(new, axis);
                self.relayout();
                self.after_focus(old);
            }
            Err(e) => self.notice(format!("cannot start shell: {e}")),
        }
    }

    fn new_floating(&mut self) {
        let (area, m) = (self.area(), self.layout_metrics());
        let cwd = self.focused_pane().cwd(&self.hostname);
        let id = self.next_id;
        let f = self.w.tabs[self.w.active].default_float(id, area, m);
        match self.spawn_pane(f.cols as u16, f.rows as u16, cwd, None) {
            Ok(new) => {
                self.w.tabs[self.w.active].add_float(layout::Float { id: new, ..f });
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
                self.w.tabs.insert(self.w.active + 1, Tab::new(id));
                self.w.active += 1;
                self.relayout();
            }
            Err(e) => self.notice(format!("cannot start shell: {e}")),
        }
    }

    /// Next visible pane (tiled in order, then floats), wrapping.
    fn focus_next(&mut self) {
        let view = self.view();
        let order: Vec<PaneId> = view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)).collect();
        let fid = self.focused_id();
        if let Some(i) = order.iter().position(|&p| p == fid) {
            let next = order[(i + 1) % order.len()];
            self.w.tabs[self.w.active].set_focus(next);
        }
    }

    // --- windows and moving tabs between them ----------------------------------

    /// A new window with a shell in the directory of the focused pane.
    fn open_window(&mut self) {
        let cwd = self.focused_pane().cwd(&self.hostname);
        let from = self.w.id;
        if let Err(e) = self.new_window() {
            return self.notice(format!("cannot open a window: {e}"));
        }
        let (cols, rows) = {
            let m = self.layout_metrics_default();
            let a = self.area();
            ((a.w / m.0).max(2) as u16, (a.h / m.1).max(1) as u16)
        };
        match self.spawn_pane(cols, rows, cwd, None) {
            Ok(id) => {
                self.w.tabs.push(Tab::new(id));
                self.w.active = 0;
                self.relayout();
                self.update_title();
            }
            Err(e) => {
                self.destroy_window();
                self.activate(from);
                self.notice(format!("cannot start shell: {e}"));
            }
        }
    }

    fn layout_metrics_default(&self) -> (i32, i32) {
        let (w, h) = self.cell();
        (w as i32, h as i32)
    }

    /// Move the tab holding `pane` from window `from` to window `to` (a new
    /// window if `None`), at index `at` (default: the end). A window left
    /// without tabs closes.
    fn move_tab(&mut self, from: WinId, pane: PaneId, to: Option<WinId>, at: Option<usize>) {
        if !self.activate(from) {
            return;
        }
        let Some(i) = self.w.tabs.iter().position(|t| t.contains(pane)) else { return };
        if to == Some(from) {
            // Reorder within the window.
            let j = at.unwrap_or(self.w.tabs.len() - 1).min(self.w.tabs.len() - 1);
            let t = self.w.tabs.remove(i);
            self.w.tabs.insert(j, t);
            self.w.active = j;
            self.w.dirty = true;
            return;
        }
        if to.is_none() && self.w.tabs.len() == 1 {
            return; // already alone in its window
        }
        if self.w.copy.as_ref().is_some_and(|c| self.w.tabs[i].contains(c.pane)) || self.w.mode == Mode::Scroll {
            self.set_mode(Mode::Normal);
        }
        let old = self.focused_id();
        let tab = self.w.tabs.remove(i);
        if self.w.active > i || self.w.active >= self.w.tabs.len() {
            self.w.active = self.w.active.saturating_sub(1);
        }
        let src_empty = self.w.tabs.is_empty();
        if !src_empty {
            self.relayout();
            self.after_focus(old);
        }
        let target = match to {
            Some(t) if self.activate(t) => t,
            _ => match self.new_window() {
                Ok(t) => t,
                Err(e) => {
                    // Put it back.
                    self.activate(from);
                    self.w.tabs.insert(i.min(self.w.tabs.len()), tab);
                    return self.notice(format!("cannot open a window: {e}"));
                }
            },
        };
        let j = at.unwrap_or(self.w.tabs.len()).min(self.w.tabs.len());
        self.w.tabs.insert(j, tab);
        self.w.active = j;
        self.relayout();
        self.update_title();
        if src_empty {
            self.activate(from);
            self.destroy_window();
            self.activate(target);
        }
    }

    /// Take the focused pane out of its tab into a tab of a new window.
    fn pane_to_new_window(&mut self) {
        let id = self.focused_id();
        let from = self.w.id;
        if self.w.tabs[self.w.active].panes().len() == 1 {
            return self.move_tab(from, id, None, None);
        }
        if !self.w.tabs[self.w.active].remove(id) {
            return;
        }
        self.relayout();
        self.after_focus(id);
        match self.new_window() {
            Ok(_) => {
                self.w.tabs.push(Tab::new(id));
                self.relayout();
                self.update_title();
            }
            Err(e) => {
                self.activate(from);
                self.w.tabs.insert(self.w.active + 1, Tab::new(id));
                self.notice(format!("cannot open a window: {e}"));
            }
        }
    }

    /// Ask before closing a window's panes if something runs in them.
    fn request_close_window(&mut self) {
        let ids: Vec<PaneId> = self.w.tabs.iter().flat_map(|t| t.panes()).collect();
        if self.parked.is_empty() {
            return self.request_quit();
        }
        self.request_close(ids, "this window");
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
            self.w.prompts.push_back(Prompt::Close(ids, format!("Close {what}? Running: {}", busy.join(", "))));
            self.w.dirty = true;
        }
    }

    fn request_quit(&mut self) {
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        let busy = self.busy(&ids);
        if busy.is_empty() {
            self.quit_requested = true;
            self.exit = true;
        } else if !self.w.prompts.iter().any(|p| matches!(p, Prompt::Quit(_))) {
            self.w.prompts.push_back(Prompt::Quit(format!("Close the window? Running: {}", busy.join(", "))));
            self.w.dirty = true;
        }
    }

    /// Remove a pane (its process exited or the user closed it). The last
    /// pane in a tab closes the tab, the last tab its window (or starts a
    /// new shell, with `panes.keep_last_shell`); the last window ends the
    /// program.
    fn close_pane(&mut self, id: PaneId) {
        if !self.activate_pane(id) {
            self.panes.remove(&id);
            return;
        }
        let Some(ti) = self.w.tabs.iter().position(|t| t.contains(id)) else { return };
        let last_tab = self.w.tabs.len() == 1 && self.w.tabs[0].panes().len() == 1;
        if last_tab && self.parked.is_empty() && !self.cfg.keep_last_shell {
            if let Some(p) = self.panes.get_mut(&id) {
                self.exit_code = p.finish();
            }
            self.exit = true;
            return;
        }
        let old = self.focused_id();
        if !self.w.tabs[ti].remove(id) {
            self.w.tabs.remove(ti);
            if self.w.active > ti {
                self.w.active -= 1;
            }
            self.w.active = self.w.active.min(self.w.tabs.len().saturating_sub(1));
        }
        self.panes.remove(&id);
        self.w.prompts.retain(|p| match p {
            Prompt::ClipboardRead(p, ..) | Prompt::Paste(p, _) => *p != id,
            _ => true,
        });
        if self.w.mouse.selecting == Some(id) || self.w.mouse.reported.is_some_and(|r| r.0 == id) {
            self.w.mouse = Mouse { pos: self.w.mouse.pos, ..Mouse::default() };
        }
        if self.w.tabs.is_empty() {
            if self.cfg.keep_last_shell {
                if let Err(e) = self.open_default(None) {
                    eprintln!("proteus: cannot start shell: {e}");
                    self.destroy_window();
                }
            } else {
                self.destroy_window();
            }
            return;
        }
        self.relayout();
        self.after_focus(old);
    }

    /// Bookkeeping after the focused pane may have changed (`old` was
    /// focused before): focus reports, window title, cursor.
    fn after_focus(&mut self, old: PaneId) {
        let new = self.focused_id();
        if new != old {
            self.w.hints = None;
        }
        if self.w.focused
            && let Some(p) = self.panes.get_mut(&new)
        {
            p.bell = false;
        }
        if self.w.copy.as_ref().is_some_and(|c| c.pane != new) || (self.w.mode == Mode::Scroll && self.w.copy.is_none())
        {
            self.set_mode(Mode::Normal);
        }
        if new != old && self.w.focused {
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
        self.w.dirty = true;
    }

    fn run_action(&mut self, action: Action) {
        let (area, m) = (self.area(), self.layout_metrics());
        let old = self.focused_id();
        let n = self.w.tabs.len();
        let tab = &mut self.w.tabs[self.w.active];
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
                        Dir::Left if self.w.active > 0 => self.w.active -= 1,
                        Dir::Right if self.w.active + 1 < n => self.w.active += 1,
                        _ => {}
                    }
                }
            }
            Action::Split(axis) => self.split(Some(axis)),
            Action::NewPane => self.split(None),
            Action::ClosePane => self.request_close(vec![old], "this pane"),
            Action::Zoom => {
                if tab.tiled().len() > 1 && !tab.focus_is_float() {
                    tab.zoomed = !tab.zoomed;
                    self.relayout();
                }
            }
            Action::PaneToNewTab => {
                if tab.remove(old) {
                    self.w.tabs.insert(self.w.active + 1, Tab::new(old));
                    self.w.active += 1;
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
                if tab.swap(area, m, d, 1) {
                    self.relayout();
                }
            }
            Action::NewTab => self.new_tab(),
            Action::GotoTab(i) if (i as usize) < n => self.w.active = i as usize,
            Action::GotoTab(_) => {}
            Action::PrevTab => self.w.active = (self.w.active + n - 1) % n,
            Action::NextTab => self.w.active = (self.w.active + 1) % n,
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
            Action::Font(d) => self.set_font_size(self.w.font_size + d as f32),
            Action::Quit => self.request_close_window(),
            Action::CopyMove(m) => self.copy_move(m),
            Action::CopySelect(k) => self.copy_select(k),
            Action::CopyYank => self.copy_yank(),
            Action::CopyEscape => self.copy_escape(),
            Action::SearchStart => self.search_start(),
            Action::SearchUp => self.search_step(true),
            Action::SearchDown => self.search_step(false),
            Action::RenameTab => {
                let current = self.w.tabs[self.w.active].name.clone().unwrap_or_default();
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
            Action::FocusNext => self.focus_next(),
            Action::ToggleFloating => {
                if tab.floats.is_empty() {
                    self.new_floating();
                } else {
                    tab.toggle_floats();
                    self.relayout();
                }
            }
            Action::ToggleEmbed => {
                if tab.toggle_float(area, m) {
                    self.relayout();
                }
            }
            Action::NewFloating => self.new_floating(),
            Action::ToggleStack => {
                if tab.toggle_stack() {
                    self.relayout();
                }
            }
            Action::TogglePaneTitles => {
                self.cfg.pane_titles = match self.cfg.pane_titles {
                    PaneTitles::Never => PaneTitles::Always,
                    PaneTitles::Always | PaneTitles::Auto => PaneTitles::Never,
                };
                self.relayout();
            }
            Action::NewWindow => self.open_window(),
            Action::TabToNewWindow => {
                let from = self.w.id;
                self.move_tab(from, old, None, None);
            }
            Action::PaneToNewWindow => self.pane_to_new_window(),
            Action::TabToNextWindow => {
                let from = self.w.id;
                let ids = self.window_ids();
                let next =
                    ids.iter().copied().find(|&i| i > from).or_else(|| ids.first().copied().filter(|&i| i != from));
                self.move_tab(from, old, next, None);
            }
            Action::Palette => self.open_palette(),
            Action::BlockCopy(p) => self.block_copy(p),
            Action::BlockSelect(p) => self.block_select(p),
            Action::BlockFold => self.block_fold(),
            Action::BlockUnfoldAll => self.unfold_all(),
            Action::BlockRerun => self.block_rerun(),
            Action::BlockInfo => self.block_info(),
            Action::Hints(h) => self.hints_start(h),
            Action::ViewImage => self.view_last_image(),
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
    /// answers a prompt, `key:X` types X in hint mode, `sleep:SECS` waits.
    fn run_env_actions(&mut self) {
        let Ok(list) = std::env::var("PROTEUS_ACTIONS") else { return };
        let after = std::env::var("PROTEUS_ACTIONS_AFTER").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let steps: Vec<String> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        self.run_env_steps(steps, after);
    }

    fn run_env_steps(&mut self, steps: Vec<String>, after: f64) {
        let run = move |app: &mut App| {
            // The first window, whichever was drawn last.
            app.activate(1);
            for (i, name) in steps.iter().enumerate() {
                let name = name.as_str();
                if let Some(secs) = name.strip_prefix("sleep:").and_then(|s| s.parse::<f64>().ok()) {
                    app.run_env_steps(steps[i + 1..].to_vec(), secs.max(0.001));
                    return;
                }
                if let Some(c) =
                    name.strip_prefix("key:").filter(|_| app.w.hints.is_some()).and_then(|k| k.chars().next())
                {
                    app.hint_key(input::Key::Char(c), input::Mods::empty());
                    continue;
                }
                if let Some(k) = name.strip_prefix("key:").filter(|_| app.w.viewer.is_some()) {
                    let key = match k {
                        "esc" => input::Key::Escape,
                        "left" => input::Key::Left,
                        "right" => input::Key::Right,
                        "up" => input::Key::Up,
                        "down" => input::Key::Down,
                        _ => input::Key::Char(k.chars().next().unwrap_or(' ')),
                    };
                    app.viewer_key(key, input::Mods::empty());
                    continue;
                }
                if let Some(text) = name.strip_prefix("type:") {
                    // `\n` stands for Enter.
                    let text = &text.replace("\\n", "\r");
                    if app.w.input.is_some() {
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
        // Events (title, OSC 52 prompts) concern the pane's window.
        self.activate_pane(id);
        let focused = id == self.focused_id();
        let Some(p) = self.panes.get_mut(&id) else { return };
        p.term.advance(bytes);
        p.deliver_run(workspace::RUN_TIMEOUT);
        let reply = p.term.take_pty_output();
        if !reply.is_empty() {
            p.write(&reply);
        }
        let events = p.term.take_events();
        // A command started or ended: the foreground process changed.
        if events.iter().any(|e| matches!(e, Event::PromptStarted { .. } | Event::CommandStarted { .. })) {
            p.expire_proc_title();
        }
        let mut title = p.refresh_proc_title();
        for ev in events {
            match ev {
                Event::TitleChanged(_) => title = true,
                Event::ClipboardStore { kind, text } => self.set_selection(kind, text),
                Event::ClipboardLoad { kind, terminator } => self.clipboard_load(id, kind, terminator),
                Event::Bell => self.bell(id),
                Event::Notification { title, body } => {
                    if self.cfg.notify_osc {
                        let summary =
                            title.unwrap_or_else(|| self.panes.get(&id).map_or(APP_NAME, |p| p.title()).to_string());
                        self.notifier.send(id, summary, body);
                    }
                }
                Event::CommandFinished { block, exit_code, duration } => {
                    self.command_finished(id, block, exit_code, duration)
                }
                _ => {}
            }
        }
        if title {
            self.update_title();
            if self.tab_bar_visible() {
                self.w.dirty = true;
            }
        }
        if focused {
            self.ensure_blink();
        }
    }

    // --- bell and notifications -----------------------------------------------

    /// Window holding pane `id`, without making it current.
    fn win_with_pane(&self, id: PaneId) -> Option<&Win> {
        std::iter::once(&self.w).chain(&self.parked).find(|w| w.tabs.iter().any(|t| t.contains(id)))
    }

    /// Whether the user is looking at pane `id`: its window has focus and
    /// it is the focused pane of the active tab.
    fn pane_watched(&self, id: PaneId) -> bool {
        self.win_with_pane(id).is_some_and(|w| w.focused && w.tabs.get(w.active).is_some_and(|t| t.focus == id))
    }

    fn bell(&mut self, id: PaneId) {
        let watched = self.pane_watched(id);
        let visual = self.cfg.visual_bell;
        let Some(p) = self.panes.get_mut(&id) else { return };
        if visual {
            p.flash_until = Some(Instant::now() + FLASH);
        }
        if !watched {
            p.bell = true;
        }
        self.w.dirty = true;
    }

    fn command_finished(
        &mut self,
        id: PaneId,
        block: proteus_core::blocks::BlockId,
        exit_code: Option<i32>,
        duration: Option<Duration>,
    ) {
        let (Some(min), Some(d)) = (self.cfg.notify_after, duration) else { return };
        if d < min || self.pane_watched(id) {
            return;
        }
        let Some(p) = self.panes.get(&id) else { return };
        let command = p.term.blocks().get(block).and_then(|b| b.command.clone()).filter(|c| !c.is_empty());
        let summary = match exit_code {
            Some(c) if c != 0 => format!("Command failed (exit {c})"),
            _ => "Command finished".to_string(),
        };
        let mut body = command.map(|c| c.lines().next().unwrap_or("").to_string()).unwrap_or_default();
        body.push_str(&format!("{}took {}", if body.is_empty() { "" } else { "\n" }, blocks::human_duration(d)));
        self.notifier.send(id, summary, body);
    }

    /// Show pane `id`: its window, tab and focus. A window other than the
    /// keyboard window is raised with xdg-activation (`token` from a
    /// notification daemon, else one we request).
    fn reveal_pane(&mut self, id: PaneId, token: Option<String>) {
        if !self.activate_pane(id) {
            return;
        }
        let old = self.focused_id();
        if let Some(i) = self.w.tabs.iter().position(|t| t.contains(id)) {
            self.w.active = i;
            self.w.tabs[i].set_focus(id);
            self.relayout();
            self.after_focus(old);
        }
        let target = self.w.id;
        match (&self.activation, token) {
            (Some(a), Some(t)) => a.activate::<App>(self.w.window.wl_surface(), t),
            (Some(_), None) if self.kbd_win != Some(target) => self.request_activation(target),
            _ => {}
        }
        self.w.dirty = true;
    }

    /// Ask the compositor to focus window `target`, vouched for by the
    /// window that has the keyboard.
    fn request_activation(&mut self, target: WinId) {
        let Some(a) = &self.activation else { return };
        let from = self.kbd_win.and_then(|k| std::iter::once(&self.w).chain(&self.parked).find(|w| w.id == k));
        let seat = self.seat_state.seats().next();
        let data = RequestData {
            app_id: Some(APP_ID.into()),
            seat_and_serial: seat.map(|s| (s, self.serial)),
            surface: from.map(|w| w.window.wl_surface().clone()),
            udata: target,
        };
        a.request_token(&self.qh, data);
    }

    fn notification_clicked(&mut self, id: PaneId, token: Option<String>) {
        self.reveal_pane(id, token);
        self.activate_kbd();
    }

    // --- keyboard ------------------------------------------------------------

    fn key(&mut self, event: KeyEvent, action: KeyAction) {
        self.activate_kbd();
        if keys::is_modifier(event.keysym) {
            // Only for applications that asked for every key (kitty flag 8),
            // and only when nothing of ours takes the keys.
            let ours = !self.w.prompts.is_empty()
                || self.w.input.is_some()
                || self.w.hints.is_some()
                || self.w.viewer.is_some()
                || !self.w.mode.passes_keys();
            let wants =
                self.focused_pane().term.keyboard_flags().contains(proteus_core::KeyboardFlags::REPORT_ALL_AS_ESCAPES);
            if ours || !wants {
                return;
            }
        }
        let mods = keys::mods(&self.mods);
        let Some((key, shifted)) = self.levels.key(event.keysym, event.raw_code) else { return };
        // The key as our own UI sees it (keypad keys as their main keys);
        // the application gets `pane_key`.
        let pane_key = key;
        let key = match key {
            input::Key::Keypad(k) => k.main_key(event.utf8.as_deref().unwrap_or("")),
            k => k,
        };
        if action == KeyAction::Release {
            // The press went to the multiplexer: so does the release.
            if let Some(i) = self.consumed.iter().position(|&c| c == event.raw_code) {
                self.consumed.swap_remove(i);
                return;
            }
        } else {
            self.restart_blink();
            if !self.w.prompts.is_empty() {
                self.consume(event.raw_code);
                if action == KeyAction::Press {
                    self.answer_prompt(key);
                }
                return;
            }
            if self.w.viewer.is_some() {
                self.consume(event.raw_code);
                // `+` is typed as Shift+`=` on most layouts.
                let key = match (key, shifted) {
                    (input::Key::Char(_), Some(c)) if mods.contains(input::Mods::SHIFT) && "+".contains(c) => {
                        input::Key::Char(c)
                    }
                    _ => key,
                };
                self.viewer_key(key, mods);
                return;
            }
            if self.w.input.is_some() {
                self.consume(event.raw_code);
                self.input_key(key, mods, event.utf8.as_deref().unwrap_or(""));
                return;
            }
            if self.w.hints.is_some() {
                self.consume(event.raw_code);
                if action == KeyAction::Press {
                    self.hint_key(key, mods);
                }
                return;
            }
            match self.keymap.lookup_shifted(self.w.mode, key, mods, shifted) {
                Lookup::Pass => {}
                Lookup::Swallow => {
                    self.consume(event.raw_code);
                    return;
                }
                Lookup::Do(Action::Passthrough) => self.set_mode(self.keymap.after(self.w.mode, Action::Passthrough)),
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
        let ev = input::KeyEvent { key: pane_key, mods, action, text, shifted };
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
        let before = self.w.mode;
        let next = self.keymap.after(before, a);
        self.run_action(a);
        if self.w.mode == before && !self.exit {
            self.set_mode(next);
        }
        self.w.dirty = true;
    }

    fn consume(&mut self, raw_code: u32) {
        if !self.consumed.contains(&raw_code) {
            self.consumed.push(raw_code);
        }
    }

    /// Scrolling bound in Normal/Locked (Shift+PgUp, ...) applies to the
    /// scrollback only; full-screen applications get those keys.
    fn applies(&self, a: Action) -> bool {
        !(matches!(a, Action::Scroll(_)) && self.w.mode.passes_keys() && self.focused_pane().term.is_alt_screen())
    }

    // --- clipboard -------------------------------------------------------------

    fn set_selection(&mut self, kind: ClipboardKind, text: String) {
        match kind {
            ClipboardKind::Clipboard => {
                let (Some(mgr), Some(dev)) = (&self.data_manager, &self.data_device) else { return };
                let src = mgr.create_copy_paste_source(&self.qh, MIMES);
                src.set_selection(dev, self.serial);
                self.clipboard = Some((src, text));
                self.clipboard_image = None;
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
            ClipboardKind::Clipboard => {
                self.data_device.as_ref().and_then(|d| d.data().selection_offer()).and_then(|o| {
                    let mime = o.with_mime_types(pick)?;
                    o.receive(mime).ok().map(OwnedFd::from)
                })
            }
            ClipboardKind::Primary => {
                self.primary_device.as_ref().and_then(|d| d.data().selection_offer()).and_then(|o| {
                    let mime = o.with_mime_types(pick)?;
                    o.receive(mime).ok().map(OwnedFd::from)
                })
            }
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
        let (ClipTarget::Paste(id) | ClipTarget::Osc52(id, ..)) = target;
        self.activate_pane(id);
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
            if !self.w.prompts.iter().any(|p| matches!(p, Prompt::Paste(..))) {
                self.w.prompts.push_back(Prompt::Paste(id, text));
                self.w.dirty = true;
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
        } else if !self.w.prompts.iter().any(|p| matches!(p, Prompt::ClipboardRead(..))) {
            // One question at a time; repeated queries while it is up are dropped.
            self.w.prompts.push_back(Prompt::ClipboardRead(id, kind, terminator));
            self.w.dirty = true;
        }
    }

    fn answer_prompt(&mut self, key: input::Key) {
        use input::Key;
        let Some(p) = self.w.prompts.pop_front() else { return };
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
                _ => self.w.prompts.push_front(Prompt::ClipboardRead(id, kind, terminator)),
            },
            Prompt::Paste(id, text) => match key {
                Key::Enter => self.paste_now(id, &text),
                _ if yes => self.paste_now(id, &text),
                _ if no => {}
                _ => self.w.prompts.push_front(Prompt::Paste(id, text)),
            },
            Prompt::Close(ids, msg) => match key {
                Key::Enter => ids.into_iter().for_each(|id| self.close_pane(id)),
                _ if yes => ids.into_iter().for_each(|id| self.close_pane(id)),
                _ if no => {}
                _ => self.w.prompts.push_front(Prompt::Close(ids, msg)),
            },
            Prompt::Quit(msg) => match key {
                Key::Enter | Key::Char('y' | 'Y') => {
                    self.quit_requested = true;
                    self.exit = true;
                }
                _ if no => {}
                _ => self.w.prompts.push_front(Prompt::Quit(msg)),
            },
            Prompt::Restore(spec) => match key {
                Key::Enter => self.restore(&spec),
                _ if yes => self.restore(&spec),
                _ if no => {}
                _ => self.w.prompts.push_front(Prompt::Restore(spec)),
            },
            Prompt::Notice(_) => {}
        }
        self.w.dirty = true;
    }

    // --- cursor blink ----------------------------------------------------------

    fn wants_blink(&self) -> bool {
        let t = &self.focused_pane().term;
        let app = match self.cfg.cursor_blink {
            Blink::Never => false,
            Blink::Always => true,
            Blink::App => t.mode().contains(TermMode::CURSOR_BLINK),
        };
        app && self.w.focused && t.cursor_visible() && self.last_input.elapsed() < BLINK_IDLE
    }

    /// Input happened: show the cursor and restart the blink cycle.
    fn restart_blink(&mut self) {
        self.last_input = Instant::now();
        if self.blink_off {
            self.blink_off = false;
            self.w.dirty = true;
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
                app.activate_kbd();
                if !app.exit && app.wants_blink() {
                    app.blink_off = !app.blink_off;
                    app.w.dirty = true;
                    TimeoutAction::ToDuration(BLINK)
                } else {
                    app.blink_timer = None;
                    if app.blink_off {
                        app.blink_off = false;
                        app.w.dirty = true;
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
        let Some((col, line, pixel)) = self.cell_in(id, self.w.mouse.pos) else { return false };
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
        self.w.mouse.pos = ev.position;
        if self.w.viewer.is_some() && self.viewer_pointer(ev) {
            return;
        }
        match ev.kind {
            PointerEventKind::Enter { .. } => {
                self.w.mouse.icon = None;
                self.update_pointer_icon();
            }
            PointerEventKind::Leave { .. } => {
                self.update_hover(None);
            }
            PointerEventKind::Motion { .. } => {
                if self.w.mouse.drag.is_some() {
                    self.drag_to(ev.position);
                    return;
                }
                self.update_hover(Some(ev.position));
                self.update_pointer_icon();
                if let Some(id) = self.w.mouse.selecting {
                    let Some(p) = self.point_in(id, ev.position) else { return };
                    let block = self.mods.alt;
                    let Some(pane) = self.panes.get_mut(&id) else { return };
                    if let Some(anchor) = self.w.mouse.pending.take() {
                        if anchor != p {
                            let kind = if block { SelectionKind::Block } else { SelectionKind::Simple };
                            pane.term.start_selection(kind, anchor);
                        } else {
                            self.w.mouse.pending = Some(anchor);
                        }
                    }
                    if self.w.mouse.pending.is_none() {
                        pane.term.update_selection(p);
                    }
                } else if let Some((id, b)) = self.w.mouse.reported {
                    self.report_mouse(id, MouseAction::Motion, Some(b));
                } else if let Some(id) = self.pane_at(ev.position).filter(|&id| id == self.focused_id())
                    && self.mouse_reporting(id)
                {
                    self.report_mouse(id, MouseAction::Motion, None);
                }
            }
            PointerEventKind::Press { button, serial, .. } => {
                self.serial = serial;
                self.press_serial = serial;
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
                            self.w.mouse.drag = Some(Drag::Tab(i));
                        }
                        (MouseButton::Middle, Some(i)) => {
                            let ids = self.w.tabs[i].panes();
                            self.request_close(ids, "this tab");
                        }
                        _ => {}
                    }
                    return;
                }
                if b == MouseButton::Left
                    && let Some(d) = self.divider_at(ev.position)
                {
                    self.w.mouse.drag = Some(Drag::Divider(d));
                    return;
                }
                // A gutter mark folds its block; a fold marker unfolds.
                if b == MouseButton::Left
                    && let Some((id, block)) = self.gutter_hit(ev.position)
                {
                    self.toggle_fold(id, block);
                    return;
                }
                if b == MouseButton::Left
                    && let Some((id, row, false)) = self.row_at(ev.position)
                    && let Some(rows::Row::Fold { block, .. }) =
                        self.panes.get(&id).and_then(|p| rows::Rows::of(&p.term).0.get(row).copied())
                {
                    self.toggle_fold(id, block);
                    return;
                }
                let Some((id, part)) = self.hit_at(ev.position) else { return };
                // Ctrl+click on an image opens the viewer.
                if b == MouseButton::Left
                    && self.mods.ctrl
                    && part == layout::Part::Content
                    && self.view_image_at(id, ev.position)
                {
                    return;
                }
                if id != self.focused_id() {
                    let old = self.focused_id();
                    self.w.tabs[self.w.active].set_focus(id);
                    self.after_focus(old);
                }
                match part {
                    layout::Part::Content => {}
                    layout::Part::Title => {
                        self.relayout();
                        return;
                    }
                    layout::Part::FloatFrame if b == MouseButton::Left => {
                        let (x, y) = self.phys(ev.position);
                        if let Some(r) = self.view().rect(id) {
                            self.w.mouse.drag = Some(Drag::FloatMove(id, (x - r.x, y - r.y)));
                        }
                        return;
                    }
                    layout::Part::FloatCorner if b == MouseButton::Left => {
                        self.w.mouse.drag = Some(Drag::FloatResize(id));
                        return;
                    }
                    layout::Part::FloatFrame | layout::Part::FloatCorner => return,
                }
                if self.mouse_reporting(id) && self.report_mouse(id, MouseAction::Press, Some(b)) {
                    self.w.mouse.reported = Some((id, b));
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
                    && let Some(d) = self.w.mouse.drag.take()
                {
                    if matches!(d, Drag::Divider(_) | Drag::FloatResize(_)) {
                        // The PTYs hear the final size only.
                        self.w.defer_pty_resize = false;
                        self.relayout();
                    }
                    return;
                }
                if let Some((id, b)) = self.w.mouse.reported.take() {
                    self.report_mouse(id, MouseAction::Release, Some(b));
                    return;
                }
                if button == BTN_LEFT
                    && let Some(id) = self.w.mouse.selecting.take()
                {
                    self.w.mouse.pending = None;
                    if let Some(text) =
                        self.panes.get(&id).and_then(|p| p.term.selection_text()).filter(|t| !t.is_empty())
                    {
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
                    let line_h = self.cell().1 as f64 / self.w.scale;
                    vertical.absolute / line_h / 3.0
                };
                self.w.mouse.scroll_acc += notches;
                let n = self.w.mouse.scroll_acc.trunc();
                self.w.mouse.scroll_acc -= n;
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

    /// Pointer events while the viewer is open. Returns false for the ones
    /// handled as usual (enter, leave).
    fn viewer_pointer(&mut self, ev: &PointerEvent) -> bool {
        match ev.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Leave { .. } => return false,
            PointerEventKind::Motion { .. } => {
                if let Some(Drag::Viewer(from)) = self.w.mouse.drag {
                    self.viewer_drag(from, ev.position);
                    self.w.mouse.drag = Some(Drag::Viewer(ev.position));
                }
            }
            PointerEventKind::Press { button, serial, .. } => {
                self.serial = serial;
                match button {
                    BTN_LEFT => self.w.mouse.drag = Some(Drag::Viewer(ev.position)),
                    BTN_RIGHT => self.close_viewer(),
                    _ => {}
                }
            }
            PointerEventKind::Release { button, .. } => {
                if button == BTN_LEFT {
                    self.w.mouse.drag = None;
                }
            }
            PointerEventKind::Axis { vertical, .. } => {
                let notches = if vertical.value120 != 0 {
                    vertical.value120 as f64 / 120.0
                } else if vertical.discrete != 0 {
                    vertical.discrete as f64
                } else {
                    vertical.absolute / 40.0
                };
                if notches != 0.0 {
                    self.viewer_wheel(notches, ev.position);
                }
            }
        }
        true
    }

    fn divider_at(&self, pos: (f64, f64)) -> Option<layout::Divider> {
        let (x, y) = self.phys(pos);
        let slop = (3.0 * self.w.scale).round() as i32;
        self.view().divider_at(x, y, slop).cloned()
    }

    /// Mouse moved while dragging a divider or a tab.
    fn drag_to(&mut self, pos: (f64, f64)) {
        let (x, y) = self.phys(pos);
        match &self.w.mouse.drag {
            Some(Drag::Divider(d)) => {
                let at = if d.axis == Axis::X { x } else { y };
                let m = self.layout_metrics();
                let d = d.clone();
                if self.w.tabs[self.w.active].drag_divider(&d, at, m) {
                    self.w.defer_pty_resize = true;
                    self.relayout();
                }
            }
            Some(Drag::FloatMove(id, (gx, gy))) => {
                let (id, gx, gy) = (*id, *gx, *gy);
                let (area, m) = (self.area(), self.layout_metrics());
                let t = &mut self.w.tabs[self.w.active];
                let Some(f) = t.floats.iter().find(|f| f.id == id).copied() else { return };
                let col = ((x - gx - area.x) as f32 / m.cell_w as f32).round() as i32;
                let row = ((y - gy - area.y) as f32 / m.cell_h as f32).round() as i32;
                if t.set_float_rect(id, col, row, f.cols, f.rows, area, m) {
                    self.relayout();
                }
            }
            Some(Drag::FloatResize(id)) => {
                let id = *id;
                let (area, m) = (self.area(), self.layout_metrics());
                let Some(r) = self.view().rect(id) else { return };
                let t = &mut self.w.tabs[self.w.active];
                let Some(f) = t.floats.iter().find(|f| f.id == id).copied() else { return };
                let cols = ((x - r.x) as f32 / m.cell_w as f32).round() as i32;
                let rows = ((y - r.y) as f32 / m.cell_h as f32).round() as i32;
                if t.set_float_rect(id, f.col, f.row, cols, rows, area, m) {
                    self.w.defer_pty_resize = true;
                    self.relayout();
                }
            }
            Some(Drag::Tab(i)) => {
                let i = *i;
                let (pw, ph) = self.physical();
                let ch = self.cell().1 as i32;
                let outside = x < 0 || y < 0 || x >= pw as i32 || y >= ph as i32;
                // Pulled well away from the tab bar: drag it to another window.
                if (outside || y > self.area().y + 2 * ch) && self.start_tab_dnd(i) {
                    self.w.mouse.drag = None;
                    return;
                }
                if let Some(Some(j)) = self.tab_at(pos)
                    && j != i
                    && i < self.w.tabs.len()
                {
                    let t = self.w.tabs.remove(i);
                    self.w.tabs.insert(j, t);
                    self.w.active = j;
                    self.w.mouse.drag = Some(Drag::Tab(j));
                    self.w.dirty = true;
                }
            }
            Some(Drag::Viewer(_)) | None => {}
        }
    }

    /// Begin dragging tab `i` with Wayland drag and drop, so it can be
    /// dropped on another window (or outside: a new window).
    fn start_tab_dnd(&mut self, i: usize) -> bool {
        let (Some(mgr), Some(dev)) = (&self.data_manager, &self.data_device) else { return false };
        if self.tab_dnd.is_some() || i >= self.w.tabs.len() {
            return false;
        }
        let source = mgr.create_drag_and_drop_source(&self.qh, [TAB_MIME], DndAction::Move);
        source.start_drag(dev, self.w.window.wl_surface(), None, self.press_serial);
        let pane = self.w.tabs[i].focus;
        self.tab_dnd = Some(TabDnd { source, from: self.w.id, pane, done: false, at: None });
        true
    }

    /// Resize arrows over dividers, a move/resize cursor over floating
    /// frames, an arrow over the tab bar and titles, else a text cursor.
    fn update_pointer_icon(&mut self) {
        let icon = match (self.divider_at(self.w.mouse.pos), self.hit_at(self.w.mouse.pos)) {
            _ if self.w.viewer.is_some() => CursorIcon::Grab,
            _ if self.w.hover.is_some() => CursorIcon::Pointer,
            (Some(d), _) if d.axis == Axis::X => CursorIcon::ColResize,
            (Some(_), _) => CursorIcon::RowResize,
            _ if self.tab_at(self.w.mouse.pos).is_some() => CursorIcon::Default,
            (None, Some((_, layout::Part::FloatFrame))) => CursorIcon::Move,
            (None, Some((_, layout::Part::FloatCorner))) => CursorIcon::SeResize,
            (None, Some((_, layout::Part::Title))) | (None, None) => CursorIcon::Default,
            (None, Some(_)) => CursorIcon::Text,
        };
        if self.w.mouse.icon != Some(icon) {
            self.w.mouse.icon = Some(icon);
            if let Some(p) = &self.pointer {
                let _ = p.set_cursor(&self.conn, icon);
            }
        }
    }

    fn click(&mut self, id: PaneId, pos: (f64, f64)) {
        let Some(p) = self.point_in(id, pos) else { return };
        let now = Instant::now();
        let count = match self.w.mouse.last_click {
            Some((t, i, q, n)) if now.duration_since(t) < DOUBLE_CLICK && i == id && q == p => n % 3 + 1,
            _ => 1,
        };
        self.w.mouse.last_click = Some((now, id, p, count));
        self.w.mouse.selecting = Some(id);
        let Some(pane) = self.panes.get_mut(&id) else { return };
        match count {
            1 => {
                pane.term.clear_selection();
                self.w.mouse.pending = Some(p);
            }
            n => {
                self.w.mouse.pending = None;
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
        if self.activate_surface(surface) && self.w.fractional.is_none() {
            surface.set_buffer_scale(factor);
            self.set_scale(factor as f64);
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        if self.activate_surface(surface) {
            self.w.frame_pending = false;
        }
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
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if self.activate_surface(window.wl_surface()) {
            self.request_close_window();
        }
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window, cfg: WindowConfigure, _: u32) {
        if !self.activate_surface(window.wl_surface()) {
            return;
        }
        let (w, h) = cfg.new_size;
        let default = if self.w.configured {
            self.w.size
        } else {
            // First configure without a size: 80x24 cells at the current scale.
            let m = self.w.fonts.metrics();
            let pad = self.pad() * 2;
            (
                ((80 * m.cell_w + pad) as f64 / self.w.scale).round() as u32,
                ((24 * m.cell_h + pad) as f64 / self.w.scale).round() as u32,
            )
        };
        self.w.size = (w.map_or(default.0, |v| v.get()), h.map_or(default.1, |v| v.get()));
        let focused = cfg.is_activated();
        if focused != self.w.focused && self.keyboard.is_none() {
            self.w.focused = focused;
        }
        let first = !self.w.configured;
        self.w.configured = true;
        self.apply_size();
        if first && self.w.id == 1 {
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
            match self.seat_state.get_pointer_with_theme::<App, ()>(
                qh,
                &seat,
                self.shm.wl_shm(),
                surface,
                ThemeSpec::default(),
            ) {
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
                self.w.dirty = true;
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
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        s: &WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if self.activate_surface(s) {
            self.kbd_win = Some(self.w.id);
            self.serial = serial;
            self.w.focused = true;
            self.w.dirty = true;
            self.restart_blink();
            let p = self.focused_pane_mut();
            p.bell = false;
            if let Some(b) = input::encode_focus(true, p.term.mode()) {
                p.write(b);
            }
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, s: &WlSurface, _: u32) {
        if self.activate_surface(s) {
            self.w.focused = false;
            self.w.dirty = true;
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
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        m: Modifiers,
        _: RawModifiers,
        layout: u32,
    ) {
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
            if self.activate_surface(&ev.surface) {
                self.pointer_event(ev);
            }
        }
        self.activate_kbd();
    }
}

impl DataDeviceHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, dev: &WlDataDevice, x: f64, y: f64, s: &WlSurface) {
        let Some(win) = self.win_of_surface(s) else { return };
        let Some(dnd) = self.tab_dnd.as_mut() else { return };
        dnd.at = Some((win, (x, y)));
        // Only our own tab drags are accepted.
        let Some(offer) = self.data_device.as_ref().filter(|d| d.inner() == dev).and_then(|d| d.data().drag_offer())
        else {
            return;
        };
        if offer.with_mime_types(|m| m.iter().any(|m| m == TAB_MIME)) {
            offer.accept_mime_type(offer.serial, Some(TAB_MIME.into()));
            offer.set_actions(DndAction::Move, DndAction::Move);
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        if let Some(d) = self.tab_dnd.as_mut() {
            d.at = None;
        }
    }
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64) {
        if let Some((_, pos)) = self.tab_dnd.as_mut().and_then(|d| d.at.as_mut()) {
            *pos = (x, y);
        }
    }
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        let offer = self.data_device.as_ref().and_then(|d| d.data().drag_offer());
        let Some(dnd) = self.tab_dnd.as_mut() else { return };
        let Some((win, pos)) = dnd.at else { return };
        dnd.done = true;
        let (from, pane) = (dnd.from, dnd.pane);
        if let Some(o) = offer {
            if o.selected_action == DndAction::Move {
                o.finish();
            }
            o.destroy();
        }
        // Dropped on a tab: insert there; elsewhere: at the end.
        let at = if self.activate(win) { self.tab_at(pos).flatten() } else { None };
        self.move_tab(from, pane, Some(win), at);
        self.activate_kbd();
    }
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
        } else if let Some((_, png)) = self.clipboard_image.as_ref().filter(|(s, _)| s.inner() == src)
            && mime == "image/png"
        {
            let (png, fd) = (png.clone(), OwnedFd::from(fd));
            let _ = std::thread::Builder::new().name("copy".into()).spawn(move || {
                let _ = File::from(fd).write_all(&png);
            });
        }
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, src: &WlDataSource) {
        if self.tab_dnd.as_ref().is_some_and(|d| d.source.inner() == src) {
            // Dropped outside our windows: the tab gets a window of its own.
            let Some(d) = self.tab_dnd.take() else { return };
            if !d.done {
                self.move_tab(d.from, d.pane, None, None);
                self.activate_kbd();
            }
            return;
        }
        if self.clipboard.as_ref().is_some_and(|(s, _)| s.inner() == src) {
            self.clipboard = None;
        }
        if self.clipboard_image.as_ref().is_some_and(|(s, _)| s.inner() == src) {
            self.clipboard_image = None;
        }
        src.destroy();
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, src: &WlDataSource) {
        if self.tab_dnd.as_ref().is_some_and(|d| d.source.inner() == src) {
            self.tab_dnd = None;
        }
    }
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl PrimarySelectionDeviceHandler for App {
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ZwpPrimarySelectionDeviceV1) {}
}

impl PrimarySelectionSourceHandler for App {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        src: &ZwpPrimarySelectionSourceV1,
        mime: String,
        fd: WritePipe,
    ) {
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

impl ActivationHandler for App {
    /// The window to activate with the token.
    type RequestUdata = WinId;
    fn new_token(&mut self, token: String, data: &RequestData<WinId>) {
        if let Some(a) = &self.activation
            && let Some(w) = std::iter::once(&self.w).chain(&self.parked).find(|w| w.id == data.udata)
        {
            a.activate::<App>(w.window.wl_surface(), token);
        }
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
                let preview: String =
                    first.chars().map(|c| if c.is_control() { '\u{fffd}' } else { c }).take(200).collect();
                Overlay { title, detail: Some(preview), keys: "[Enter] paste   [Esc] cancel".into() }
            }
            Prompt::Restore(spec) => Overlay {
                title: format!(
                    "Restore the previous session? ({} windows, {} tabs, {} panes; it did not exit normally)",
                    spec.windows.len(),
                    spec.tabs().count(),
                    spec.pane_count()
                ),
                detail: None,
                keys: "[y] restore   [n] start fresh".into(),
            },
            Prompt::Close(_, msg) | Prompt::Quit(msg) => {
                Overlay { title: msg.clone(), detail: None, keys: "[y] close   [n] cancel".into() }
            }
            Prompt::Notice(msg) => {
                Overlay { title: msg.clone(), detail: None, keys: "press any key to dismiss".into() }
            }
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
