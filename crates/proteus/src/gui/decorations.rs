//! Client-side decorations (`window.decorations = "client"`): a title bar
//! with minimize, maximize and close buttons, a thin border, and resize
//! grips at the window edges, drawn with the UI theme.

use smithay_client_toolkit::reexports::csd_frame::{WindowManagerCapabilities, WindowState};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
use smithay_client_toolkit::shell::xdg::window::DecorationMode;

use super::*;

/// Width of the resize grips inside the window edges, in logical pixels.
const GRIP: f64 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CsdButton {
    Minimize,
    Maximize,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CsdHit {
    Button(CsdButton),
    Title,
    Edge(ResizeEdge),
}

/// What the compositor told us about the window (from `configure`).
#[derive(Debug, Clone, Copy)]
pub(super) struct CsdState {
    pub mode: DecorationMode,
    pub state: WindowState,
    pub caps: WindowManagerCapabilities,
    pub hover: Option<CsdButton>,
    pub pressed: Option<CsdButton>,
    pub last_title_click: Option<Instant>,
}

impl Default for CsdState {
    fn default() -> Self {
        CsdState {
            mode: DecorationMode::Server,
            state: WindowState::empty(),
            caps: WindowManagerCapabilities::all(),
            hover: None,
            pressed: None,
            last_title_click: None,
        }
    }
}

impl CsdHit {
    fn icon(self) -> CursorIcon {
        match self {
            CsdHit::Button(_) | CsdHit::Title => CursorIcon::Default,
            CsdHit::Edge(e) => match e {
                ResizeEdge::Top => CursorIcon::NResize,
                ResizeEdge::Bottom => CursorIcon::SResize,
                ResizeEdge::Left => CursorIcon::WResize,
                ResizeEdge::Right => CursorIcon::EResize,
                ResizeEdge::TopLeft => CursorIcon::NwResize,
                ResizeEdge::TopRight => CursorIcon::NeResize,
                ResizeEdge::BottomLeft => CursorIcon::SwResize,
                _ => CursorIcon::SeResize,
            },
        }
    }
}

impl App {
    /// We draw the decorations of the current window: always with "client",
    /// and with "auto" when the compositor leaves decorating to us.
    pub(super) fn csd(&self) -> bool {
        self.cfg.decorations != crate::config::Decorations::None
            && self.w.csd.mode == DecorationMode::Client
            && !self.w.csd.state.contains(WindowState::FULLSCREEN)
    }

    /// Height of the title bar in physical pixels (0 without one).
    pub(super) fn title_bar_h(&self) -> i32 {
        if self.csd() { (self.cell().1 + self.pad_y()) as i32 } else { 0 }
    }

    /// Maximized or tiled windows have no border and cannot be resized by their edges.
    fn floating_window(&self) -> bool {
        !self.w.csd.state.intersects(WindowState::MAXIMIZED | WindowState::TILED | WindowState::FULLSCREEN)
    }

    /// Buttons and their physical x range, left to right.
    fn csd_buttons(&self) -> Vec<(CsdButton, i32, i32)> {
        let caps = self.w.csd.caps;
        let all = caps.is_empty();
        let bw = 3 * self.cell().0 as i32;
        let mut x = self.physical().0 as i32 - self.pad() as i32;
        let mut out = Vec::new();
        for (b, cap) in [
            (CsdButton::Close, WindowManagerCapabilities::all()),
            (CsdButton::Maximize, WindowManagerCapabilities::MAXIMIZE),
            (CsdButton::Minimize, WindowManagerCapabilities::MINIMIZE),
        ] {
            if all || caps.intersects(cap) {
                out.push((b, x - bw, x));
                x -= bw;
            }
        }
        out.reverse();
        out
    }

    pub(super) fn csd_hit(&self, pos: (f64, f64)) -> Option<CsdHit> {
        if !self.csd() {
            return None;
        }
        let (x, y) = self.phys(pos);
        let (pw, ph) = self.physical();
        let (pw, ph) = (pw as i32, ph as i32);
        if self.floating_window() {
            let g = (GRIP * self.w.scale).round() as i32;
            let edge = match (y < g, y >= ph - g, x < g, x >= pw - g) {
                (true, _, true, _) => Some(ResizeEdge::TopLeft),
                (true, _, _, true) => Some(ResizeEdge::TopRight),
                (_, true, true, _) => Some(ResizeEdge::BottomLeft),
                (_, true, _, true) => Some(ResizeEdge::BottomRight),
                (true, ..) => Some(ResizeEdge::Top),
                (_, true, ..) => Some(ResizeEdge::Bottom),
                (_, _, true, _) => Some(ResizeEdge::Left),
                (_, _, _, true) => Some(ResizeEdge::Right),
                _ => None,
            };
            if let Some(e) = edge {
                return Some(CsdHit::Edge(e));
            }
        }
        if y >= self.title_bar_h() {
            return None;
        }
        let button = self.csd_buttons().into_iter().find(|&(_, x0, x1)| x >= x0 && x < x1);
        Some(button.map_or(CsdHit::Title, |b| CsdHit::Button(b.0)))
    }

    pub(super) fn csd_icon(&self, pos: (f64, f64)) -> Option<CursorIcon> {
        self.csd_hit(pos).map(CsdHit::icon)
    }

    /// Pointer events over the decorations. Returns true if they used it.
    pub(super) fn csd_pointer(&mut self, ev: &PointerEvent) -> bool {
        if !self.csd() {
            return false;
        }
        let hit = self.csd_hit(ev.position);
        let hover = match hit {
            Some(CsdHit::Button(b)) => Some(b),
            _ => None,
        };
        if hover != self.w.csd.hover {
            self.w.csd.hover = hover;
            self.w.dirty = true;
        }
        match ev.kind {
            PointerEventKind::Leave { .. } => {
                self.w.csd.hover = None;
                self.w.csd.pressed = None;
                self.w.dirty = true;
                false
            }
            PointerEventKind::Motion { .. } => {
                let busy = self.w.mouse.drag.is_some()
                    || self.w.mouse.selecting.is_some()
                    || self.w.mouse.reported.is_some()
                    || self.w.surface_drag.is_some();
                if hit.is_some() && !busy {
                    self.update_pointer_icon();
                    return true;
                }
                false
            }
            PointerEventKind::Press { button, serial, .. } => {
                let Some(hit) = hit else { return false };
                self.serial = serial;
                self.press_serial = serial;
                let Some(seat) = self.seat_state.seats().next() else { return true };
                match (hit, button) {
                    (CsdHit::Button(b), BTN_LEFT) => {
                        self.w.csd.pressed = Some(b);
                        self.w.dirty = true;
                    }
                    (CsdHit::Title, BTN_LEFT) => {
                        let now = Instant::now();
                        let double = self.w.csd.last_title_click.is_some_and(|t| now - t < DOUBLE_CLICK);
                        self.w.csd.last_title_click = if double { None } else { Some(now) };
                        if double {
                            self.toggle_maximized();
                        } else {
                            self.w.window.move_(&seat, serial);
                        }
                    }
                    (CsdHit::Title, BTN_RIGHT) => {
                        let (x, y) = ev.position;
                        self.w.window.show_window_menu(&seat, serial, (x as i32, y as i32));
                    }
                    (CsdHit::Edge(e), BTN_LEFT) => self.w.window.resize(&seat, serial, e),
                    _ => {}
                }
                true
            }
            PointerEventKind::Release { button, .. } => {
                let Some(pressed) = self.w.csd.pressed.take() else { return false };
                self.w.dirty = true;
                if button == BTN_LEFT && hit == Some(CsdHit::Button(pressed)) {
                    match pressed {
                        CsdButton::Minimize => self.w.window.set_minimized(),
                        CsdButton::Maximize => self.toggle_maximized(),
                        CsdButton::Close => self.request_close_window(),
                    }
                }
                true
            }
            PointerEventKind::Enter { .. } | PointerEventKind::Axis { .. } => hit.is_some(),
        }
    }

    fn toggle_maximized(&mut self) {
        if self.w.csd.state.contains(WindowState::MAXIMIZED) {
            self.w.window.unset_maximized();
        } else {
            self.w.window.set_maximized();
        }
    }

    /// Title bar and border, drawn over everything.
    pub(super) fn csd_parts(&self, theme: &Theme) -> (Vec<([f32; 4], proteus_core::color::Rgb)>, Vec<render::Label>) {
        let (mut rects, mut labels) = (Vec::new(), Vec::new());
        if !self.csd() {
            return (rects, labels);
        }
        let (pw, ph) = self.physical();
        let (pw, ph) = (pw as f32, ph as f32);
        let (cw, ch) = (self.cell().0 as f32, self.cell().1 as f32);
        let th = self.title_bar_h() as f32;
        let y = ((th - ch) / 2.0).floor();
        let active = self.w.focused || self.w.csd.state.contains(WindowState::ACTIVATED);
        let fg = if active { theme.fg } else { theme.dim };
        rects.push(([0.0, 0.0, pw, th], theme.bar));

        let buttons = self.csd_buttons();
        let buttons_x = buttons.first().map_or(pw, |b| b.1 as f32);
        let maximized = self.w.csd.state.contains(WindowState::MAXIMIZED);
        let red = self.focused_pane().term.palette().indexed(1);
        for (b, x0, _) in &buttons {
            let glyph = match b {
                CsdButton::Minimize => "─",
                CsdButton::Maximize if maximized => "❐",
                CsdButton::Maximize => "□",
                CsdButton::Close => "✕",
            };
            let lit = self.w.csd.hover == Some(*b) || self.w.csd.pressed == Some(*b);
            let (fg, bg) = match (b, lit) {
                (CsdButton::Close, true) => (theme.bg, red),
                (_, true) => (theme.bg, theme.accent),
                _ => (fg, theme.bar),
            };
            rects.push(([*x0 as f32, 0.0, 3.0 * cw, th], bg));
            labels.push(render::Label {
                x: *x0 as f32,
                y,
                cols: 3,
                text: format!(" {glyph} "),
                fg,
                bg: None,
                bold: false,
            });
        }

        // The title is centered in the window, clipped before the buttons.
        let room = (((buttons_x - self.pad() as f32) / cw).floor() as usize).saturating_sub(1).max(1);
        let title = if self.w.window_title.is_empty() { "proteus" } else { &self.w.window_title };
        let cols = chrome::text_width(title).min(room);
        let center = ((pw - cols as f32 * cw) / 2.0).max(self.pad() as f32);
        let x = center.min(buttons_x - cw - cols as f32 * cw).max(self.pad() as f32);
        labels.push(render::Label { x, y, cols, text: title.to_string(), fg, bg: None, bold: active });

        if self.floating_window() {
            let t = self.w.scale.round().max(1.0) as f32;
            let line = if active { theme.accent } else { theme.dim };
            rects.extend([
                ([0.0, 0.0, pw, t], line),
                ([0.0, ph - t, pw, t], line),
                ([0.0, 0.0, t, ph], line),
                ([pw - t, 0.0, t, ph], line),
            ]);
        }
        (rects, labels)
    }
}
