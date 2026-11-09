//! Questions and notices at the bottom of the window: asking, answering, drawing.

use super::*;

/// A question (or notice) shown at the bottom of the window. While one is
/// up, keys answer it instead of going to the application.
#[derive(Debug)]
pub(super) enum Prompt {
    ClipboardRead(PaneId, ClipboardKind, Terminator, Instant, String),
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

/// How long after the clipboard question appears keys are ignored.
const PROMPT_GRACE: Duration = Duration::from_millis(500);

impl App {
    pub(super) fn clipboard_load(&mut self, id: PaneId, kind: ClipboardKind, terminator: Terminator) {
        if self.cfg.osc52_read == ClipboardRead::Allow || self.osc52_always {
            self.read_clipboard(kind, ClipTarget::Osc52(id, kind, terminator));
        } else if !self.w.prompts.iter().any(|p| matches!(p, Prompt::ClipboardRead(..))) {
            // One question at a time; repeated queries while it is up are dropped.
            let title: String = self
                .panes
                .get(&id)
                .map(|p| p.title().chars().filter(|c| !c.is_control()).take(60).collect())
                .unwrap_or_default();
            self.w.prompts.push_back(Prompt::ClipboardRead(id, kind, terminator, Instant::now(), title));
            self.w.dirty = true;
        }
    }

    /// Start the grace period of the front prompt once it is on screen.
    pub(super) fn prompt_drawn(&mut self) {
        let front = self.w.prompts.front().map(std::mem::discriminant);
        self.w.prompt_shown = match (front, self.w.prompt_shown) {
            (None, _) => None,
            (Some(d), Some((s, t))) if s == d => Some((s, t)),
            (Some(d), _) => Some((d, Instant::now())),
        };
    }

    pub(super) fn answer_prompt(&mut self, key: input::Key) {
        use input::Key;
        let Some(front) = self.w.prompts.front() else { return };
        if graced(front, self.w.prompt_shown, Instant::now()) {
            return;
        }
        let Some(p) = self.w.prompts.pop_front() else { return };
        let yes = matches!(key, Key::Char('y' | 'Y'));
        let no = matches!(key, Key::Char('n' | 'N') | Key::Escape);
        let enter = matches!(key, Key::Enter);
        let keep = match p {
            Prompt::ClipboardRead(id, kind, terminator, shown, title) => match key {
                _ if shown.elapsed() < PROMPT_GRACE => Some(Prompt::ClipboardRead(id, kind, terminator, shown, title)),
                // Shift+A: lowercase `a` is too easily typed by accident.
                Key::Char('A') => {
                    self.osc52_always = true;
                    self.read_clipboard(kind, ClipTarget::Osc52(id, kind, terminator));
                    None
                }
                _ if yes => {
                    self.read_clipboard(kind, ClipTarget::Osc52(id, kind, terminator));
                    None
                }
                Key::Enter => None,
                _ if no => None,
                _ => Some(Prompt::ClipboardRead(id, kind, terminator, shown, title)),
            },
            Prompt::Paste(id, text) => match key {
                _ if yes || enter => {
                    self.paste_now(id, &text);
                    None
                }
                _ if no => None,
                _ => Some(Prompt::Paste(id, text)),
            },
            Prompt::Close(ids, msg) => match key {
                _ if yes || enter => {
                    ids.into_iter().for_each(|id| self.close_pane(id));
                    None
                }
                _ if no => None,
                _ => Some(Prompt::Close(ids, msg)),
            },
            Prompt::Quit(msg) => match key {
                _ if yes || enter => {
                    self.quit_requested = true;
                    self.exit = true;
                    None
                }
                _ if no => None,
                _ => Some(Prompt::Quit(msg)),
            },
            Prompt::Restore(spec) => match key {
                _ if yes || enter => {
                    self.restore(&spec);
                    None
                }
                _ if no => None,
                _ => Some(Prompt::Restore(spec)),
            },
            Prompt::PluginConfirm(n, q) => match key {
                _ if yes => {
                    self.plugin_answer(n, Some("yes".into()));
                    None
                }
                _ if no => {
                    self.plugin_answer(n, None);
                    None
                }
                _ => Some(Prompt::PluginConfirm(n, q)),
            },
            Prompt::PluginPermissions(entry) => match key {
                _ if yes => {
                    self.plugin_permissions_answer(*entry, true);
                    None
                }
                _ if no => {
                    self.plugin_permissions_answer(*entry, false);
                    None
                }
                _ => Some(Prompt::PluginPermissions(entry)),
            },
            Prompt::Notice(_) => None,
        };
        match keep {
            Some(p) => self.w.prompts.push_front(p),
            None => self.w.prompt_shown = None,
        }
        self.w.dirty = true;
    }
}

/// Whether keys are ignored by prompt `p` (drawn as `shown`): prompts not
/// raised by something the user just did take no answers until they have
/// been on screen for [`PROMPT_GRACE`].
fn graced(p: &Prompt, shown: Option<(std::mem::Discriminant<Prompt>, Instant)>, now: Instant) -> bool {
    matches!(p, Prompt::Restore(_) | Prompt::PluginConfirm(..) | Prompt::PluginPermissions(_))
        && !shown.is_some_and(|(d, t)| d == std::mem::discriminant(p) && now.duration_since(t) >= PROMPT_GRACE)
}

/// Whether pasting `text` should be confirmed. Bracketed paste is exempt
/// only because [`proteus_core::input::encode_paste`] strips controls there.
pub(super) fn needs_paste_confirmation(text: &str, mode: TermMode) -> bool {
    proteus_core::input::paste_needs_confirmation(text, mode)
}

impl Prompt {
    pub(super) fn overlay(&self, catalog: &crate::i18n::Catalog) -> Overlay {
        match self {
            Prompt::ClipboardRead(_, kind, _, _, pane) => {
                let which =
                    if matches!(kind, ClipboardKind::Primary) { "ui-primary-selection" } else { "ui-clipboard" };
                Overlay {
                    title: catalog.t("prompt-read-clipboard", &[("which", &catalog.t(which, &[])), ("pane", pane)]),
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
                detail: {
                    let commands = spec.commands();
                    (!commands.is_empty()).then(|| {
                        let list: String = commands
                            .join("; ")
                            .chars()
                            .map(|c| if c.is_control() { '\u{fffd}' } else { c })
                            .take(300)
                            .collect();
                        catalog.t("prompt-restore-commands", &[("commands", &list)])
                    })
                },
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
                let name: String = e.name.chars().filter(|&c| !proteus_wasm::is_deceptive(c)).take(80).collect();
                let mut source: String =
                    e.source.source.chars().filter(|&c| !proteus_wasm::is_deceptive(c)).take(200).collect();
                if let Some(rev) = &e.source.rev {
                    source
                        .extend(" @ ".chars().chain(rev.chars().filter(|&c| !proteus_wasm::is_deceptive(c)).take(64)));
                }
                source.push_str(&format!(" (sha256 {})", e.hash.get(..16).unwrap_or(&e.hash)));
                let mut detail = approvals::describe(&e.granted).join(", ");
                if !e.granted.run_command.is_empty() {
                    detail.push_str(". ");
                    detail.push_str(&catalog.t("prompt-plugin-run-command-warning", &[]));
                }
                if e.granted.open_pane {
                    detail.push_str(". ");
                    detail.push_str(&catalog.t("prompt-plugin-open-pane-warning", &[]));
                }
                if e.granted.writes_startup_files() {
                    detail.push_str(". ");
                    detail.push_str(&catalog.t("prompt-plugin-startup-files-warning", &[]));
                }
                Overlay {
                    title: catalog.t("prompt-plugin-permissions", &[("plugin", &name), ("source", &source)]),
                    detail: Some(detail),
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
    fn unrequested_prompts_ignore_keys_until_shown_for_a_while() {
        let now = Instant::now();
        let ask = Prompt::PluginConfirm(1, "q".into());
        let d = std::mem::discriminant(&ask);
        let early = now - PROMPT_GRACE / 2;
        let late = now - PROMPT_GRACE * 2;
        assert!(graced(&ask, None, now));
        assert!(graced(&ask, Some((d, early)), now));
        assert!(!graced(&ask, Some((d, late)), now));
        let notice = Prompt::Notice("n".into());
        assert!(graced(&ask, Some((std::mem::discriminant(&notice), late)), now));
        assert!(!graced(&notice, None, now));
        assert!(!graced(&Prompt::Paste(1, "x".into()), None, now));
        assert!(graced(&Prompt::Restore(LayoutSpec::default()), None, now));
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
