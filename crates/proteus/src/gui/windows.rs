//! Several windows: which one is current, opening and closing them, GPU
//! loss recovery, drawing all of them.

use super::*;

impl App {
    /// Make window `id` the current one ([`App::w`]).
    pub(super) fn activate(&mut self, id: WinId) -> bool {
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

    pub(super) fn win_of_surface(&self, s: &WlSurface) -> Option<WinId> {
        std::iter::once(&self.w).chain(&self.parked).find(|w| w.window.wl_surface() == s).map(|w| w.id)
    }

    pub(super) fn activate_surface(&mut self, s: &WlSurface) -> bool {
        match self.win_of_surface(s) {
            Some(id) => self.activate(id),
            None => false,
        }
    }

    /// Window holding a pane.
    pub(super) fn win_of_pane(&self, pane: PaneId) -> Option<WinId> {
        std::iter::once(&self.w).chain(&self.parked).find(|w| w.tabs.iter().any(|t| t.contains(pane))).map(|w| w.id)
    }

    /// Make the window holding `pane` current. Returns false if no window does.
    pub(super) fn activate_pane(&mut self, pane: PaneId) -> bool {
        match self.win_of_pane(pane) {
            Some(id) => self.activate(id),
            None => false,
        }
    }

    /// Back to the keyboard window (where keys go).
    pub(super) fn activate_kbd(&mut self) {
        if let Some(id) = self.kbd_win {
            self.activate(id);
        }
    }

    pub(super) fn window_ids(&self) -> Vec<WinId> {
        let mut v: Vec<WinId> = std::iter::once(self.w.id).chain(self.parked.iter().map(|w| w.id)).collect();
        v.sort();
        v
    }

    pub(super) fn mark_all_dirty(&mut self) {
        self.w.dirty = true;
        for w in &mut self.parked {
            w.dirty = true;
        }
    }

    pub(super) fn watch_gpu(&self) {
        let sender = self.sender.clone();
        let id = self.w.id;
        let lost = self.w.renderer.lost.clone();
        self.w.renderer.device.set_device_lost_callback(move |reason, message| {
            if reason != wgpu::DeviceLostReason::Destroyed {
                lost.store(true, std::sync::atomic::Ordering::Release);
                eprintln!("proteus: GPU device lost: {message}");
                let _ = sender.send(Msg::GpuLost(id));
            }
        });
    }

    pub(super) fn recover_gpu(&mut self, id: WinId) {
        if !self.activate(id) {
            return;
        }
        let result = self.w.renderer.recover(&self.instance, self.physical());
        match result {
            Ok(mut renderer) => {
                renderer.images.budget = self.cfg.graphics.gpu_memory;
                self.w.renderer = renderer;
                self.w.styled = None;
                self.style_window();
                self.watch_gpu();
                self.w.frame_pending = false;
                self.w.dirty = true;
                eprintln!("proteus: GPU recovered with {}", self.w.renderer.adapter_info);
            }
            Err(e) => {
                eprintln!("proteus: GPU recovery failed: {e}; retrying in two seconds");
                let _ = self.handle.insert_source(Timer::from_duration(Duration::from_secs(2)), move |_, _, app| {
                    app.recover_gpu(id);
                    TimeoutAction::Drop
                });
            }
        }
    }

    /// Open a new (empty) window and make it current. Its size follows the
    /// current window.
    pub(super) fn new_window(&mut self) -> io::Result<WinId> {
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
            (self.cfg.window == Default::default()).then_some(self.w.size),
        )?;
        self.parked.push(win);
        self.activate(id);
        self.setup_accessibility();
        self.watch_gpu();
        self.style_window();
        Ok(id)
    }

    /// Apply opacity and blur to the current window. Blur covers the whole
    /// window and is only requested while the background is translucent.
    pub(super) fn style_window(&mut self) {
        let translucent = self.w.renderer.set_opacity(self.cfg.background_opacity) && self.cfg.background_opacity < 1.0;
        if self.cfg.background_opacity < 1.0 && !translucent {
            self.warn_once("window.opacity: the GPU surface cannot be presented with transparency");
        }
        let blur = translucent && self.cfg.background_blur;
        if self.cfg.background_blur && self.blur_manager.is_none() {
            self.warn_once("window.background_blur: the compositor does not support ext-background-effect-v1");
        }
        let state = (self.w.size, blur && self.blur_capable);
        if self.w.styled == Some(state) {
            return;
        }
        self.w.styled = Some(state);
        let Some(manager) = &self.blur_manager else { return };
        if !state.1 {
            if let Some(effect) = &self.w.blur {
                effect.set_blur_region(None);
            }
        } else {
            let surface = self.w.window.wl_surface().clone();
            let effect =
                self.w.blur.get_or_insert_with(|| manager.get_background_effect(&surface, &self.qh, ScaleData));
            if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
                region.add(0, 0, self.w.size.0 as i32, self.w.size.1 as i32);
                effect.set_blur_region(Some(region.wl_region()));
            }
        }
        self.w.dirty = true;
    }

    pub(super) fn style_all_windows(&mut self) {
        for id in self.window_ids() {
            if self.activate(id) {
                self.style_window();
            }
        }
        self.activate_kbd();
    }

    fn warn_once(&mut self, msg: &str) {
        if !self.blur_warned {
            self.blur_warned = true;
            self.toast(msg.into(), true, None);
        }
    }

    /// Close the current window (it must have no tabs left). The last
    /// window ends the program.
    pub(super) fn destroy_window(&mut self) {
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
    pub(super) fn draw_all(&mut self) {
        for id in self.window_ids() {
            if self.activate(id) {
                self.update_accessibility();
                self.draw();
            }
        }
        self.activate_kbd();
    }

    pub(super) fn snapshot(&mut self, path: &std::path::Path) {
        if self.w.renderer.lost.load(std::sync::atomic::Ordering::Acquire) {
            eprintln!("proteus: snapshot unavailable while the GPU is recovering");
            return;
        }
        let mut parts = self.frame_parts();
        // PROTEUS_PREEDIT=text[@byte] previews a composition (testing aid).
        if let Some(pre) = std::env::var("PROTEUS_PREEDIT").ok().filter(|s| !s.is_empty()) {
            let (text, cursor) = match pre.rsplit_once('@') {
                Some((t, c)) => (t.to_string(), c.parse().ok().map(|c: usize| (c, c))),
                None => (pre.clone(), Some((pre.len(), pre.len()))),
            };
            let fid = self.focused_id();
            let sf = self.w.surface_focus;
            let target = |p: &PanePart| if sf.is_some() { p.surface == sf } else { p.id == fid && p.surface.is_none() };
            for p in parts.panes.iter_mut().filter(|p| target(p)) {
                p.focused = true;
                p.preedit = Some(ime::Preedit { text: text.clone(), cursor });
            }
        }
        let area = self.viewer_area();
        let mut info = parts.resolve(&self.panes, &self.pl.surfaces);
        info.viewer = self.w.viewer.as_ref().map(|v| v.frame(area));
        let (w, h, px) = self.w.renderer.snapshot(&mut self.w.fonts, &info);
        drop(info);
        if let Err(e) = image::save_buffer(path, &px, w, h, image::ColorType::Rgba8) {
            eprintln!("proteus: snapshot: {e}");
        }
    }
}
