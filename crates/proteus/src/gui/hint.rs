//! Hint mode (doc 01): URLs, paths, hashes... on screen get short labels;
//! typing one opens, copies or inserts it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use proteus_core::event::ClipboardKind;
use proteus_core::hints::{self, Hint, HintKind, HintPattern};
use proteus_core::input::{Key, Mods};

use super::render::{Label, Mark, MarkKind};
use super::rows::Rows;
use super::{App, Theme};
use crate::config::HintPatternConfig;
use crate::layout::PaneId;
use crate::modes::{HintAction, Mode};

#[derive(Debug, Clone)]
pub struct HintState {
    /// What a label typed without modifiers does.
    pub action: HintAction,
    pub items: Vec<(PaneId, Hint)>,
    /// Label characters typed so far.
    pub typed: String,
}

impl HintState {
    fn matching(&self) -> impl Iterator<Item = &(PaneId, Hint)> {
        self.items.iter().filter(|(_, h)| h.label.starts_with(&self.typed))
    }
}

/// User patterns (first, so they win on overlap), then the built-in ones.
pub fn patterns(custom: &[HintPatternConfig]) -> Vec<HintPattern> {
    let mut v: Vec<HintPattern> =
        custom.iter().filter_map(|c| HintPattern::new(HintKind::Custom(c.name.clone()), &c.regex).ok()).collect();
    v.extend(hints::builtin_patterns());
    v
}

/// `text` quoted for a POSIX shell if it needs it.
pub fn shell_quote(text: &str) -> String {
    let safe = |c: char| c.is_alphanumeric() || "-_./~:@%+=,".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// Editor command line opening `path` at `line`/`col`, from `$VISUAL` or
/// `$EDITOR` (`vi` if neither is set).
pub fn editor_command(editor: &str, path: &Path, line: Option<u32>, col: Option<u32>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = editor.split_whitespace().map(OsString::from).collect();
    if argv.is_empty() {
        argv.push("vi".into());
    }
    let name = Path::new(&argv[0]).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let p = path.display().to_string();
    match (name.as_str(), line) {
        (_, None) => argv.push(p.into()),
        ("hx" | "helix", Some(l)) => {
            argv.push(format!("{p}:{l}{}", col.map(|c| format!(":{c}")).unwrap_or_default()).into())
        }
        ("code" | "codium" | "code-oss", Some(l)) => {
            argv.push("-g".into());
            argv.push(format!("{p}:{l}{}", col.map(|c| format!(":{c}")).unwrap_or_default()).into());
        }
        ("nano", Some(l)) => {
            argv.push(format!("+{l}{}", col.map(|c| format!(",{c}")).unwrap_or_default()).into());
            argv.push(p.into());
        }
        (_, Some(l)) => {
            argv.push(format!("+{l}").into());
            argv.push(p.into());
        }
    }
    argv
}

/// `~/x` → `$HOME/x`; relative paths are taken from `base`.
pub fn resolve(base: Option<&Path>, path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let p = PathBuf::from(path);
    match base {
        Some(b) if p.is_relative() => b.join(p),
        _ => p,
    }
}

impl App {
    pub(super) fn hints_start(&mut self, action: HintAction) {
        let view = self.view();
        let ids: Vec<PaneId> = view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)).collect();
        let mut items = Vec::new();
        for id in ids {
            let Some(p) = self.panes.get(&id) else {
                continue;
            };
            let rows = Rows::of(&p.term);
            let visible: Vec<_> = rows.visible_lines().collect();
            let (Some(&first), Some(&last)) = (visible.first(), visible.last()) else {
                continue;
            };
            let found = hints::find_hints(p.term.grid(), first..last + 1, &self.hint_patterns, &self.cfg.hint_alphabet);
            items.extend(found.into_iter().filter(|h| visible.binary_search(&h.start.line).is_ok()).map(|h| (id, h)));
        }
        if items.is_empty() {
            return self.notice("no hints on screen".into());
        }
        let labels = hints::labels(items.len(), &self.cfg.hint_alphabet);
        for ((_, h), label) in items.iter_mut().zip(labels) {
            h.label = label;
        }
        self.w.hints = Some(HintState { action, items, typed: String::new() });
        self.w.dirty = true;
    }

    /// A key while hints are shown. Shift on the last label key copies,
    /// Ctrl inserts.
    pub(super) fn hint_key(&mut self, key: Key, mods: Mods) {
        let Some(st) = self.w.hints.as_mut() else {
            return;
        };
        let ctrl = mods.contains(Mods::CTRL);
        match key {
            Key::Escape => self.w.hints = None,
            Key::Char('c' | 'g') if ctrl => self.w.hints = None,
            Key::Backspace => {
                st.typed.pop();
            }
            Key::Char(c) => {
                st.typed.push(c.to_ascii_lowercase());
                let hit = st.matching().find(|(_, h)| h.label == st.typed).cloned();
                if st.matching().next().is_none() {
                    st.typed.pop();
                } else if let Some((id, h)) = hit {
                    let action = if mods.contains(Mods::SHIFT) {
                        HintAction::Copy
                    } else if ctrl {
                        HintAction::Insert
                    } else {
                        st.action
                    };
                    self.w.hints = None;
                    self.w.dirty = true;
                    self.hint_pick(id, &h, action);
                }
            }
            _ => {}
        }
        self.w.dirty = true;
    }

    fn hint_pick(&mut self, id: PaneId, h: &Hint, action: HintAction) {
        match action {
            HintAction::Copy => {
                self.set_selection(ClipboardKind::Clipboard, h.text.clone());
                self.set_selection(ClipboardKind::Primary, h.text.clone());
            }
            HintAction::Insert => {
                let text = match h.kind {
                    HintKind::Path | HintKind::Url | HintKind::Custom(_) => shell_quote(&h.text),
                    _ => h.text.clone(),
                };
                if self.w.mode == Mode::Scroll {
                    self.set_mode(Mode::Normal);
                }
                let fid = self.focused_id();
                self.paste_now(fid, &text);
            }
            HintAction::Open => self.hint_open(id, h),
        }
    }

    /// Directory a relative path in pane `id` at `line` is relative to: the
    /// block's cwd (OSC 7 when it ran), else the pane's.
    fn hint_base(&self, id: PaneId, line: proteus_core::Line) -> Option<PathBuf> {
        let p = self.panes.get(&id)?;
        let from_block = super::blocks::block_at(&p.term, line)
            .and_then(|b| b.cwd.clone())
            .map(PathBuf::from)
            .filter(|d| d.is_dir());
        from_block.or_else(|| p.cwd(&self.hostname))
    }

    fn hint_open(&mut self, id: PaneId, h: &Hint) {
        let base = self.hint_base(id, h.start.line);
        match &h.kind {
            HintKind::Url => self.xdg_open(&hints::url_with_scheme(&h.text), base.as_deref()),
            HintKind::Custom(name) => {
                match self.cfg.hint_patterns.iter().find(|p| &p.name == name).and_then(|p| p.url.clone()) {
                    Some(url) => self.xdg_open(&url.replace("{}", &h.text), base.as_deref()),
                    None => self.hint_pick(id, h, HintAction::Copy),
                }
            }
            HintKind::Path => {
                let loc = hints::parse_location(&h.text);
                // `a/b.rs:12` may also be a file literally named so.
                let (path, line, col) = {
                    let whole = resolve(base.as_deref(), &h.text);
                    if whole.exists() {
                        (whole, None, None)
                    } else {
                        (resolve(base.as_deref(), &loc.path), loc.line, loc.col)
                    }
                };
                if !path.exists() {
                    return self.notice(format!("no such file: {}", path.display()));
                }
                if line.is_some() && path.is_file() {
                    let editor =
                        std::env::var("VISUAL").ok().filter(|s| !s.is_empty()).or_else(|| std::env::var("EDITOR").ok());
                    let argv = editor_command(editor.as_deref().unwrap_or("vi"), &path, line, col);
                    let dir = path.parent().map(Path::to_path_buf);
                    self.split_with(None, dir, Some(argv));
                } else {
                    self.xdg_open(&path.display().to_string(), base.as_deref());
                }
            }
            _ => self.hint_pick(id, h, HintAction::Copy),
        }
    }

    /// Hand `target` to the desktop's default application.
    fn xdg_open(&mut self, target: &str, dir: Option<&Path>) {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(d) = dir.filter(|d| d.is_dir()) {
            cmd.current_dir(d);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let _ = std::thread::Builder::new().name("xdg-open".into()).spawn(move || child.wait());
            }
            Err(e) => self.notice(format!("cannot run xdg-open: {e}")),
        }
    }

    /// Hint text of pane `id`, highlighted.
    pub(super) fn hint_marks(&self, id: PaneId) -> Vec<Mark> {
        let Some(st) = &self.w.hints else {
            return Vec::new();
        };
        st.matching()
            .filter(|(p, _)| *p == id)
            .map(|(_, h)| Mark { start: h.start, end: h.end, kind: MarkKind::Hint })
            .collect()
    }

    /// The labels, over the start of each hint (typed characters dropped).
    pub(super) fn hint_labels(&self, theme: &Theme) -> Vec<Label> {
        let Some(st) = &self.w.hints else {
            return Vec::new();
        };
        let (cw, ch) = self.cell();
        let view = self.view();
        let mut out = Vec::new();
        for (id, h) in st.matching() {
            let (Some(r), Some(p)) = (view.rect(*id), self.panes.get(id)) else {
                continue;
            };
            let skip = self.row_skip(*id);
            let Some(row) = Rows::of(&p.term).row_of(h.start.line).filter(|&y| y >= skip) else {
                continue;
            };
            let text = h.label[st.typed.len()..].to_string();
            out.push(Label {
                x: (r.x + h.start.col as i32 * cw as i32) as f32,
                y: (r.y + (row - skip) as i32 * ch as i32) as f32,
                cols: text.len(),
                text,
                fg: theme.bg,
                bg: Some(p.term.palette().indexed(3)),
                bold: true,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("src/a.rs:12"), "src/a.rs:12");
        assert_eq!(shell_quote("my file"), "'my file'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("https://x/?a=1&b=2"), "'https://x/?a=1&b=2'");
    }

    #[test]
    fn editors() {
        let p = Path::new("/tmp/a.rs");
        let s = |v: Vec<OsString>| v.into_iter().map(|a| a.into_string().unwrap()).collect::<Vec<_>>().join(" ");
        assert_eq!(s(editor_command("nvim", p, Some(3), Some(4))), "nvim +3 /tmp/a.rs");
        assert_eq!(s(editor_command("/usr/bin/hx", p, Some(3), Some(4))), "/usr/bin/hx /tmp/a.rs:3:4");
        assert_eq!(s(editor_command("code --wait", p, Some(3), None)), "code --wait -g /tmp/a.rs:3");
        assert_eq!(s(editor_command("nano", p, Some(3), Some(2))), "nano +3,2 /tmp/a.rs");
        assert_eq!(s(editor_command("", p, None, None)), "vi /tmp/a.rs");
    }

    #[test]
    fn paths() {
        assert_eq!(resolve(Some(Path::new("/srv")), "a/b"), PathBuf::from("/srv/a/b"));
        assert_eq!(resolve(Some(Path::new("/srv")), "/etc/x"), PathBuf::from("/etc/x"));
        let custom = vec![HintPatternConfig { name: "jira".into(), regex: r"\b[A-Z]+-\d+\b".into(), url: None }];
        let pats = patterns(&custom);
        assert_eq!(pats[0].kind, HintKind::Custom("jira".into()));
        assert!(pats.len() > 1);
    }
}
