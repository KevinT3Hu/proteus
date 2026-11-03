//! Wayland protocol handlers: compositor, windows, seat, keyboard, pointer,
//! data devices (clipboard and drag and drop), activation.

use super::*;

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
        let suspended = cfg.state.contains(smithay_client_toolkit::reexports::csd_frame::WindowState::SUSPENDED);
        if self.w.suspended && !suspended {
            // A hidden surface may never receive its old frame callback.
            self.w.frame_pending = false;
            self.w.dirty = true;
        }
        self.w.suspended = suspended;
        self.w.csd.mode = cfg.decoration_mode;
        self.w.csd.state = cfg.state;
        self.w.csd.caps = cfg.capabilities;
        self.w.dirty = true;
        let (w, h) = cfg.new_size;
        let default = if self.w.configured {
            self.w.size
        } else {
            // A compositor-provided dimension takes precedence on that axis.
            // Recompute at the negotiated scale, preserving inherited sizes for new windows.
            if self.w.id == 1 || self.cfg.window != Default::default() {
                let m = self.w.fonts.metrics();
                self.cfg.window_size((m.cell_w, m.cell_h), self.w.scale)
            } else {
                self.w.size
            }
        };
        self.w.size = (w.map_or(default.0, |v| v.get()), h.map_or(default.1, |v| v.get()));
        let focused = cfg.is_activated();
        if focused != self.w.focused && self.keyboard.is_none() {
            self.w.focused = focused;
        }
        let first = !self.w.configured;
        self.w.configured = true;
        if first {
            self.apply_size();
        } else {
            self.w.size_pending = true;
        }
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
        if self.tab_dnd.is_none() {
            self.file_dnd = Some((win, (x, y)));
            return self.file_drag_moved();
        }
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
        self.file_dnd = None;
    }
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64) {
        if let Some((_, pos)) = self.tab_dnd.as_mut().and_then(|d| d.at.as_mut()) {
            *pos = (x, y);
        }
        if let Some((_, pos)) = self.file_dnd.as_mut() {
            *pos = (x, y);
            self.file_drag_moved();
        }
    }
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        let offer = self.data_device.as_ref().and_then(|d| d.data().drag_offer());
        if let Some((win, pos)) = self.file_dnd.take() {
            let surface = if self.activate(win) { self.drop_surface_at(pos) } else { None };
            self.activate_kbd();
            let (Some(o), Some(s)) = (offer, surface) else { return };
            let Ok(pipe) = o.receive(URI_LIST.into()) else { return };
            let tx = self.sender.clone();
            let _ = std::thread::Builder::new().name("drop".into()).spawn(move || {
                let data = transfer::read(File::from(OwnedFd::from(pipe)), 1 << 20, Duration::from_secs(3))
                    .unwrap_or_default();
                o.finish();
                o.destroy();
                let _ = tx.send(Msg::Dropped(s, String::from_utf8_lossy(&data).into_owned()));
            });
            return;
        }
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

const URI_LIST: &str = "text/uri-list";

impl App {
    /// Files are accepted over plugin surfaces only.
    fn file_drag_moved(&mut self) {
        let Some((win, pos)) = self.file_dnd else { return };
        let over = self.activate(win) && self.drop_surface_at(pos).is_some();
        self.activate_kbd();
        let Some(offer) = self.data_device.as_ref().and_then(|d| d.data().drag_offer()) else { return };
        if over && offer.with_mime_types(|m| m.iter().any(|m| m == URI_LIST)) {
            offer.accept_mime_type(offer.serial, Some(URI_LIST.into()));
            offer.set_actions(DndAction::Copy, DndAction::Copy);
        } else {
            offer.accept_mime_type(offer.serial, None);
        }
    }

    /// The plugin surface (or plugin pane) under a position.
    fn drop_surface_at(&self, pos: (f64, f64)) -> Option<u32> {
        self.surface_at(pos).map(|s| s.0).or_else(|| self.panes.get(&self.pane_at(pos)?)?.surface)
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
