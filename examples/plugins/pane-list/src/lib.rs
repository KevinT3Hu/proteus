//! Example plugin: a sidebar listing every pane, with a status segment.
//!
//! Palette: "Pane list: toggle" opens or closes the sidebar. In the
//! sidebar `j`/`k` (or arrows) choose a pane, Enter (or a click) shows it,
//! Esc goes back to the terminal, `q` closes the sidebar.

use std::cell::RefCell;

use proteus_plugin::types::{
    Color, Event, Frame, Line, PaneInfo, Style, SurfaceId, SurfaceKind, SurfaceOptions, Theme, UiEvent,
};
use proteus_plugin::{Guest, List, fit, host, pad, text, types};

const TOGGLE: &str = "sidebar.toggle";
const WIDTH: u32 = 28;

#[derive(Default)]
struct State {
    surface: Option<SurfaceId>,
    size: (u32, u32),
    panes: Vec<PaneInfo>,
    list: List,
    theme: Option<Theme>,
    i18n: proteus_plugin::i18n::Catalog,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

struct PaneList;

fn refresh(s: &mut State) {
    s.panes = host::list_panes().unwrap_or_default();
    s.list.len = s.panes.len();
    let n = s.panes.len();
    let mut args = proteus_plugin::i18n::FluentArgs::new();
    args.set("count", n);
    let _ = host::set_status(types::StatusSide::Right, &s.i18n.format("panes-count", Some(&args)).unwrap_or_default());
}

fn draw(s: &mut State) {
    let Some(id) = s.surface else { return };
    let (cols, rows) = (s.size.0 as usize, s.size.1 as usize);
    let accent = s.theme.map_or(Color::Indexed(4), |t| Color::Rgb(t.accent));
    let dim = s.theme.map_or(Color::Indexed(8), |t| Color::Rgb(t.dim));
    // Two rows per pane: title, then directory.
    let shown = rows / 2;
    let mut lines: Vec<Line> = Vec::new();
    let visible: Vec<(usize, bool)> = s.list.visible(shown).collect();
    for (i, selected) in visible {
        let p = &s.panes[i];
        let mark = if p.focused { "● " } else { "  " };
        let title = fit(&format!("{mark}{} {}", p.id, p.title), cols);
        let dir = p.cwd.as_deref().map(home).unwrap_or_default();
        let dir = fit(&format!("    {dir}"), cols);
        let (style, sub) = if selected {
            let st = Style { fg: s.theme.map(|t| Color::Rgb(t.bg)), bg: Some(accent), attrs: types::Attrs::BOLD };
            (st, Style { attrs: types::Attrs::empty(), ..st })
        } else {
            (Style::PLAIN, Style { fg: Some(dim), ..Style::PLAIN })
        };
        lines.push(pad(vec![types::Span { text: title, style, link: None }], cols, style));
        lines.push(pad(vec![types::Span { text: dir, style: sub, link: None }], cols, sub));
    }
    if s.panes.is_empty() {
        lines.push(vec![text(s.i18n.format("panes-empty", None).unwrap_or_default()).fg(dim)]);
    }
    let _ = host::draw(id, &Frame { lines, cursor: None, background: None, images: vec![], accessibility: vec![] });
}

fn home(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

fn toggle(s: &mut State) {
    if let Some(id) = s.surface.take() {
        host::close_surface(id);
        return;
    }
    let opts = SurfaceOptions {
        kind: SurfaceKind::SidebarLeft,
        title: s.i18n.format("panes-title", None).unwrap_or_default(),
        cols: WIDTH,
        rows: 0,
        focus: true,
        pane: None,
    };
    match host::open_surface(&opts) {
        Ok(id) => {
            s.surface = Some(id);
            s.size = host::surface_size(id).unwrap_or((WIDTH, 10));
            refresh(s);
            if let Some(i) = s.panes.iter().position(|p| p.focused) {
                s.list.selected = i;
            }
            draw(s);
        }
        Err(e) => host::notify(&format!("pane list: {e}"), true),
    }
}

impl Guest for PaneList {
    fn init() -> Result<(), String> {
        STATE.with_borrow_mut(|s| -> Result<(), String> {
            s.i18n = proteus_plugin::i18n::catalog()?;
            s.i18n.add_resource("en", include_str!("../locales/en.ftl"))?;
            s.i18n.add_resource("zh", include_str!("../locales/zh.ftl"))?;
            host::register_action(TOGGLE, &s.i18n.format("panes-toggle", None)?, Some("alt+p"))
        })?;
        STATE.with_borrow_mut(refresh);
        host::log("pane list ready");
        Ok(())
    }

    fn on_event(event: Event) {
        STATE.with_borrow_mut(|s| {
            match event {
                Event::Action(a) if a == TOGGLE => return toggle(s),
                Event::ThemeChanged(t) => s.theme = Some(t),
                Event::ConfigChanged => {
                    let _ = proteus_plugin::i18n::sync(&mut s.i18n);
                    let _ = host::register_action(
                        TOGGLE,
                        &s.i18n.format("panes-toggle", None).unwrap_or_default(),
                        Some("alt+p"),
                    );
                    refresh(s);
                }
                Event::PaneOpened(_)
                | Event::PaneClosed(_)
                | Event::FocusChanged(_)
                | Event::CwdChanged(_)
                | Event::TitleChanged(_) => refresh(s),
                _ => return,
            }
            draw(s);
        })
    }

    fn on_ui(surface: SurfaceId, event: UiEvent) {
        STATE.with_borrow_mut(|s| {
            if s.surface != Some(surface) {
                return;
            }
            let rows = (s.size.1 / 2) as usize;
            match event {
                UiEvent::Key(k) => match (k.key.as_str(), k.text.as_str()) {
                    ("enter", _) => {
                        if let Some(p) = s.panes.get(s.list.selected) {
                            let _ = host::focus_pane(p.id);
                        }
                    }
                    ("escape", _) => {
                        if let Ok(Some(p)) = host::focused_pane() {
                            let _ = host::focus_pane(p);
                        }
                    }
                    (_, "q") => return toggle(s),
                    (key, t) => {
                        if !s.list.key(key, t, rows) {
                            return;
                        }
                    }
                },
                UiEvent::Mouse(m) if m.action != types::MouseAction::Press => return,
                UiEvent::Mouse(m) => match m.button {
                    types::MouseButton::Left => {
                        if let Some(i) = s.list.at_row(m.row as usize / 2) {
                            s.list.selected = i;
                            let _ = host::focus_pane(s.panes[i].id);
                        }
                    }
                    types::MouseButton::WheelUp => s.list.offset = s.list.offset.saturating_sub(1),
                    types::MouseButton::WheelDown => s.list.offset += 1,
                    _ => return,
                },
                UiEvent::Resize(size) => s.size = size,
                UiEvent::Closed => {
                    s.surface = None;
                    return;
                }
                UiEvent::Accessibility(_)
                | UiEvent::Text(_)
                | UiEvent::Preedit(_)
                | UiEvent::Focus(_)
                | UiEvent::Drop(_) => return,
            }
            draw(s);
        })
    }
}

proteus_plugin::export!(PaneList);
