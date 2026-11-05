//! Pointer input: mouse reporting, clicks and selections, wheel, dragging
//! dividers, floating panes and tabs, pointer icons.

use super::*;

impl App {
    pub(super) fn mouse_reporting(&self, id: PaneId) -> bool {
        self.panes.get(&id).is_some_and(|p| p.term.mode().intersects(TermMode::MOUSE_ANY)) && !self.mods.shift
    }

    pub(super) fn report_mouse(&mut self, id: PaneId, action: MouseAction, button: Option<MouseButton>) -> bool {
        let Some((col, line, pixel)) = self.cell_in(id, self.w.mouse.pos) else { return false };
        let t = &self.panes.get(&id).unwrap().term;
        let map = t.visual_row(t.grid().viewport_line(line));
        let logical = map.logical(col);
        let col = logical + col.saturating_sub(map.visual(logical));
        let pixel = (col as u32 * self.cell().0 + pixel.0 % self.cell().0, pixel.1);
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
    pub(super) fn tab_at(&self, pos: (f64, f64)) -> Option<Option<usize>> {
        if !self.tab_bar_visible() {
            return None;
        }
        let (x, y) = self.phys(pos);
        if y < self.title_bar_h() || y >= self.area().y {
            return None;
        }
        let labels = self.tab_labels(&self.theme());
        Some(chrome::tab_at(&labels, x as f32, self.cell().0 as f32))
    }

    pub(super) fn pointer_event(&mut self, ev: &PointerEvent) {
        self.w.mouse.pos = ev.position;
        if self.csd_pointer(ev) {
            return;
        }
        if self.w.prompts.is_empty() && self.completion_pointer(ev) {
            return;
        }
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
                if self.surface_motion(ev.position) {
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
                            self.request_close(ids, "ui-this-tab");
                        }
                        _ => {}
                    }
                    return;
                }
                if let Some((s, cell)) = self.surface_at(ev.position) {
                    if let Some(pb) = surface_button(b) {
                        self.surface_press(s, cell, pb);
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
                // The pinned command scrolls back to its prompt.
                if b == MouseButton::Left
                    && let Some((id, block)) = self.sticky_hit(ev.position)
                {
                    self.reveal_prompt(id, block);
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
                self.unfocus_surface();
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
                if let Some(s) = self.panes.get(&id).and_then(|p| p.surface)
                    && let Some(pb) = surface_button(b)
                {
                    let cell = self.cell_in(id, ev.position).map(|(c, r, _)| (c as u32, r as u32));
                    self.surface_press(s, cell, pb);
                    return;
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
                if self.surface_release(ev.position) {
                    return;
                }
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
                let pane_surface = self
                    .pane_at(ev.position)
                    .and_then(|id| Some((self.panes.get(&id)?.surface?, self.cell_in(id, ev.position)?)))
                    .map(|(s, (c, r, _))| (s, Some((c as u32, r as u32))));
                if let Some((s, Some((col, row)))) = self.surface_at(ev.position).or(pane_surface) {
                    use proteus_wasm::types::{MouseAction as A, MouseButton as P, MouseEvent, UiEvent};
                    let button = if n < 0 { P::WheelUp } else { P::WheelDown };
                    let mods = proteus_wasm::types::Modifiers::empty();
                    for _ in 0..n.unsigned_abs().min(10) {
                        let action = A::Press;
                        self.surface_ui(s, UiEvent::Mouse(MouseEvent { button, action, col, row, mods }));
                    }
                    return;
                }
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

    pub(super) fn divider_at(&self, pos: (f64, f64)) -> Option<layout::Divider> {
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
    pub(super) fn update_pointer_icon(&mut self) {
        let icon = if let Some(icon) = self.csd_icon(self.w.mouse.pos) {
            icon
        } else {
            match (self.divider_at(self.w.mouse.pos), self.hit_at(self.w.mouse.pos)) {
                _ if self.w.viewer.is_some() => CursorIcon::Grab,
                _ if self.w.hover.is_some() => CursorIcon::Pointer,
                (Some(d), _) if d.axis == Axis::X => CursorIcon::ColResize,
                (Some(_), _) => CursorIcon::RowResize,
                _ if self.tab_at(self.w.mouse.pos).is_some() => CursorIcon::Default,
                (None, Some((_, layout::Part::FloatFrame))) => CursorIcon::Move,
                (None, Some((_, layout::Part::FloatCorner))) => CursorIcon::SeResize,
                (None, Some((_, layout::Part::Title))) | (None, None) => CursorIcon::Default,
                (None, Some(_)) => CursorIcon::Text,
            }
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

/// A mouse button as plugins see it.
fn surface_button(b: MouseButton) -> Option<proteus_wasm::types::MouseButton> {
    use proteus_wasm::types::MouseButton as P;
    Some(match b {
        MouseButton::Left => P::Left,
        MouseButton::Middle => P::Middle,
        MouseButton::Right => P::Right,
        _ => return None,
    })
}
