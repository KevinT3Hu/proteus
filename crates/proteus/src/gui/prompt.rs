//! Questions and notices at the bottom of the window: asking, answering, drawing.

use super::*;

/// A question (or notice) shown at the bottom of the window. While one is
/// up, keys answer it instead of going to the application.
#[derive(Debug)]
pub(super) enum Prompt {
    ClipboardRead(PaneId, ClipboardKind, Terminator),
    Paste(PaneId, String),
    Notice(String),
    /// Close these panes (something is running in them).
    Close(Vec<PaneId>, String),
    /// Close the window (something is running).
    Quit(String),
    /// Restore the session saved before an unclean exit.
    Restore(LayoutSpec),
    /// A plugin's yes/no question (`Plugins::asks`).
    PluginConfirm(u64, String),
    /// A plugin requests permissions the config does not grant.
    PluginPermissions(Box<proteus_wasm::Resolved>),
}

impl App {
    pub(super) fn clipboard_load(&mut self, id: PaneId, kind: ClipboardKind, terminator: Terminator) {
        if self.cfg.osc52_read == ClipboardRead::Allow || self.osc52_always {
            self.read_clipboard(kind, ClipTarget::Osc52(id, kind, terminator));
        } else if !self.w.prompts.iter().any(|p| matches!(p, Prompt::ClipboardRead(..))) {
            // One question at a time; repeated queries while it is up are dropped.
            self.w.prompts.push_back(Prompt::ClipboardRead(id, kind, terminator));
            self.w.dirty = true;
        }
    }

    pub(super) fn answer_prompt(&mut self, key: input::Key) {
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
            Prompt::PluginConfirm(n, q) => match key {
                Key::Enter => self.plugin_answer(n, Some("yes".into())),
                _ if yes => self.plugin_answer(n, Some("yes".into())),
                _ if no => self.plugin_answer(n, None),
                _ => self.w.prompts.push_front(Prompt::PluginConfirm(n, q)),
            },
            Prompt::PluginPermissions(entry) => match key {
                _ if yes => self.plugin_permissions_answer(*entry, true),
                _ if no => self.plugin_permissions_answer(*entry, false),
                _ => self.w.prompts.push_front(Prompt::PluginPermissions(entry)),
            },
            Prompt::Notice(_) => {}
        }
        self.w.dirty = true;
    }
}

/// Whether pasting `text` should be confirmed: outside bracketed paste a
/// newline runs whatever precedes it, and control characters can drive the
/// application.
pub(super) fn needs_paste_confirmation(text: &str, mode: TermMode) -> bool {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        return false;
    }
    text.chars().any(|c| c.is_control() && c != '\t')
}

impl Prompt {
    pub(super) fn overlay(&self, catalog: &crate::i18n::Catalog) -> Overlay {
        match self {
            Prompt::ClipboardRead(_, kind, _) => {
                let which =
                    if matches!(kind, ClipboardKind::Primary) { "ui-primary-selection" } else { "ui-clipboard" };
                Overlay {
                    title: catalog.t("prompt-read-clipboard", &[("which", &catalog.t(which, &[]))]),
                    detail: None,
                    keys: catalog.t("prompt-clipboard-keys", &[]),
                }
            }
            Prompt::Paste(_, text) => {
                let lines = text.trim_end_matches('\n').split('\n').count();
                let control = text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'));
                let title = match (lines, control) {
                    (1, false) => catalog.t("prompt-paste-run", &[]),
                    (1, true) => catalog.t("prompt-paste-control", &[]),
                    (n, false) => catalog.t("prompt-paste-lines", &[("count", &n.to_string())]),
                    (n, true) => catalog.t("prompt-paste-control-lines", &[("count", &n.to_string())]),
                };
                let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                let preview: String =
                    first.chars().map(|c| if c.is_control() { '\u{fffd}' } else { c }).take(200).collect();
                Overlay { title, detail: Some(preview), keys: catalog.t("prompt-paste-keys", &[]) }
            }
            Prompt::Restore(spec) => Overlay {
                title: catalog.t(
                    "prompt-restore-session",
                    &[
                        ("windows", &spec.windows.len().to_string()),
                        ("tabs", &spec.tabs().count().to_string()),
                        ("panes", &spec.pane_count().to_string()),
                    ],
                ),
                detail: None,
                keys: catalog.t("prompt-restore-keys", &[]),
            },
            Prompt::Close(_, msg) | Prompt::Quit(msg) => {
                Overlay { title: msg.clone(), detail: None, keys: catalog.t("prompt-close-keys", &[]) }
            }
            Prompt::Notice(msg) => {
                Overlay { title: msg.clone(), detail: None, keys: catalog.t("prompt-dismiss-keys", &[]) }
            }
            Prompt::PluginConfirm(_, q) => {
                Overlay { title: q.clone(), detail: None, keys: catalog.t("prompt-confirm-keys", &[]) }
            }
            Prompt::PluginPermissions(e) => {
                let name: String = e.name.chars().filter(|c| !c.is_control()).take(80).collect();
                let source: String = e.source.source.chars().filter(|c| !c.is_control()).take(200).collect();
                Overlay {
                    title: catalog.t("prompt-plugin-permissions", &[("plugin", &name), ("source", &source)]),
                    detail: Some(approvals::describe(&e.granted).join(", ")),
                    keys: catalog.t("prompt-plugin-permissions-keys", &[]),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translated_prompts_keep_user_content_and_shortcuts() {
        let mut catalog = crate::i18n::defaults();
        catalog.set_locale("zh").unwrap();
        let overlay = Prompt::Paste(1, "echo one\necho two\n".into()).overlay(&catalog);
        assert!(overlay.title.contains("2 行"));
        assert_eq!(overlay.detail.as_deref(), Some("echo one"));
        assert!(overlay.keys.contains("[Enter]"));
        let overlay = Prompt::PluginConfirm(1, "plugin's own text".into()).overlay(&catalog);
        assert_eq!(overlay.title, "plugin's own text");
        assert_eq!(overlay.keys, "[y] 是   [n] 否");
    }

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
        let o = Prompt::Paste(1, "echo 1\necho 2\n".into()).overlay(&crate::i18n::Catalog::new("en").unwrap());
        assert!(o.title.contains("2 lines"), "{o:?}");
        assert_eq!(o.detail.as_deref(), Some("echo 1"));
        let o = Prompt::Paste(1, "a\x1bb".into()).overlay(&crate::i18n::Catalog::new("en").unwrap());
        assert!(o.title.contains("control"), "{o:?}");
        assert!(o.detail.unwrap().contains('\u{fffd}'));
    }
}
