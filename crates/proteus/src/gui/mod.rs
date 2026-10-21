//! The Wayland frontend: one xdg-toplevel window running one terminal session,
//! rendered with wgpu.
//!
//! Event loop: calloop drives the Wayland connection, a channel fed by the PTY
//! reader thread, and timers. Frames are drawn only when something changed
//! and only after the compositor's frame callback (no busy loop).

mod font;
mod ime;
mod keys;
mod render;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use proteus_core::event::{ClipboardKind, Terminator};
use proteus_core::input::{self, KeyAction, MouseAction, MouseButton, MouseEvent, Mods};
use proteus_core::selection::SelectionKind;
use proteus_core::{ClipboardRead, Event, Point, TermMode, Terminal};
use proteus_pty::Pty;
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

use crate::config::{Blink, Config};
use crate::session::{self, SpawnOptions};
use font::Fonts;
use render::{FrameInfo, Overlay, Renderer};

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

/// What clipboard contents are read for.
#[derive(Debug, Clone, Copy)]
enum ClipTarget {
    /// Paste into the PTY (subject to paste protection).
    Paste,
    /// Answer an approved OSC 52 read.
    Osc52(ClipboardKind, Terminator),
}

/// A question (or notice) shown at the bottom of the window. While one is
/// up, keys answer it instead of going to the application.
#[derive(Debug)]
enum Prompt {
    ClipboardRead(ClipboardKind, Terminator),
    Paste(String),
    Notice(String),
}

enum Msg {
    Pty(Vec<u8>),
    PtyClosed,
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
    /// Button reported to the application while held.
    reported: Option<MouseButton>,
    /// Left button held for a local selection.
    selecting: bool,
    /// Anchor of a single click; becomes a selection once the pointer moves.
    pending: Option<Point>,
    last_click: Option<(Instant, Point, u8)>,
    scroll_acc: f64,
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
    fonts: Fonts,
    font_size: f32,
    prompts: VecDeque<Prompt>,
    /// OSC 52 reads allowed for the rest of the session ("always").
    osc52_always: bool,
    /// Blink phase: cursor hidden.
    blink_off: bool,
    blink_timer: Option<RegistrationToken>,
    last_input: Instant,
    term: Terminal,
    pty: Pty,
    writer: File,

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
    serial: u32,
    clipboard: Option<(CopyPasteSource, String)>,
    primary: Option<(PrimarySelectionSource, String)>,
    mouse: Mouse,
    exit: bool,
}

pub fn run(cfg: Config, config_error: Option<String>) -> io::Result<i32> {
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

    // Terminal + PTY.
    let spawn = SpawnOptions {
        command: cfg.command.clone(),
        integration: cfg.shell_integration,
        cols: 80,
        rows: 24,
        cell: Some((m.cell_w as u16, m.cell_h as u16)),
    };
    let (pty, _shell) = session::spawn_pty(&spawn)?;
    let mut term = Terminal::new(80, 24, cfg.core());
    term.set_cell_pixel_size(m.cell_w, m.cell_h);
    let writer = pty.try_clone_master()?;
    let mut reader = pty.try_clone_master()?;
    let (sender, rx) = channel::channel();
    let tx = sender.clone();
    std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Msg::Pty(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(Msg::PtyClosed);
    })?;
    handle
        .insert_source(rx, |ev, _, app: &mut App| match ev {
            channel::Event::Msg(Msg::Pty(bytes)) => app.pty_output(&bytes),
            channel::Event::Msg(Msg::Clipboard(target, text)) => app.clipboard_received(target, text),
            channel::Event::Msg(Msg::PtyClosed) | channel::Event::Closed => app.exit = true,
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
        fonts,
        font_size: cfg.font_size,
        cfg,
        prompts: config_error.map(|e| Prompt::Notice(format!("config error: {e}"))).into_iter().collect(),
        osc52_always: false,
        blink_off: false,
        blink_timer: None,
        last_input: Instant::now(),
        term,
        pty,
        writer,
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
        serial: 0,
        clipboard: None,
        primary: None,
        mouse: Mouse::default(),
        exit: false,
    };

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
        let mut info = app.frame_info();
        // PROTEUS_PREEDIT=text[@byte] previews a composition (testing aid).
        if let Some(pre) = std::env::var("PROTEUS_PREEDIT").ok().filter(|s| !s.is_empty()) {
            let (text, cursor) = match pre.rsplit_once('@') {
                Some((t, c)) => (t.to_string(), c.parse().ok().map(|c: usize| (c, c))),
                None => (pre.clone(), Some((pre.len(), pre.len()))),
            };
            info.preedit = Some(ime::Preedit { text, cursor });
            info.focused = true;
        }
        let (w, h, px) = app.renderer.snapshot(&app.term, &mut app.fonts, &info);
        if let Err(e) = image::save_buffer(&path, &px, w, h, image::ColorType::Rgba8) {
            eprintln!("proteus: snapshot: {e}");
        }
    }
    app.pty.hangup();
    let status = app.pty.wait()?;
    drop(app);
    Ok(status.code().unwrap_or(0))
}

impl App {
    // --- geometry ------------------------------------------------------------

    fn pad(&self) -> u32 {
        (PAD * self.scale).round() as u32
    }

    fn physical(&self) -> (u32, u32) {
        ((self.size.0 as f64 * self.scale).round() as u32, (self.size.1 as f64 * self.scale).round() as u32)
    }

    /// Recompute surface size and grid after a size, scale or font change.
    fn apply_size(&mut self) {
        let (pw, ph) = self.physical();
        if let Some(vp) = &self.viewport {
            vp.set_destination(self.size.0 as i32, self.size.1 as i32);
        }
        self.renderer.resize(pw, ph);
        let m = self.fonts.metrics();
        let pad = self.pad() * 2;
        let cols = (pw.saturating_sub(pad) / m.cell_w).max(2) as u16;
        let rows = (ph.saturating_sub(pad) / m.cell_h).max(1) as u16;
        self.term.set_cell_pixel_size(m.cell_w, m.cell_h);
        if cols as usize != self.term.cols() || rows as usize != self.term.lines() {
            self.term.resize(cols as usize, rows as usize);
        }
        let cell = Some((m.cell_w as u16, m.cell_h as u16));
        let _ = self.pty.resize(session::window_size(cols, rows, cell));
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

    /// Grid cell (col, viewport row) under a surface-local logical position.
    fn cell_at(&self, x: f64, y: f64) -> (usize, usize) {
        let m = self.fonts.metrics();
        let pad = self.pad() as f64;
        let px = (x * self.scale - pad).max(0.0);
        let py = (y * self.scale - pad).max(0.0);
        let col = ((px / m.cell_w as f64) as usize).min(self.term.cols() - 1);
        let row = ((py / m.cell_h as f64) as usize).min(self.term.lines() - 1);
        (col, row)
    }

    fn point_at(&self, x: f64, y: f64) -> Point {
        let (col, row) = self.cell_at(x, y);
        Point::new(self.term.grid().viewport_line(row), col)
    }

    // --- drawing -------------------------------------------------------------

    fn frame_info(&self) -> FrameInfo {
        FrameInfo {
            focused: self.focused,
            pad: (self.pad(), self.pad()),
            cursor_blink_off: self.blink_off,
            ligatures: self.cfg.ligatures,
            preedit: self.ime.as_ref().and_then(|i| i.preedit.clone()),
            overlay: self.prompts.front().map(Prompt::overlay),
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn window_surface(&self) -> &WlSurface {
        self.window.wl_surface()
    }

    // --- input method ----------------------------------------------------------

    /// Text committed by the input method is sent as if typed.
    pub(crate) fn commit_text(&mut self, text: &str) {
        self.restart_blink();
        self.term.scroll_to_bottom();
        self.write_pty(text.as_bytes());
    }

    /// Tell the input method where the cursor (or the preedit cursor) is.
    pub(crate) fn update_ime_rect(&mut self) {
        let m = self.fonts.metrics();
        let pad = self.pad() as f64;
        let cursor = self.term.cursor();
        let (cols, x, y) = (self.term.cols(), cursor.x, cursor.y);
        let Some(ime) = self.ime.as_mut() else { return };
        let col = match &ime.preedit {
            Some(p) => render::preedit_start(x, p.width(), cols) + p.cursor_col().unwrap_or(0),
            None => x,
        }
        .min(cols.saturating_sub(1));
        let s = self.scale;
        let lx = ((pad + col as f64 * m.cell_w as f64) / s).round() as i32;
        let ly = ((pad + y as f64 * m.cell_h as f64) / s).round() as i32;
        let w = (m.cell_w as f64 / s).ceil() as i32;
        let h = (m.cell_h as f64 / s).ceil() as i32;
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
        if !self.configured || self.frame_pending {
            return;
        }
        // Mode 2026: hold the frame while the application is mid-update.
        if self.term.is_synchronized() {
            let since = *self.sync_since.get_or_insert_with(Instant::now);
            let held = since.elapsed();
            if held < SYNC_TIMEOUT {
                self.schedule(SYNC_TIMEOUT - held);
                return;
            }
        } else {
            self.sync_since = None;
        }
        let damage = self.term.take_damage();
        if damage.is_empty() && !self.dirty {
            return;
        }
        self.dirty = false;
        let surface = self.window.wl_surface().clone();
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let info = self.frame_info();
        if self.renderer.draw(&self.term, &mut self.fonts, &info) {
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

    // --- PTY -----------------------------------------------------------------

    fn write_pty(&mut self, bytes: &[u8]) {
        if let Err(e) = self.writer.write_all(bytes) {
            eprintln!("proteus: write to pty: {e}");
        }
    }

    fn pty_output(&mut self, bytes: &[u8]) {
        self.term.advance(bytes);
        self.ensure_blink();
        let reply = self.term.take_pty_output();
        if !reply.is_empty() {
            self.write_pty(&reply);
        }
        for ev in self.term.take_events() {
            match ev {
                Event::TitleChanged(t) => self.window.set_title(if t.is_empty() { "proteus".to_string() } else { t }),
                Event::ClipboardStore { kind, text } => self.set_selection(kind, text),
                Event::ClipboardLoad { kind, terminator } => self.clipboard_load(kind, terminator),
                _ => {}
            }
        }
    }

    // --- keyboard ------------------------------------------------------------

    fn key(&mut self, event: KeyEvent, action: KeyAction) {
        if keys::is_modifier(event.keysym) {
            return;
        }
        let mods = keys::mods(&self.mods);
        let Some((key, shifted)) = self.levels.key(event.keysym, event.raw_code) else { return };
        if action != KeyAction::Release {
            self.restart_blink();
            if !self.prompts.is_empty() {
                self.answer_prompt(key);
                return;
            }
        }
        if action != KeyAction::Release && self.shortcut(key, mods) {
            return;
        }
        let text = event.utf8.as_deref().unwrap_or("");
        let ev = input::KeyEvent { key, mods, action, text, shifted };
        if let Some(bytes) = input::encode_key(&ev, self.term.mode(), self.term.keyboard_flags()) {
            if action != KeyAction::Release {
                self.term.scroll_to_bottom();
            }
            self.write_pty(&bytes);
        }
    }

    /// Built-in key bindings. Returns true if the key was consumed.
    fn shortcut(&mut self, key: input::Key, mods: Mods) -> bool {
        use input::Key;
        let m = mods & (Mods::SHIFT | Mods::CTRL | Mods::ALT | Mods::SUPER);
        let cs = Mods::CTRL | Mods::SHIFT;
        let page = self.term.lines() as isize - 1;
        let primary_screen = !self.term.is_alt_screen();
        match (m, key) {
            (m, Key::Char('c')) if m == cs => {
                if let Some(text) = self.term.selection_text().filter(|t| !t.is_empty()) {
                    self.set_selection(ClipboardKind::Clipboard, text);
                }
            }
            (m, Key::Char('v')) if m == cs => self.paste(ClipboardKind::Clipboard),
            (Mods::SHIFT, Key::Insert) => self.paste(ClipboardKind::Primary),
            (m, Key::Char('=' | '+')) if m == cs || m == Mods::CTRL => self.set_font_size(self.font_size + 1.0),
            (m, Key::Char('-')) if m == cs || m == Mods::CTRL => self.set_font_size(self.font_size - 1.0),
            (m, Key::Char('0')) if m == cs || m == Mods::CTRL => self.set_font_size(self.cfg.font_size),
            (m, Key::Char('z')) if m == cs => {
                self.term.scroll_to_prompt(false);
            }
            (m, Key::Char('x')) if m == cs => {
                self.term.scroll_to_prompt(true);
            }
            (Mods::SHIFT, Key::PageUp) if primary_screen => self.term.scroll_display(page),
            (Mods::SHIFT, Key::PageDown) if primary_screen => self.term.scroll_display(-page),
            (Mods::SHIFT, Key::Home) if primary_screen => self.term.scroll_display(isize::MAX / 2),
            (Mods::SHIFT, Key::End) if primary_screen => self.term.scroll_to_bottom(),
            _ => return false,
        }
        true
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
        self.read_clipboard(kind, ClipTarget::Paste);
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
            ClipTarget::Paste => self.paste_text(text),
            ClipTarget::Osc52(kind, terminator) => {
                self.term.respond_clipboard(kind, &text, terminator);
                let reply = self.term.take_pty_output();
                self.write_pty(&reply);
            }
        }
    }

    /// Paste, asking first if the text could run commands unseen.
    fn paste_text(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.cfg.paste_protection && needs_paste_confirmation(&text, self.term.mode()) {
            if !self.prompts.iter().any(|p| matches!(p, Prompt::Paste(_))) {
                self.prompts.push_back(Prompt::Paste(text));
                self.dirty = true;
            }
            return;
        }
        self.paste_now(&text);
    }

    fn paste_now(&mut self, text: &str) {
        let bytes = input::encode_paste(text, self.term.mode());
        self.term.scroll_to_bottom();
        self.write_pty(&bytes);
    }

    // --- prompts ---------------------------------------------------------------

    fn clipboard_load(&mut self, kind: ClipboardKind, terminator: Terminator) {
        if self.cfg.osc52_read == ClipboardRead::Allow || self.osc52_always {
            self.read_clipboard(kind, ClipTarget::Osc52(kind, terminator));
        } else if !self.prompts.iter().any(|p| matches!(p, Prompt::ClipboardRead(..))) {
            // One question at a time; repeated queries while it is up are dropped.
            self.prompts.push_back(Prompt::ClipboardRead(kind, terminator));
            self.dirty = true;
        }
    }

    fn answer_prompt(&mut self, key: input::Key) {
        use input::Key;
        let Some(p) = self.prompts.pop_front() else { return };
        let yes = matches!(key, Key::Char('y' | 'Y'));
        match p {
            Prompt::ClipboardRead(kind, terminator) => match key {
                Key::Char('a' | 'A') => {
                    self.osc52_always = true;
                    self.read_clipboard(kind, ClipTarget::Osc52(kind, terminator));
                }
                _ if yes => self.read_clipboard(kind, ClipTarget::Osc52(kind, terminator)),
                Key::Char('n' | 'N') | Key::Escape | Key::Enter => {}
                _ => self.prompts.push_front(Prompt::ClipboardRead(kind, terminator)),
            },
            Prompt::Paste(text) => match key {
                Key::Enter => self.paste_now(&text),
                _ if yes => self.paste_now(&text),
                Key::Char('n' | 'N') | Key::Escape => {}
                _ => self.prompts.push_front(Prompt::Paste(text)),
            },
            Prompt::Notice(_) => {}
        }
        self.dirty = true;
    }

    // --- cursor blink ----------------------------------------------------------

    fn wants_blink(&self) -> bool {
        let app = match self.cfg.cursor_blink {
            Blink::Never => false,
            Blink::Always => true,
            Blink::App => self.term.mode().contains(TermMode::CURSOR_BLINK),
        };
        app && self.focused && self.term.cursor_visible() && self.last_input.elapsed() < BLINK_IDLE
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
                if app.wants_blink() {
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

    fn mouse_reporting(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_ANY) && !self.mods.shift
    }

    fn report_mouse(&mut self, action: MouseAction, button: Option<MouseButton>) -> bool {
        let (col, line) = self.cell_at(self.mouse.pos.0, self.mouse.pos.1);
        let pad = self.pad() as f64;
        let pixel = Some((
            (self.mouse.pos.0 * self.scale - pad).max(0.0) as u32,
            (self.mouse.pos.1 * self.scale - pad).max(0.0) as u32,
        ));
        let ev = MouseEvent { action, button, col, line, pixel, mods: keys::mods(&self.mods) };
        match input::encode_mouse(&ev, self.term.mode()) {
            Some(bytes) => {
                self.write_pty(&bytes);
                true
            }
            None => false,
        }
    }

    fn pointer_event(&mut self, ev: &PointerEvent) {
        self.mouse.pos = ev.position;
        match ev.kind {
            PointerEventKind::Enter { .. } => {
                if let Some(p) = &self.pointer {
                    let _ = p.set_cursor(&self.conn, CursorIcon::Text);
                }
            }
            PointerEventKind::Leave { .. } => {}
            PointerEventKind::Motion { .. } => {
                if self.mouse.selecting {
                    let p = self.point_at(ev.position.0, ev.position.1);
                    if let Some(anchor) = self.mouse.pending.take() {
                        if anchor != p {
                            let kind = if self.mods.alt { SelectionKind::Block } else { SelectionKind::Simple };
                            self.term.start_selection(kind, anchor);
                        } else {
                            self.mouse.pending = Some(anchor);
                        }
                    }
                    if self.mouse.pending.is_none() {
                        self.term.update_selection(p);
                    }
                } else if self.mouse_reporting() {
                    let held = self.mouse.reported;
                    self.report_mouse(MouseAction::Motion, held);
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
                if self.mouse_reporting() && self.report_mouse(MouseAction::Press, Some(b)) {
                    self.mouse.reported = Some(b);
                    return;
                }
                match b {
                    MouseButton::Left => self.click(ev.position),
                    MouseButton::Middle => self.paste(ClipboardKind::Primary),
                    _ => {}
                }
            }
            PointerEventKind::Release { button, .. } => {
                if let Some(b) = self.mouse.reported.take() {
                    self.report_mouse(MouseAction::Release, Some(b));
                    return;
                }
                if button == BTN_LEFT && self.mouse.selecting {
                    self.mouse.selecting = false;
                    self.mouse.pending = None;
                    if let Some(text) = self.term.selection_text().filter(|t| !t.is_empty()) {
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
                    let line_h = self.fonts.metrics().cell_h as f64 / self.scale;
                    vertical.absolute / line_h / 3.0
                };
                self.mouse.scroll_acc += notches;
                let n = self.mouse.scroll_acc.trunc();
                self.mouse.scroll_acc -= n;
                if n == 0.0 {
                    return;
                }
                let n = n as i32;
                if self.mouse_reporting() {
                    let b = if n < 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
                    for _ in 0..n.unsigned_abs().min(10) {
                        self.report_mouse(MouseAction::Press, Some(b));
                    }
                } else if let Some(bytes) = input::alternate_scroll(-n * 3, self.term.mode()) {
                    self.write_pty(&bytes);
                } else {
                    self.term.scroll_display((-n * 3) as isize);
                }
            }
        }
    }

    fn click(&mut self, pos: (f64, f64)) {
        let p = self.point_at(pos.0, pos.1);
        let now = Instant::now();
        let count = match self.mouse.last_click {
            Some((t, q, n)) if now.duration_since(t) < DOUBLE_CLICK && q == p => n % 3 + 1,
            _ => 1,
        };
        self.mouse.last_click = Some((now, p, count));
        self.mouse.selecting = true;
        match count {
            1 => {
                self.term.clear_selection();
                self.mouse.pending = Some(p);
            }
            n => {
                self.mouse.pending = None;
                let kind = if n == 2 { SelectionKind::Word } else { SelectionKind::Line };
                self.term.start_selection(kind, p);
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
        self.exit = true;
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
        self.configured = true;
        self.apply_size();
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
            if let Some(b) = input::encode_focus(true, self.term.mode()) {
                self.write_pty(b);
            }
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, s: &WlSurface, _: u32) {
        if s == self.window.wl_surface() {
            self.focused = false;
            self.dirty = true;
            if let Some(b) = input::encode_focus(false, self.term.mode()) {
                self.write_pty(b);
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
            Prompt::ClipboardRead(kind, _) => {
                let which = if matches!(kind, ClipboardKind::Primary) { "primary selection" } else { "clipboard" };
                Overlay {
                    title: format!("The application wants to read the {which}."),
                    detail: None,
                    keys: "[y] allow once   [a] always (this session)   [n] deny".into(),
                }
            }
            Prompt::Paste(text) => {
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
        let o = Prompt::Paste("echo 1\necho 2\n".into()).overlay();
        assert!(o.title.contains("2 lines"), "{o:?}");
        assert_eq!(o.detail.as_deref(), Some("echo 1"));
        let o = Prompt::Paste("a\x1bb".into()).overlay();
        assert!(o.title.contains("control"), "{o:?}");
        assert!(o.detail.unwrap().contains('\u{fffd}'));
    }
}
