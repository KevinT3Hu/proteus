//! Native AT-SPI integration through AccessKit. Work is lazy until activated.
use super::{App, Msg, WinId};
use crate::script::Host;
use accesskit::{
    Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId, Rect, Role,
    TextPosition, TextSelection, TreeId, TreeInfo, TreeUpdate,
};
use unicode_segmentation::UnicodeSegmentation;

const ROOT: NodeId = NodeId(1);
const TABS: NodeId = NodeId(2);
const INPUT: NodeId = NodeId(3);
const INPUT_TEXT: NodeId = NodeId(4);
const TAB_BASE: u64 = 100;
const PROMPT: NodeId = NodeId(5);
const STATUS: NodeId = NodeId(6);
const SURFACE_BIT: u64 = 1 << 63;

#[derive(Clone)]
struct Handler {
    window: WinId,
    sender: super::Sender<Msg>,
}
impl ActivationHandler for Handler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        let _ = self.sender.send(Msg::Accessibility(self.window, None));
        None
    }
}
impl ActionHandler for Handler {
    fn do_action(&mut self, request: ActionRequest) {
        let _ = self.sender.send(Msg::Accessibility(self.window, Some(request)));
    }
}
impl DeactivationHandler for Handler {
    fn deactivate_accessibility(&mut self) {}
}

fn pane_node(id: u64) -> NodeId {
    NodeId(id << 32)
}
fn text_node(pane: u64, row: usize) -> NodeId {
    NodeId((pane << 32) | ((row as u64 + 1) << 16))
}

fn character_lengths(text: &str) -> Vec<u8> {
    text.graphemes(true)
        .flat_map(|g| {
            if let Ok(n) = u8::try_from(g.len()) { vec![n] } else { g.chars().map(|c| c.len_utf8() as u8).collect() }
        })
        .collect()
}

fn text_run(text: &str, bounds: Rect) -> Node {
    let mut node = Node::new(Role::TextRun);
    node.set_value(text);
    node.set_bounds(bounds);
    let lengths = character_lengths(text);
    node.set_character_lengths(lengths);
    node
}

/// Build logical-order text runs, each monotonic in visual space, for AT-SPI.
fn terminal_text(
    t: &proteus_core::Terminal,
    pane: u64,
    rect: crate::layout::Rect,
    cw: u32,
    ch: u32,
    nodes: &mut Vec<(NodeId, Node)>,
) -> (Vec<NodeId>, Option<TextPosition>) {
    let mut children = Vec::new();
    let mut caret = None;
    for y in 0..t.lines() {
        let row = t.grid().row(y);
        let layout = t.visual_row(t.grid().abs_line(y));
        let used = row.cells().iter().rposition(|c| c.ch() != ' ' || c.has_text()).map_or(0, |i| i + 1);
        let cursor_col =
            if t.cursor().x > 0 && row.cells()[t.cursor().x].is_spacer() { t.cursor().x - 1 } else { t.cursor().x };
        let end = if y == t.cursor().y { used.max(cursor_col + 1) } else { used };
        struct Run {
            text: String,
            lengths: Vec<u8>,
            positions: Vec<f32>,
            widths: Vec<f32>,
            rtl: bool,
            left: usize,
            right: usize,
            last: usize,
            caret: Option<usize>,
        }
        let mut runs: Vec<Run> = Vec::new();
        for (col, cell) in row.cells().iter().take(end).enumerate() {
            if cell.is_spacer() {
                continue;
            }
            let x = layout.visual(col);
            let w = cell.width().max(1);
            let rtl = layout.rtl(col);
            let joins = runs.last().is_some_and(|r| r.rtl == rtl && if rtl { x + w == r.last } else { r.right == x });
            if !joins {
                runs.push(Run {
                    text: String::new(),
                    lengths: vec![],
                    positions: vec![],
                    widths: vec![],
                    rtl,
                    left: x,
                    right: x + w,
                    last: x,
                    caret: None,
                });
            }
            let r = runs.last_mut().unwrap();
            if y == t.cursor().y && col == cursor_col {
                r.caret = Some(r.lengths.len());
            }
            r.left = r.left.min(x);
            r.right = r.right.max(x + w);
            r.last = x;
            let mut text = String::new();
            cell.push_text(&mut text);
            let lengths = character_lengths(&text);
            let mut byte = 0;
            let mut ink = false;
            for length in lengths {
                let value = &text[byte..byte + length as usize];
                byte += length as usize;
                let control = value.chars().all(proteus_core::bidi::is_control);
                r.lengths.push(length);
                r.positions.push(x as f32 * cw as f32);
                r.widths.push(if control || ink { 0.0 } else { w as f32 * cw as f32 });
                if !control {
                    ink = true;
                }
            }
            r.text.push_str(&text);
        }
        if runs.is_empty() {
            runs.push(Run {
                text: String::new(),
                lengths: vec![],
                positions: vec![],
                widths: vec![],
                rtl: false,
                left: 0,
                right: 0,
                last: 0,
                caret: None,
            });
        }
        if !row.wrapped && y + 1 < t.lines() {
            let r = runs.last_mut().unwrap();
            r.text.push('\n');
            r.lengths.push(1);
            r.positions.push(if r.rtl { r.left as f32 * cw as f32 } else { r.right as f32 * cw as f32 });
            r.widths.push(0.0);
        }
        for (i, mut r) in runs.into_iter().enumerate() {
            let id = NodeId(text_node(pane, y).0 + i as u64);
            let mut node = Node::new(Role::TextRun);
            node.set_value(r.text);
            node.set_character_lengths(r.lengths);
            let left = rect.x as f64 + r.left as f64 * cw as f64;
            let right = rect.x as f64 + r.right as f64 * cw as f64;
            node.set_bounds(Rect::new(
                left,
                rect.y as f64 + y as f64 * ch as f64,
                right,
                rect.y as f64 + (y + 1) as f64 * ch as f64,
            ));
            if r.rtl {
                node.set_text_direction(accesskit::TextDirection::RightToLeft);
            } else {
                node.set_text_direction(accesskit::TextDirection::LeftToRight);
            }
            for (x, w) in r.positions.iter_mut().zip(&r.widths) {
                *x = if r.rtl { r.right as f32 * cw as f32 - *x - *w } else { *x - r.left as f32 * cw as f32 };
            }
            node.set_character_positions(r.positions);
            node.set_character_widths(r.widths);
            if let Some(character_index) = r.caret {
                caret = Some(TextPosition { node: id, character_index });
            }
            children.push(id);
            nodes.push((id, node));
        }
    }
    (children, caret)
}

impl App {
    pub(super) fn setup_accessibility(&mut self) {
        let handler = Handler { window: self.w.id, sender: self.sender.clone() };
        self.w.accessibility = Some(accesskit_unix::Adapter::new(handler.clone(), handler.clone(), handler));
    }

    pub(super) fn accessibility_event(&mut self, window: WinId, action: Option<ActionRequest>) {
        if !self.activate(window) {
            return;
        }
        if let Some(request) = action {
            if request.target_tree != TreeId::ROOT {
                return;
            }
            if request.target_node.0 & SURFACE_BIT != 0 {
                self.semantic_action(&request);
                self.update_accessibility();
                return;
            }
            match request.action {
                Action::Focus | Action::Click => {
                    let pane = request.target_node.0 >> 32;
                    if pane != 0 && self.panes.contains_key(&pane) {
                        self.focus(pane);
                    } else if (TAB_BASE..TAB_BASE + self.w.tabs.len() as u64).contains(&request.target_node.0) {
                        let tab = (request.target_node.0 - TAB_BASE) as usize;
                        self.focus(self.w.tabs[tab].focus);
                    }
                }
                Action::SetValue if request.target_node == INPUT => {
                    if let Some(accesskit::ActionData::Value(value)) = request.data
                        && value.len() <= 1 << 20
                        && let Some(input) = self.w.input.as_mut()
                    {
                        input.text.clear();
                        input.cursor = 0;
                        self.input_text(&value);
                    }
                }
                _ => {}
            }
        }
        self.w.dirty = true;
        self.update_accessibility();
    }

    pub(super) fn update_accessibility(&mut self) {
        if self.w.tabs.is_empty() {
            return;
        }
        let Some(mut adapter) = self.w.accessibility.take() else {
            return;
        };
        adapter.update_window_focus_state(self.w.focused);
        adapter.update_if_active(|| self.accessibility_tree());
        self.w.accessibility = Some(adapter);
    }

    fn surface_semantics(
        &self,
        surface_id: u32,
        root: NodeId,
        rect: crate::layout::Rect,
        nodes: &mut Vec<(NodeId, Node)>,
    ) -> (Vec<NodeId>, Option<NodeId>) {
        use proteus_wasm::types::{AccessibilityAction as A, AccessibilityRole as R};
        let Some(surface) = self.pl.surfaces.get(&surface_id) else {
            return (vec![], None);
        };
        let Some(frame) = &surface.frame else {
            return (vec![], None);
        };
        let base = SURFACE_BIT | ((surface_id as u64) << 32);
        let key = |id: &str| surface.accessibility_ids.get(id).map(|local| NodeId(base + *local as u64));
        let (cw, ch) = self.cell();
        let mut children = std::collections::HashMap::<NodeId, Vec<NodeId>>::new();
        let mut built = Vec::new();
        let mut focus = None;
        for item in &frame.accessibility {
            let Some(id) = key(&item.id) else {
                continue;
            };
            let role = match item.role {
                R::Group => Role::GenericContainer,
                R::Text => Role::Label,
                R::Button => Role::Button,
                R::TextInput => Role::TextInput,
                R::List => Role::ListBox,
                R::ListItem => Role::ListBoxOption,
                R::Image => Role::Image,
                R::Status => Role::Status,
                R::Dialog => Role::Dialog,
            };
            let mut node = Node::new(role);
            if matches!(item.role, R::Text) {
                node.set_value(&item.label);
            } else {
                node.set_label(&item.label);
            }
            if let Some(value) = &item.value {
                node.set_value(value);
            }
            if let Some(description) = &item.description {
                node.set_description(description);
            }
            if let Some(selected) = item.selected {
                node.set_selected(selected);
            }
            if let Some(expanded) = item.expanded {
                node.set_expanded(expanded);
            }
            if let Some(position) = item.position {
                node.set_position_in_set(position as usize);
            }
            if let Some(size) = item.set_size {
                node.set_size_of_set(size as usize);
            }
            if item.disabled {
                node.set_disabled();
            }
            if matches!(item.role, R::Status) {
                node.set_live(accesskit::Live::Polite);
            }
            let x = (rect.x as f64 + item.col as f64 * cw as f64).min(rect.right() as f64);
            let y = (rect.y as f64 + item.row as f64 * ch as f64).min(rect.bottom() as f64);
            let right = (x + item.cols as f64 * cw as f64).min(rect.right() as f64);
            let bottom = (y + item.rows as f64 * ch as f64).min(rect.bottom() as f64);
            let bounds = Rect::new(x, y, right, bottom);
            node.set_bounds(bounds);
            if x >= right || y >= bottom {
                node.set_hidden();
            }
            for action in &item.actions {
                match action {
                    A::Focus => node.add_action(Action::Focus),
                    A::Select => node.add_action(Action::Click),
                    A::Activate if matches!(item.role, R::ListItem) && item.actions.contains(&A::Select) => {}
                    A::Activate => node.add_action(Action::Click),
                    A::SetValue => node.add_action(Action::SetValue),
                    A::ScrollUp => node.add_action(Action::ScrollUp),
                    A::ScrollDown => node.add_action(Action::ScrollDown),
                }
            }
            let mut button = |offset: u64, label: String| {
                let button_id = NodeId(id.0 + offset);
                let mut button = Node::new(Role::Button);
                button.set_label(label);
                button.set_bounds(bounds);
                button.add_action(Action::Click);
                if item.actions.contains(&A::Focus) {
                    button.add_action(Action::Focus);
                }
                if item.disabled {
                    button.set_disabled();
                }
                children.entry(id).or_default().push(button_id);
                nodes.push((button_id, button));
            };
            if matches!(item.role, R::ListItem)
                && item.actions.contains(&A::Select)
                && item.actions.contains(&A::Activate)
            {
                button(1, format!("{} {}", self.cfg.i18n.t("a11y-activate", &[]), item.label));
            }
            if matches!(item.role, R::List) {
                if item.actions.contains(&A::ScrollUp) {
                    button(1, self.cfg.i18n.t("a11y-previous-items", &[]));
                }
                if item.actions.contains(&A::ScrollDown) {
                    button(2, self.cfg.i18n.t("a11y-next-items", &[]));
                }
            }
            if matches!(item.role, R::TextInput) {
                let text_id = NodeId(id.0 + 1);
                nodes.push((text_id, text_run(item.value.as_deref().unwrap_or(""), bounds)));
                children.entry(id).or_default().push(text_id);
            }
            let parent = item.parent.as_deref().and_then(key).unwrap_or(root);
            children.entry(parent).or_default().push(id);
            if item.focused && !item.disabled {
                focus = Some(id);
            }
            built.push((id, node));
        }
        for (id, mut node) in built {
            node.set_children(children.remove(&id).unwrap_or_default());
            nodes.push((id, node));
        }
        (children.remove(&root).unwrap_or_default(), focus)
    }

    fn semantic_action(&mut self, request: &ActionRequest) {
        use proteus_wasm::types::{AccessibilityAction as A, AccessibilityEvent, AccessibilityRole as R, UiEvent};
        let surface_id = ((request.target_node.0 & !SURFACE_BIT) >> 32) as u32;
        let local = request.target_node.0 as u32;
        let Some(surface) = self.pl.surfaces.get(&surface_id).filter(|s| s.win == self.w.id) else {
            return;
        };
        if local == 0 && matches!(request.action, Action::Focus | Action::Click) {
            self.focus_surface(surface_id);
            return;
        }
        let Some((id, _)) = surface.accessibility_ids.iter().find(|(_, n)| **n == (local & !3)) else {
            return;
        };
        let Some(item) = surface.frame.as_ref().and_then(|f| f.accessibility.iter().find(|n| n.id == *id)) else {
            return;
        };
        if item.disabled {
            return;
        }
        let offset = local & 3;
        let action = match request.action {
            Action::Focus => A::Focus,
            Action::Click if offset == 1 && matches!(item.role, R::ListItem) => A::Activate,
            Action::Click if offset == 1 && matches!(item.role, R::List) => A::ScrollUp,
            Action::Click if offset == 2 && matches!(item.role, R::List) => A::ScrollDown,
            Action::Click if offset != 0 => return,
            Action::Click if matches!(item.role, R::ListItem) && item.actions.contains(&A::Select) => A::Select,
            Action::Click => A::Activate,
            Action::SetValue if offset == 0 => A::SetValue,
            Action::ScrollUp => A::ScrollUp,
            Action::ScrollDown => A::ScrollDown,
            _ => return,
        };
        if !item.actions.contains(&action) {
            return;
        }
        let value = if action == A::SetValue {
            let Some(accesskit::ActionData::Value(value)) = &request.data else {
                return;
            };
            if value.len() > 16384 {
                return;
            }
            Some(value.to_string())
        } else {
            None
        };
        let event = AccessibilityEvent { node: id.clone(), action, value };
        if let Some(pane) = surface.pane {
            self.focus(pane);
        } else {
            self.focus_surface(surface_id);
        }
        self.surface_ui(surface_id, UiEvent::Accessibility(event));
        self.w.dirty = true;
    }

    fn accessibility_tree(&self) -> TreeUpdate {
        let mut root = Node::new(Role::Window);
        root.set_label(if self.w.window_title.is_empty() { "Proteus" } else { &self.w.window_title });
        let (width, height) = self.physical();
        root.set_bounds(Rect::new(0.0, 0.0, width as f64, height as f64));
        let mut nodes = Vec::new();
        let mut tabs = Node::new(Role::TabList);
        tabs.set_label(self.cfg.i18n.t("a11y-tabs", &[]));
        let mut tab_children = Vec::new();
        for (index, tab) in self.w.tabs.iter().enumerate() {
            let id = NodeId(TAB_BASE + index as u64);
            let mut node = Node::new(Role::Tab);
            let title = tab.name.as_deref().unwrap_or_else(|| self.panes[&tab.focus].title());
            node.set_label(format!("{}: {title}", index + 1));
            node.set_selected(index == self.w.active);
            node.add_action(Action::Focus);
            node.add_action(Action::Click);
            tab_children.push(id);
            nodes.push((id, node));
        }
        tabs.set_children(tab_children);
        nodes.push((TABS, tabs));
        let mut children = vec![TABS];
        let mut focus = ROOT;
        let view = self.view();
        let visible = view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id));
        let (cw, ch) = self.cell();
        for id in visible {
            let Some(pane) = self.panes.get(&id) else {
                continue;
            };
            let Some(rect) = view.rect(id) else {
                continue;
            };
            let mut node = Node::new(Role::Terminal);
            node.set_label(pane.name.as_deref().unwrap_or_else(|| pane.title()));
            node.set_read_only();
            node.add_action(Action::Focus);
            node.set_bounds(Rect::new(rect.x as f64, rect.y as f64, rect.right() as f64, rect.bottom() as f64));
            if let Some(surface_id) = pane.surface.filter(|sid| {
                self.pl.surfaces.get(sid).and_then(|s| s.frame.as_ref()).is_some_and(|f| !f.accessibility.is_empty())
            }) {
                node.set_role(Role::GenericContainer);
                if !pane.private {
                    let (items, selected) = self.surface_semantics(surface_id, pane_node(id), rect, &mut nodes);
                    node.set_children(items);
                    if id == self.focused_id() && self.w.surface_focus.is_none() {
                        focus = selected.unwrap_or(pane_node(id));
                    }
                }
                nodes.push((pane_node(id), node));
                children.push(pane_node(id));
                continue;
            }
            if !pane.private {
                let (text, caret) = terminal_text(&pane.term, id, rect, cw, ch, &mut nodes);
                node.set_children(text);
                if let Some(position) = caret {
                    node.set_text_selection(TextSelection { anchor: position, focus: position });
                }
            } else {
                node.set_description(self.cfg.i18n.t("a11y-private-pane", &[]));
            }
            nodes.push((pane_node(id), node));
            children.push(pane_node(id));
            if id == self.focused_id() && self.w.surface_focus.is_none() {
                focus = pane_node(id);
            }
        }
        for placed in self.surfaces_here() {
            let Some(surface) = self.pl.surfaces.get(&placed.id) else {
                continue;
            };
            let base = NodeId(SURFACE_BIT | ((placed.id as u64) << 32));
            let text_id = NodeId(base.0 + 1);
            let mut node = Node::new(Role::Document);
            node.set_label(&surface.title);
            node.add_action(Action::Focus);
            let r = placed.content;
            let bounds = Rect::new(r.x as f64, r.y as f64, r.right() as f64, r.bottom() as f64);
            node.set_bounds(bounds);
            let semantic = surface.frame.as_ref().is_some_and(|f| !f.accessibility.is_empty());
            let selected = if semantic {
                let (items, selected) = self.surface_semantics(placed.id, base, r, &mut nodes);
                node.set_children(items);
                selected
            } else {
                node.set_children([text_id]);
                nodes.push((text_id, text_run(&surface.term.screen_text(), bounds)));
                None
            };
            nodes.push((base, node));
            children.push(base);
            if self.w.surface_focus == Some(placed.id) {
                focus = selected.unwrap_or(base);
            }
        }
        if let Some(input) = &self.w.input {
            let mut node = Node::new(Role::TextInput);
            node.set_label(self.cfg.i18n.t(input.purpose.badge(), &[]));
            node.set_value(&input.text);
            node.set_description(&input.status);
            node.add_action(Action::SetValue);
            node.set_children([INPUT_TEXT]);
            let bounds = Rect::new(0.0, height.saturating_sub(ch) as f64, width as f64, height as f64);
            node.set_bounds(bounds);
            let pos = TextPosition {
                node: INPUT_TEXT,
                character_index: character_lengths(&input.text[..input.cursor]).len(),
            };
            node.set_text_selection(TextSelection { anchor: pos, focus: pos });
            nodes.push((INPUT_TEXT, text_run(&input.text, bounds)));
            nodes.push((INPUT, node));
            children.push(INPUT);
            focus = INPUT;
        }
        if !self.w.toasts.is_empty() {
            let mut status = Node::new(Role::Status);
            status.set_label(self.w.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n"));
            status.set_live(accesskit::Live::Polite);
            nodes.push((STATUS, status));
            children.push(STATUS);
        }
        if let Some(prompt) = self.w.prompts.front() {
            let overlay = prompt.overlay(&self.cfg.i18n);
            let mut node = Node::new(Role::AlertDialog);
            node.set_label(overlay.title);
            node.set_description([overlay.detail.unwrap_or_default(), overlay.keys].join("\n"));
            node.set_live(accesskit::Live::Assertive);
            nodes.push((PROMPT, node));
            children.push(PROMPT);
            focus = PROMPT;
        }
        root.set_children(children);
        nodes.push((ROOT, root));
        let mut tree = TreeInfo::new(ROOT);
        tree.toolkit_name = Some("Proteus".into());
        tree.toolkit_version = Some(env!("CARGO_PKG_VERSION").into());
        TreeUpdate { nodes, tree: Some(tree), tree_id: TreeId::ROOT, focus }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bidi_accessibility_runs_keep_logical_text_and_visual_geometry() {
        let mut t = proteus_core::Terminal::new(20, 2, Default::default());
        t.advance("abc אבג 123".as_bytes());
        let mut nodes = vec![];
        let (children, caret) = terminal_text(&t, 1, crate::layout::Rect::new(10, 20, 200, 40), 10, 20, &mut nodes);
        assert!(caret.is_some());
        let text: String =
            children.iter().filter_map(|id| nodes.iter().find(|(n, _)| n == id).and_then(|(_, n)| n.value())).collect();
        assert!(text.starts_with("abc אבג 123"));
        assert!(nodes.iter().any(|(_, n)| n.text_direction() == Some(accesskit::TextDirection::RightToLeft)));
        for (_, node) in nodes {
            assert_eq!(node.character_lengths().len(), node.character_positions().unwrap().len());
            assert_eq!(node.character_lengths().len(), node.character_widths().unwrap().len());
            assert!(
                node.character_positions().unwrap().windows(2).all(|p| p[0] <= p[1]),
                "each directional run must be monotonic"
            );
        }
    }

    #[test]
    fn text_runs_preserve_unicode_and_bound_character_lengths() {
        let text = format!("e\u{301} 中 👨‍👩‍👧‍👦 x{}\n", "\u{301}".repeat(300));
        let node = text_run(&text, Rect::new(0.0, 0.0, 100.0, 20.0));
        assert_eq!(node.value(), Some(text.as_str()));
        assert_eq!(node.character_lengths().iter().map(|n| *n as usize).sum::<usize>(), text.len());
        assert!(node.character_lengths().iter().all(|n| *n > 0));
        assert_ne!(pane_node(1), text_node(1, 0));
        assert_ne!(text_node(1, 100), pane_node(2));
    }
}
