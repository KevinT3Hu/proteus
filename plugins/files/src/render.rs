//! File-browser components composed with the SDK layout and frame canvas.
use super::*;
use proteus_plugin::ui::{
    self, AccessibilityAction as A11yAction, AccessibilityRole as Role, Component, Element, ImageView, Length,
    Semantics, Size,
};
use proteus_plugin::width;

pub(super) fn file_node_id(path: &Path) -> String {
    use std::fmt::Write;
    let mut id = String::from("file:");
    for byte in path.as_os_str().as_encoded_bytes() {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// How a row is drawn.
#[derive(Clone, Copy)]
struct RowLook {
    cursor: bool,
    marked: bool,
    /// The miller parent column's current directory.
    here: bool,
    git: Option<git::Mark>,
    tree: bool,
    size: bool,
}

fn row_line(r: &view::Row, look: RowLook, cols: usize, c: &Colors) -> Line {
    let e = &r.entry;
    let mut st = if e.dir {
        style(Some(c.accent), None, Attrs::BOLD)
    } else if e.link {
        style(Some(Color::Indexed(6)), None, Attrs::empty())
    } else {
        Style::PLAIN
    };
    if e.hidden() {
        st.attrs |= Attrs::DIM;
    }
    let fill = if look.cursor {
        st = style(Some(c.bg), Some(c.accent), st.attrs & !Attrs::DIM);
        st
    } else if look.here {
        st.bg = Some(c.bar);
        st
    } else {
        Style::PLAIN
    };
    let mut line = Vec::new();
    let mark = if look.marked { "●" } else { " " };
    line.push(span(mark, style(Some(c.accent), fill.bg, Attrs::BOLD)));
    let (gc, gcol) = match look.git {
        Some(git::Mark::Modified) => ('M', Color::Indexed(3)),
        Some(git::Mark::Staged) => ('+', Color::Indexed(2)),
        Some(git::Mark::Untracked) => ('?', Color::Indexed(1)),
        Some(git::Mark::Ignored) => ('!', c.dim),
        None => (' ', c.dim),
    };
    let gst = if look.cursor { fill } else { style(Some(gcol), fill.bg, Attrs::empty()) };
    line.push(span(gc.to_string(), gst));
    let mut name = String::from(" ");
    if look.tree {
        name.push_str(&"  ".repeat(r.depth));
        name.push_str(if !e.dir {
            "  "
        } else if r.open {
            "▾ "
        } else {
            "▸ "
        });
    }
    name.push_str(&e.name);
    if e.dir {
        name.push('/');
    }
    if e.link {
        name.push('@');
    }
    let size = if look.size && !e.dir { format!(" {} ", fsys::human(e.size)) } else { " ".into() };
    let show_size = cols > 2 + width(&size);
    let room = cols.saturating_sub(2 + if show_size { width(&size) } else { 0 });
    line.push(span(fit(&name, room), st));
    let mut sz = fill;
    if !look.cursor {
        sz.fg = Some(c.dim);
    }
    ui::bar(line, if show_size { vec![span(size, sz)] } else { Vec::new() }, cols, fill)
}

/// A file browser is a component: it declares a page and responsive body.
struct Browser<'a> {
    ui: &'a mut Ui,
    shown: Option<&'a Shown>,
    status: Option<git::Status>,
    clipboard: Option<(OpKind, usize)>,
    conflict: Option<PathBuf>,
    colors: Colors,
}
impl Component for Browser<'_> {
    fn view(&mut self) -> Element<'_> {
        let c = self.colors;
        let ui = &mut *self.ui;
        let flag = match (ui.view.kind, ui.follow) {
            _ if ui.remote.is_some() => tr!("flag-remote"),
            (Kind::Sidebar, false) => tr!("flag-pinned"),
            _ => String::new(),
        };
        let header = Element::bar(
            vec![span(fsys::tilde(&ui.view.dir, fsys::home().as_deref()), style(Some(c.accent), None, Attrs::BOLD))],
            vec![span(
                if flag.is_empty() { flag } else { format!(" {flag}") },
                style(Some(c.dim), None, Attrs::empty()),
            )],
        );
        let footer = footer(ui, self.clipboard, self.conflict.as_deref(), &c);
        let shown = self.shown;
        let status = self.status.as_ref();
        let body = if let Some(host) = &ui.remote {
            Element::text(tr!("remote-unavailable", host = host)).style(style(Some(c.dim), None, Attrs::ITALIC))
        } else if let Some(error) = ui.view.error() {
            Element::text(error).style(style(Some(Color::Indexed(1)), None, Attrs::empty()))
        } else {
            Element::responsive(move |size| browser_body(&mut ui.view, shown, status, c, size, ui.input.is_none()))
        };
        Element::page(header.id("header"), body, footer.id("footer"))
    }
}

fn footer<'a>(ui: &Ui, clipboard: Option<(OpKind, usize)>, conflict: Option<&Path>, c: &Colors) -> Element<'a> {
    let dim = style(Some(c.dim), None, Attrs::empty());

    if let Some(p) = conflict {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let q = tr!("conflict", name = name);
        Element::line(vec![span(q.clone(), style(Some(Color::Indexed(3)), None, Attrs::BOLD))])
            .semantics(Semantics::new(Role::Dialog).id("conflict").label(q).focused(true))
    } else if let Some(input) = &ui.input {
        Element::input(vec![span("/", style(Some(c.accent), None, Attrs::BOLD))], input).semantics(
            Semantics::new(Role::TextInput)
                .id("filter")
                .label(tr!("a11y-filter"))
                .focused(true)
                .actions([A11yAction::Focus, A11yAction::SetValue]),
        )
    } else if let Some((m, err)) = &ui.message {
        let st = if *err {
            style(Some(Color::Indexed(1)), None, Attrs::empty())
        } else {
            style(Some(c.fg), None, Attrs::empty())
        };
        Element::line(vec![span(m.clone(), st)]).semantics(Semantics::new(Role::Status).label(m))
    } else {
        let v = &ui.view;
        let pos = if v.rows.is_empty() { "0/0".into() } else { format!("{}/{}", v.list.selected + 1, v.rows.len()) };
        let mut s = format!("{pos}  {}{}", tr!(v.sort.key.label()), if v.sort.reverse { "↓" } else { "" });
        if v.hidden {
            s.push_str("  .*");
        }
        if !v.filter.is_empty() {
            s.push_str(&format!("  /{}", v.filter));
        }
        if !v.selected.is_empty() {
            s.push_str(&format!("  {}", tr!("marked", count = v.selected.len())));
        }
        if ui.view.visual.is_some() {
            s.push_str(&format!("  {}", tr!("visual")));
        }
        if let Some((k, n)) = clipboard {
            s.push_str(&format!(
                "  {}",
                tr!(if k == OpKind::Copy { "clipboard-copy" } else { "clipboard-move" }, count = n)
            ));
        }
        if let Some(p) = ui.prefix {
            let prefix = match p {
                Prefix::G => "g-".to_owned(),
                Prefix::Mark => tr!("prefix-mark"),
                Prefix::Jump => tr!("prefix-jump"),
                Prefix::Sort => tr!("prefix-sort"),
            };
            s.push_str(&format!("  {prefix}"));
        }
        if ui.view.kind == Kind::Picker {
            s.push_str(&format!("  {}", tr!("picker-insert")));
        }
        Element::line(vec![span(s, dim)])
    }
}

impl State {
    pub(super) fn draw(&mut self, id: SurfaceId) {
        let Some(ui) = self.uis.get_mut(&id) else { return };
        let status = if ui.remote.is_none() && ui.view.error().is_none() {
            self.git.status(&ui.view.dir).cloned()
        } else {
            None
        };
        let (cols, rows) = (ui.size.0 as usize, ui.size.1 as usize);
        let rendered = Browser {
            ui,
            shown: self.preview.as_ref(),
            status,
            clipboard: self.clip.as_ref().map(|(kind, paths)| (*kind, paths.len())),
            conflict: self.op.as_ref().filter(|o| o.surface == id).and_then(|o| o.conflict.clone()),
            colors: colors(self.theme),
        }
        .render(cols, rows);
        ui.list_area = rendered.area("files").unwrap_or_default();
        let _ = host::draw(id, &rendered.frame);
        if self.sidebar == Some(id) {
            self.draw_float();
        }
    }
    pub(super) fn draw_all(&mut self) {
        for id in self.uis.keys().copied().collect::<Vec<_>>() {
            self.draw(id);
        }
    }
}

fn browser_body<'a>(
    view: &'a mut View,
    shown: Option<&'a Shown>,
    status: Option<&'a git::Status>,
    c: Colors,
    size: Size,
    focused: bool,
) -> Element<'a> {
    let miller = view.layout == Layout::Miller && size.width >= 30;
    let parent = if miller && size.width >= 60 { Some(parent_column(view, status, c)) } else { None };
    let preview = if miller { Some(preview_component(view, shown, status, c)) } else { None };
    let tree = view.layout == Layout::Tree;
    let kind = view.kind;
    let rows = &view.rows;
    let selected = &view.selected;
    let empty = tr!(if view.filter.is_empty() { "empty" } else { "no-match" });
    let current = Element::list(&mut view.list, move |i, on, cols| {
        let r = &rows[i];
        row_line(
            r,
            RowLook {
                cursor: on,
                marked: selected.contains(&r.path),
                here: false,
                git: status.and_then(|s| s.mark(&r.path)),
                tree,
                size: (!miller && kind != Kind::Sidebar) || cols >= 40,
            },
            cols,
            &c,
        )
    })
    .empty_message(vec![span(empty, style(Some(c.dim), None, Attrs::ITALIC))])
    .id("files")
    .label(tr!("a11y-files"))
    .focused(focused)
    .item_semantics(move |i, on| {
        let r = &rows[i];
        let mut sem = Semantics::new(Role::ListItem)
            .id(file_node_id(&r.path))
            .label(&r.entry.name)
            .description(tr!(if r.entry.dir { "a11y-directory" } else { "a11y-file" }))
            .selected(on || selected.contains(&r.path))
            .actions([A11yAction::Focus, A11yAction::Select, A11yAction::Activate]);
        if tree && r.entry.dir {
            sem = sem.expanded(r.open);
        }
        sem
    });
    if let Some(preview) = preview {
        let mut children = Vec::new();
        if let Some(parent) = parent {
            children.push(parent.width(Length::Percent(20)).id("parent"));
        }
        children.push(current.width(Length::Percent(if size.width >= 60 { 40 } else { 50 })));
        children.push(preview.id("preview").label(tr!("a11y-preview")));
        Element::row(children).separated(style(Some(c.dim), None, Attrs::empty()))
    } else {
        current
    }
}

/// The component formats only visible entries, after its size is known.
struct DirectoryColumn<'a> {
    dir: PathBuf,
    entries: Vec<fsys::Entry>,
    center: Option<usize>,
    status: Option<&'a git::Status>,
    colors: Colors,
}
impl Component for DirectoryColumn<'_> {
    fn view(&mut self) -> Element<'_> {
        Element::responsive(move |size| {
            let start = self
                .center
                .map_or(0, |at| at.saturating_sub(size.height / 2).min(self.entries.len().saturating_sub(size.height)));
            Element::lines(
                self.entries
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(size.height)
                    .map(|(i, entry)| {
                        let path = self.dir.join(&entry.name);
                        let look = RowLook {
                            cursor: false,
                            marked: false,
                            here: self.center == Some(i),
                            git: self.status.and_then(|s| s.mark(&path)),
                            tree: false,
                            size: false,
                        };
                        row_line(
                            &view::Row { path, depth: 0, entry: entry.clone(), open: false },
                            look,
                            size.width,
                            &self.colors,
                        )
                    })
                    .collect(),
            )
        })
    }
}
fn parent_column<'a>(view: &mut View, status: Option<&'a git::Status>, c: Colors) -> Element<'a> {
    let Some(parent) = view.dir.parent().map(Path::to_path_buf) else { return Element::empty() };
    let hidden = view.hidden;
    let entries = match view.listing(&parent) {
        Ok(entries) => entries.iter().filter(|e| hidden || !e.hidden()).cloned().collect::<Vec<_>>(),
        Err(_) => return Element::empty(),
    };
    let here = entries.iter().position(|e| parent.join(&e.name) == view.dir).unwrap_or(0);
    Element::component(DirectoryColumn { dir: parent, entries, center: Some(here), status, colors: c })
}
pub(super) fn preview_component<'a>(
    view: &mut View,
    shown: Option<&'a Shown>,
    status: Option<&'a git::Status>,
    c: Colors,
) -> Element<'a> {
    let Some(r) = view.current().cloned() else { return Element::empty() };
    if r.entry.dir {
        let hidden = view.hidden;
        let entries = match view.listing(&r.path) {
            Ok(entries) => entries.iter().filter(|e| hidden || !e.hidden()).cloned().collect::<Vec<_>>(),
            Err(e) => return Element::text(e.clone()).style(style(Some(Color::Indexed(1)), None, Attrs::empty())),
        };
        if entries.is_empty() {
            return Element::text(tr!("empty")).style(style(Some(c.dim), None, Attrs::ITALIC));
        }
        return Element::component(DirectoryColumn { dir: r.path, entries, center: None, status, colors: c });
    }
    let shown = shown.filter(|s| s.path == r.path && s.mtime == r.entry.mtime);
    Element::component(FilePreview { shown, name: r.entry.name, colors: c })
}
struct FilePreview<'a> {
    shown: Option<&'a Shown>,
    name: String,
    colors: Colors,
}
impl Component for FilePreview<'_> {
    fn view(&mut self) -> Element<'_> {
        let dim = style(Some(self.colors.dim), None, Attrs::empty());
        let Some(sh) = self.shown else {
            return Element::lines(vec![
                vec![span(self.name.clone(), style(None, None, Attrs::BOLD))],
                vec![span("…", dim)],
            ]);
        };
        match &sh.body {
            Body::Loading => {
                let mut lines = sh.info.clone();
                lines.push(vec![span("…", dim)]);
                Element::lines(lines)
            }
            Body::Lines(lines) => Element::borrowed_lines(if lines.is_empty() { &sh.info } else { lines }),
            Body::Image(id, pixels) => {
                let info = sh.info.as_slice();
                let image = ImageView::new(*id, *pixels);
                let name = self.name.clone();
                Element::responsive(move |size| {
                    let gap = usize::from(size.height > 2 && !info.is_empty());
                    let count = info.len().min(size.height.saturating_sub(1 + gap));
                    Element::column([
                        Element::image(image).label(name),
                        Element::borrowed_lines(info).height(Length::Fixed(count)),
                    ])
                    .gap(gap)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make_ui(tag: &str) -> (PathBuf, Ui) {
        let base = std::env::temp_dir().join(format!("proteus-files-components-{}-{tag}", std::process::id()));
        let dir = base.join("view");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..10 {
            std::fs::write(dir.join(format!("file{i}")), b"test").unwrap();
        }
        let ui = Ui {
            view: View::new(Kind::Pane, dir),
            size: (60, 8),
            list_area: Rect::default(),
            prefix: None,
            input: None,
            message: None,
            follow: false,
            remote: None,
            target: None,
            watch: None,
            dragging: false,
        };
        (base, ui)
    }
    fn browser(ui: &mut Ui) -> Browser<'_> {
        Browser { ui, shown: None, status: None, clipboard: None, conflict: None, colors: colors(None) }
    }
    #[test]
    fn file_list_semantics_keep_names_focus_and_filter_identity() {
        let (base, mut ui) = make_ui("accessibility");
        ui.view.list.selected = 3;
        let frame = browser(&mut ui).render(12, 6).frame;
        let list = frame.accessibility.iter().find(|n| n.id == "files").unwrap();
        assert_eq!(list.role, Role::List);
        assert_eq!(list.set_size, Some(10));
        let selected = frame.accessibility.iter().find(|n| n.focused).unwrap();
        assert_eq!(selected.id, file_node_id(&ui.view.rows[3].path));
        assert_eq!(selected.label, ui.view.rows[3].entry.name);
        assert_eq!(selected.selected, Some(true));
        assert_eq!(selected.position, Some(4));
        ui.input = Some(TextInput::new("file3"));
        let frame = browser(&mut ui).render(12, 6).frame;
        let filter = frame.accessibility.iter().find(|n| n.focused).unwrap();
        assert_eq!(filter.id, "filter");
        assert_eq!(filter.value.as_deref(), Some("file3"));
        assert!(filter.actions.contains(&A11yAction::SetValue));
        let mut state = State::default();
        state.uis.insert(7, ui);
        use proteus_plugin::types::AccessibilityEvent;
        let id = file_node_id(&state.uis[&7].view.rows[1].path);
        state.accessibility(7, AccessibilityEvent { node: id, action: A11yAction::Select, value: None });
        assert_eq!(state.uis[&7].view.list.selected, 1);
        state.accessibility(
            7,
            AccessibilityEvent { node: "filter".into(), action: A11yAction::SetValue, value: Some("file2".into()) },
        );
        assert_eq!(state.uis[&7].view.rows.len(), 1);
        assert_eq!(state.uis[&7].view.rows[0].entry.name, "file2");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn chinese_reload_preserves_browser_state_and_clips_translations() {
        i18n::with_locale("en", || {
            let (base, mut ui) = make_ui("locale");
            ui.view.list.selected = 3;
            ui.view.selected.insert(ui.view.rows[0].path.clone());
            ui.prefix = Some(Prefix::Sort);
            i18n::set_locale("zh-CN").unwrap();
            let rendered = browser(&mut ui).render(100, 8);
            let footer: String = rendered.frame.lines.last().unwrap().iter().map(|s| s.text.as_str()).collect();
            assert!(footer.contains("名称"), "{footer}");
            assert!(footer.contains("已标记 1 项"), "{footer}");
            assert!(footer.contains("n 名称"), "{footer}");
            assert_eq!(ui.view.list.selected, 3);
            for cols in [0, 1, 10, 30, 60, 100] {
                let mut component = browser(&mut ui);
                component.conflict = Some("中文 { $name }".into());
                let rendered = component.render(cols, 8);
                for line in rendered.frame.lines {
                    assert!(line.iter().map(|s| width(&s.text)).sum::<usize>() <= cols);
                }
            }
            std::fs::remove_dir_all(base).unwrap();
        });
    }

    #[test]
    fn responsive_components_supply_actual_regions_for_hit_testing() {
        let (base, mut ui) = make_ui("regions");
        let wide = browser(&mut ui).render(60, 8);
        assert_eq!(wide.area("parent"), Some(Rect::new(0, 1, 11, 6)));
        assert_eq!(wide.area("files"), Some(Rect::new(12, 1, 23, 6)));
        assert_eq!(wide.area("preview"), Some(Rect::new(36, 1, 24, 6)));
        let area = wide.area("files").unwrap();
        assert_eq!(ui.view.list.at_point(area, 12, 1), Some(0));
        assert_eq!(ui.view.list.at_point(area, 36, 1), None);
        let medium = browser(&mut ui).render(30, 8);
        assert!(medium.area("parent").is_none());
        assert_eq!(medium.area("files").unwrap().width, 14);
        for cols in [0, 1, 29, 30, 59, 60, 100] {
            for rows in 0..4 {
                let rendered = browser(&mut ui).render(cols, rows);
                if let Some(area) = rendered.area("files") {
                    assert!(area.right() <= cols && area.bottom() <= rows);
                }
                assert_eq!(rendered.frame.lines.len(), rows);
                for line in rendered.frame.lines {
                    assert!(line.iter().map(|s| width(&s.text)).sum::<usize>() <= cols);
                }
            }
        }
        ui.view.set_layout(Layout::Tree);
        assert!(browser(&mut ui).render(100, 8).area("preview").is_none());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn declarative_filter_and_conflicts_do_not_leave_stale_cursor() {
        let (base, mut ui) = make_ui("input");
        ui.input = Some(TextInput::new("abc"));
        assert_eq!(browser(&mut ui).render(60, 8).frame.cursor, Some((4, 7)));
        assert!(browser(&mut ui).render(1, 1).frame.cursor.is_none());
        let mut component = browser(&mut ui);
        component.conflict = Some("file".into());
        let conflict = component.render(60, 8).frame;
        assert!(conflict.cursor.is_none());
        assert_eq!(conflict.accessibility.iter().find(|n| n.focused).unwrap().id, "conflict");
        ui.remote = Some("remote".into());
        assert!(browser(&mut ui).render(60, 8).area("files").is_none());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn file_rows_and_preview_components_clip_small_regions() {
        let row = view::Row {
            path: "/中文👩‍💻".into(),
            depth: 2,
            entry: fsys::Entry { name: "中文👩‍💻".into(), dir: false, link: false, size: 1024, mtime: 0 },
            open: false,
        };
        let look =
            RowLook { cursor: true, marked: true, here: false, git: Some(git::Mark::Staged), tree: true, size: true };
        let c = colors(None);
        for cols in 0..20 {
            let row = row_line(&row, look, cols, &c);
            assert_eq!(row.iter().map(|s| width(&s.text)).sum::<usize>(), cols);
        }
        let shown = Shown {
            path: "/image.png".into(),
            mtime: 0,
            info: vec![vec![span("metadata", Style::PLAIN)]],
            body: Body::Image(1, (800, 400)),
        };
        let mut preview = FilePreview { shown: Some(&shown), name: "image".into(), colors: c };
        assert!(preview.render(0, 3).frame.images.is_empty());
        assert!(preview.render(3, 0).frame.images.is_empty());
        let rendered = preview.render(3, 1);
        let image = &rendered.frame.images[0];
        assert!(image.cols <= 3 && image.rows <= 1);
    }
}
