//! Clipboard and primary selection: offering, reading, pasting.

use super::*;

impl App {
    pub(super) fn set_selection(&mut self, kind: ClipboardKind, text: String) {
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

    pub(super) fn paste(&mut self, kind: ClipboardKind) {
        let id = self.focused_id();
        self.read_clipboard(kind, ClipTarget::Paste(id));
    }

    /// Fetch a selection's text; the result arrives as `Msg::Clipboard`
    /// (immediately if we own the selection).
    pub(super) fn read_clipboard(&mut self, kind: ClipboardKind, target: ClipTarget) {
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
        static TRANSFERS: std::sync::OnceLock<proteus_pty::budget::Budget> = std::sync::OnceLock::new();
        let budget = TRANSFERS.get_or_init(|| proteus_pty::budget::Budget::new(64 << 20, 4));
        let Some(permit) = budget.try_acquire(16 << 20) else {
            self.clipboard_received(target, String::new());
            return;
        };
        let tx = self.sender.clone();
        let _ = std::thread::Builder::new().name("paste".into()).spawn(move || {
            // Timeout/overflow discards the whole selection, never a partial command.
            let data = transfer::read(File::from(fd), 16 << 20, Duration::from_secs(3)).unwrap_or_default();
            let _ = tx.send(Msg::Clipboard(target, String::from_utf8_lossy(&data).into_owned(), permit));
        });
    }

    pub(super) fn clipboard_received(&mut self, target: ClipTarget, text: String) {
        let id = match target {
            ClipTarget::Paste(id) | ClipTarget::Osc52(id, ..) => id,
            ClipTarget::Plugin(n) => return self.plugin_clipboard(n, text),
        };
        self.activate_pane(id);
        match target {
            ClipTarget::Paste(id) => self.paste_text(id, text),
            ClipTarget::Plugin(_) => {}
            ClipTarget::Osc52(id, kind, terminator) => {
                let Some(p) = self.panes.get_mut(&id) else { return };
                p.term.respond_clipboard(kind, &text, terminator);
                let reply = p.term.take_pty_output();
                p.write(&reply);
            }
        }
    }

    /// Paste, asking first if the text could run commands unseen.
    pub(super) fn paste_text(&mut self, id: PaneId, text: String) {
        let Some(p) = self.panes.get(&id) else { return };
        if text.is_empty() {
            return;
        }
        if self.cfg.paste_protection && prompt::needs_paste_confirmation(&text, p.term.mode()) {
            if !self.w.prompts.iter().any(|p| matches!(p, Prompt::Paste(..))) {
                self.w.prompts.push_back(Prompt::Paste(id, text));
                self.w.dirty = true;
            }
            return;
        }
        self.paste_now(id, &text);
    }

    pub(super) fn paste_now(&mut self, id: PaneId, text: &str) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        let bytes = input::encode_paste(text, p.term.mode());
        p.term.scroll_to_bottom();
        if let Err(e) = p.try_write(&bytes) {
            self.toast(format!("Paste not queued: {e}"), true, None);
        }
    }

    pub(super) fn send_selection(text: &str, fd: OwnedFd) {
        let text = text.to_string();
        let _ = std::thread::Builder::new().name("copy".into()).spawn(move || {
            let _ = File::from(fd).write_all(text.as_bytes());
        });
    }
}
