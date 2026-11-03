//! The Wayland frontend: one xdg-toplevel window with tabs of split panes,
//! each pane a terminal session, rendered with wgpu.
//!
//! Event loop: calloop drives the Wayland connection, a channel fed by the PTY
//! reader thread, and timers. Frames are drawn only when something changed
//! and only after the compositor's frame callback (no busy loop).
//!
//! This module holds the application state and the event loop; the `App`
//! methods live in submodules by concern (windows, geometry, frame, panes,
//! actions, output, keyboard, pointer, clipboard, prompt, wayland, ...).

mod accessibility;
mod actions;
mod approvals;
mod blocks;
mod chrome;
mod clipboard;
mod completion;
mod config_ui;
mod copy;
mod decorations;
mod font;
mod frame;
mod geometry;
mod hint;
mod images;
mod ime;
mod ipc;
mod keyboard;
mod keys;
mod notify;
mod output;
mod pane;
mod panes;
mod plugins;
mod pointer;
mod prompt;
mod render;
mod rows;
mod scripting;
mod scroll;
mod ssh;
mod textinput;
mod transfer;
mod viewer;
mod wayland;
mod windows;
mod workspace;

pub use scripting::Overrides;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use proteus_core::event::{ClipboardKind, Terminator};
use proteus_core::input::{self, KeyAction, MouseAction, MouseButton, MouseEvent};
use proteus_core::selection::SelectionKind;
use proteus_core::{ClipboardRead, Event, Point, TermMode};

use crate::script::{ScriptEvent, Val};
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
use smithay_client_toolkit::reexports::protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1,
};
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
use prompt::Prompt;
use render::{FrameInfo, Overlay, PaneFrame, Renderer, Theme};
use textinput::{Input, Purpose};
use workspace::SessionStore;

const APP_ID: &str = "dev.proteus.Proteus";
const APP_NAME: &str = "Proteus";
/// How long the visual bell tints a pane.
const FLASH: Duration = Duration::from_millis(120);
const MIMES: [&str; 4] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING"];
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
    /// A plugin's `clipboard-read` (its waiting answer).
    Plugin(u64),
}

pub(crate) enum Msg {
    Ipc(crate::ipc::Call),
    Accessibility(WinId, Option<accesskit::ActionRequest>),
    GpuLost(WinId),
    Exported(Result<(), String>),
    SshWorkspace(PaneId, Result<Option<crate::project::Workspace>, String>),
    Pty(PaneId, Vec<u8>, (proteus_pty::budget::Permit, proteus_pty::budget::Permit)),
    PtyClosed(PaneId),
    Clipboard(ClipTarget, String, proteus_pty::budget::Permit),
    /// A desktop notification about this pane was clicked (with an
    /// xdg-activation token, if the daemon sent one).
    NotifyActivated(PaneId, Option<String>),
    /// Images of this pane finished decoding in the background.
    ImagesDecoded(PaneId),
    /// A plugin called the host, started, stopped or failed.
    Plugin(proteus_wasm::Key, proteus_wasm::FromPlugin),
    PluginsResolved(u64, Vec<Result<proteus_wasm::Resolved, String>>),
    /// A `text/uri-list` dropped on a plugin surface.
    Dropped(u32, String),
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
impl Dispatch2<ExtBackgroundEffectManagerV1, App> for ScaleData {
    fn event(
        &self,
        app: &mut App,
        _: &ExtBackgroundEffectManagerV1,
        ev: ext_background_effect_manager_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = ev {
            let blur =
                flags.into_result().is_ok_and(|f| f.contains(ext_background_effect_manager_v1::Capability::Blur));
            if blur != app.blur_capable {
                app.blur_capable = blur;
                app.style_all_windows();
            }
        }
    }
}
impl Dispatch2<ExtBackgroundEffectSurfaceV1, App> for ScaleData {
    fn event(
        &self,
        _: &mut App,
        _: &ExtBackgroundEffectSurfaceV1,
        _: <ExtBackgroundEffectSurfaceV1 as Proxy>::Event,
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
    /// A plugin surface instead of pane `id`.
    surface: Option<u32>,
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

impl PanePart {
    /// A plugin surface drawn like a pane.
    fn surface(id: u32, r: Rect, focused: bool, term: &proteus_core::Terminal) -> PanePart {
        PanePart {
            id: 0,
            surface: Some(id),
            x: r.x as f32,
            y: r.y as f32,
            focused,
            preedit: None,
            copy_cursor: None,
            marks: Vec::new(),
            skip: 0,
            floating: false,
            backdrop: Vec::new(),
            labels: Vec::new(),
            rows: rows::Rows::of(term),
            flash: false,
        }
    }
}

impl FrameParts {
    fn resolve<'a>(
        self,
        panes: &'a HashMap<PaneId, Pane>,
        surfaces: &'a std::collections::BTreeMap<u32, plugins::Surface>,
    ) -> FrameInfo<'a> {
        let mut info: FrameInfo<'a> = self.info;
        info.panes = self
            .panes
            .into_iter()
            .filter_map(|p| {
                let term = match p.surface {
                    Some(s) => &surfaces.get(&s)?.term,
                    None => &panes.get(&p.id)?.term,
                };
                Some(PaneFrame {
                    term,
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
    accessibility: Option<accesskit_unix::Adapter>,
    window: Window,
    id: WinId,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// Background effects of the surface, created when blur is first wanted.
    blur: Option<ExtBackgroundEffectSurfaceV1>,
    /// Logical size, opacity and blur last given to the compositor.
    styled: Option<((u32, u32), bool)>,
    /// Decoration mode, window state and our title bar's buttons.
    csd: decorations::CsdState,
    fonts: Fonts,
    font_size: f32,
    scale: f64,
    /// Logical window size.
    size: (u32, u32),
    configured: bool,
    suspended: bool,
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
    /// A configure changed the window; applied before the next frame.
    size_pending: bool,
    /// A hidden tab's pane waits for its resize (see `App::relayout`).
    resize_deferred: bool,
    /// Block whose gutter mark the pointer is over.
    hover: Option<(PaneId, proteus_core::blocks::BlockId)>,
    /// Hint mode: labels are shown and keys pick one.
    hints: Option<hint::HintState>,
    /// The image viewer, over everything; it takes keys and the mouse.
    viewer: Option<viewer::Viewer>,
    /// Messages in the top right corner (scripts, config errors).
    toasts: Vec<scripting::Toast>,
    /// Status segments from scripts: left and right of the tab bar.
    status: (Vec<String>, Vec<String>),
    /// A plugin surface has the keyboard.
    surface_focus: Option<u32>,
    /// A button pressed on a surface: drags and the release go to it.
    surface_drag: Option<(u32, proteus_wasm::types::MouseButton, (u32, u32))>,
    /// Paths a plugin carries in that drag (`start-drag`).
    drag_paths: Option<Vec<String>>,
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
    ipc_socket: Option<PathBuf>,
    xdg: XdgShell,
    viewporter: Option<WpViewporter>,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    blur_manager: Option<ExtBackgroundEffectManagerV1>,
    /// The compositor announced the blur capability.
    blur_capable: bool,
    /// Told the user that blur is unavailable.
    blur_warned: bool,
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
    /// Files from another program dragged over a window: where.
    file_dnd: Option<(WinId, (f64, f64))>,
    notifier: notify::Notifier,
    activation: Option<ActivationState>,
    hint_patterns: Vec<proteus_core::hints::HintPattern>,
    /// Palette entries used recently, most recent first.
    recent: Vec<String>,
    project: Result<Option<crate::project::Workspace>, String>,
    ssh_suggestions: crate::ssh::Suggestions,
    ssh_transfer_path: Option<(PaneId, bool, PathBuf)>,
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
    /// Lua: the config's script, its queued calls, reloading.
    sc: scripting::Scripting,
    /// WASM plugins and their surfaces.
    pl: plugins::Plugins,
    /// PTY output waiting for idle processing, merged per pane, with the
    /// budget permits that hold its readers back until then.
    pty_pending: Vec<PendingOutput>,
}

struct PendingOutput {
    pane: PaneId,
    bytes: Vec<u8>,
    permits: Vec<(proteus_pty::budget::Permit, proteus_pty::budget::Permit)>,
}

impl App {
    fn queue_pty_output(
        &mut self,
        pane: PaneId,
        bytes: Vec<u8>,
        permits: (proteus_pty::budget::Permit, proteus_pty::budget::Permit),
    ) {
        if self.pty_pending.is_empty() {
            self.handle.insert_idle(App::flush_pty_output);
        }
        match self.pty_pending.iter_mut().find(|p| p.pane == pane) {
            Some(p) => {
                p.bytes.extend_from_slice(&bytes);
                p.permits.push(permits);
            }
            None => self.pty_pending.push(PendingOutput { pane, bytes, permits: vec![permits] }),
        }
    }

    fn flush_pty_output(&mut self) {
        for p in std::mem::take(&mut self.pty_pending) {
            self.pty_output(p.pane, &p.bytes);
            drop(p.permits);
        }
    }
}

/// Wayland and GPU objects of a new window.
fn gpu_surface(
    instance: &wgpu::Instance,
    conn: &Connection,
    window: &Window,
) -> Result<wgpu::Surface<'static>, String> {
    let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(conn.backend().display_ptr().cast()).ok_or("no Wayland display")?,
    ));
    let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(window.wl_surface().id().as_ptr().cast()).ok_or("no surface")?,
    ));
    // SAFETY: the display (kept alive by `conn`) and the surface (owned by
    // `window`) outlive the wgpu surface, which `Win` drops first.
    unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_display),
            raw_window_handle: raw_window,
        })
    }
    .map_err(|e| format!("cannot create GPU surface: {e}"))
}

#[allow(clippy::too_many_arguments)] // Wayland globals needed to construct one window.
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
    let decorations = match cfg.decorations {
        crate::config::Decorations::Auto => WindowDecorations::RequestServer,
        crate::config::Decorations::None | crate::config::Decorations::Client => WindowDecorations::RequestClient,
    };
    let window = xdg.create_window(surface, decorations, qh);
    window.set_title("proteus");
    window.set_app_id(APP_ID);
    window.set_min_size(Some((100, 60)));
    window.commit();

    let fonts = Fonts::from_config(cfg, 1.0).map_err(err)?;
    let m = fonts.metrics();
    let size = logical.unwrap_or_else(|| cfg.window_size((m.cell_w, m.cell_h), 1.0));
    let wsurface = gpu_surface(instance, conn, &window).map_err(err)?;
    let mut renderer = Renderer::new(instance, wsurface.into(), size).map_err(err)?;
    renderer.images.budget = cfg.graphics.gpu_memory;
    Ok(Win {
        renderer,
        accessibility: None,
        window,
        id,
        viewport,
        fractional,
        blur: None,
        styled: None,
        csd: Default::default(),
        fonts,
        font_size: cfg.font_size,
        scale: 1.0,
        size,
        configured: false,
        suspended: false,
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
        size_pending: false,
        resize_deferred: false,
        hover: None,
        hints: None,
        viewer: None,
        toasts: Vec::new(),
        status: Default::default(),
        surface_focus: None,
        surface_drag: None,
        drag_paths: None,
    })
}

/// Inspect compositor capabilities without creating windows or PTYs.
pub fn diagnostic_protocols() -> Result<Vec<String>, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, _queue) = registry_queue_init::<App>(&conn).map_err(|e| e.to_string())?;
    let mut protocols: Vec<_> =
        globals.contents().clone_list().into_iter().map(|g| format!("{} v{}", g.interface, g.version)).collect();
    protocols.sort();
    protocols.dedup();
    Ok(protocols)
}

/// Run the GUI. `command` runs in the first pane instead of the shell;
/// `layout` opens a layout file instead.
pub fn run(
    loaded: crate::config::Loaded,
    config_path: Option<PathBuf>,
    overrides: Overrides,
    config_error: Option<String>,
    command: Option<Vec<OsString>>,
    layout: Option<LayoutSpec>,
    listen: Option<PathBuf>,
) -> io::Result<i32> {
    let err = |e: String| io::Error::other(e);
    let crate::config::Loaded { config: cfg, script } = loaded;
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
    let blur_manager = globals.bind::<ExtBackgroundEffectManagerV1, App, _>(&qh, 1..=1, ScaleData).ok();
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
            channel::Event::Msg(Msg::Exported(result)) => match result {
                Ok(()) => app.toast(app.cfg.i18n.t("notice-scrollback-exported", &[]), false, None),
                Err(e) => app.toast(e, true, None),
            },
            channel::Event::Msg(Msg::Accessibility(window, action)) => app.accessibility_event(window, action),
            channel::Event::Msg(Msg::Ipc(call)) => app.ipc_call(call),
            channel::Event::Msg(Msg::GpuLost(id)) => app.recover_gpu(id),
            channel::Event::Msg(Msg::Pty(id, bytes, permits)) => {
                // Retain permits until idle processing: producers cannot
                // continuously refill this source and starve input/rendering.
                app.queue_pty_output(id, bytes, permits);
            }
            channel::Event::Msg(Msg::PtyClosed(id)) => {
                app.handle.insert_idle(move |app| app.ssh_closed(id));
            }
            channel::Event::Msg(Msg::SshWorkspace(id, result)) => app.ssh_workspace_received(id, result),
            channel::Event::Msg(Msg::Clipboard(target, text, _permit)) => app.clipboard_received(target, text),
            channel::Event::Msg(Msg::NotifyActivated(pane, token)) => app.notification_clicked(pane, token),
            channel::Event::Msg(Msg::ImagesDecoded(id)) => app.pty_output(id, &[]),
            channel::Event::Msg(Msg::Plugin(key, m)) => {
                app.handle.insert_idle(move |app| app.plugin_msg(key, m));
            }
            channel::Event::Msg(Msg::PluginsResolved(generation, results)) => app.plugins_resolved(generation, results),
            channel::Event::Msg(Msg::Dropped(s, list)) => app.surface_dropped(s, &list),
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
        ipc_socket: None,
        xdg,
        viewporter,
        fractional_manager,
        blur_manager,
        blur_capable: false,
        blur_warned: false,
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
        file_dnd: None,
        notifier,
        activation,
        hint_patterns,
        recent: scroll::load_recent(),
        project: Ok(None),
        ssh_suggestions: Default::default(),
        ssh_transfer_path: None,
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
        sc: scripting::Scripting::default(),
        pl: plugins::Plugins::default(),
        pty_pending: Vec::new(),
    };
    let _ipc_server = if let Some(path) = listen.or_else(|| app.cfg.ipc_socket.clone()) {
        let path = if path.is_absolute() { path } else { std::env::current_dir()?.join(path) };
        let sender = app.sender.clone();
        let server = crate::ipc::Server::start(path, move |call| sender.send(Msg::Ipc(call)).is_ok())?;
        eprintln!("proteus: control socket {}", server.path.display());
        app.ipc_socket = Some(server.path.clone());
        Some(server)
    } else {
        None
    };
    app.setup_accessibility();
    app.watch_gpu();
    app.style_window();
    app.sc.config_path = config_path;
    app.sc.overrides = overrides;
    app.install_script(script);
    app.watch_config();
    app.plugins_init();
    if let Some(e) = config_error {
        app.toast(format!("config error (using the defaults): {e}"), true, Some(Duration::from_secs(15)));
    }

    // What to open: a layout, the saved session, or one pane.
    use crate::config::Restore;
    if app.cfg.restore != Restore::Never {
        app.session = SessionStore::acquire();
    }
    let saved = app.session.as_mut().and_then(SessionStore::read);
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
        app.run_scripts();
        event_loop.dispatch(None, &mut app)?;
        app.run_scripts();
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
        && let Some(pty) = app.panes.values_mut().next().and_then(|p| p.pty.as_mut())
    {
        pty.hangup();
        app.exit_code = pty.wait()?.code().unwrap_or(0);
    }
    let code = app.exit_code;
    drop(app);
    Ok(code)
}
