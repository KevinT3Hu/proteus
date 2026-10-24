//! Declarative UI: components return a tree, and the SDK owns drawing and layout.
use super::{
    AccessibilityAction as A11yAction, AccessibilityRole as Role, Canvas, ImageView, Length, Page, Rect, Semantics,
};
use crate::{
    List, TextInput, text,
    types::{Frame, Line, Style},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub width: usize,
    pub height: usize,
}
impl From<Rect> for Size {
    fn from(r: Rect) -> Self {
        Self { width: r.width, height: r.height }
    }
}

/// Implement this for a UI or a reusable part of one. A view may borrow its
/// component's state; the tree is consumed before render returns. No plugin
/// lifecycle, host calls, or drawing coordinates are required.
pub trait Component {
    fn view(&mut self) -> Element<'_>;
    fn render(&mut self, cols: usize, rows: usize) -> Rendered
    where
        Self: Sized,
    {
        render(self, cols, rows)
    }
}

/// The rendered frame and named regions from that exact layout pass.
pub struct Rendered {
    pub frame: Frame,
    regions: Vec<(String, Rect)>,
}
impl Rendered {
    /// IDs should be unique; if repeated, the last rendered occurrence wins.
    pub fn area(&self, id: &str) -> Option<Rect> {
        self.regions.iter().rev().find(|(key, _)| key == id).map(|(_, area)| *area)
    }
    /// The deepest/last named region under this point, in zero-based cells.
    pub fn hit(&self, col: usize, row: usize) -> Option<(&str, Rect)> {
        self.regions.iter().rev().find(|(_, area)| area.contains(col, row)).map(|(key, area)| (key.as_str(), *area))
    }
}

pub fn render(component: &mut (impl Component + ?Sized), cols: usize, rows: usize) -> Rendered {
    component.view().render(cols, rows)
}

type RowSemantics<'a> = Box<dyn FnMut(usize, bool) -> Semantics + 'a>;
type RowRenderer<'a> = Box<dyn FnMut(usize, bool, usize) -> Line + 'a>;
enum Kind<'a> {
    Empty,
    Text(String),
    Lines(std::borrow::Cow<'a, [Line]>),
    Bar(Line, Line),
    Input { prefix: Line, field: TextInput, focused: bool },
    Image(ImageView),
    List { state: &'a mut List, row: RowRenderer<'a>, empty: Line, semantics: Option<RowSemantics<'a>> },
    Stack { horizontal: bool, children: Vec<Element<'a>>, gap: usize, separator: Option<Style> },
    Page { header: Box<Element<'a>>, body: Box<Element<'a>>, footer: Box<Element<'a>> },
    Responsive(Box<dyn FnOnce(Size) -> Element<'a> + 'a>),
    Component(Box<dyn Component + 'a>),
}

/// An ephemeral view description. Builders describe *what* is shown; the
/// renderer computes child geometry, padding, cursors and image placements.
pub struct Element<'a> {
    kind: Kind<'a>,
    width: Length,
    height: Length,
    style: Style,
    id: Option<String>,
    semantics: Option<Semantics>,
}
impl<'a> Element<'a> {
    fn new(kind: Kind<'a>) -> Self {
        Self { kind, width: Length::Fill(1), height: Length::Fill(1), style: Style::PLAIN, id: None, semantics: None }
    }
    pub fn empty() -> Self {
        Self::new(Kind::Empty)
    }
    pub fn text(value: impl Into<String>) -> Self {
        Self::new(Kind::Text(value.into()))
    }
    /// A labeled control. Handle Focus/Activate through UiEvent::Accessibility.
    pub fn button(label: impl Into<String>) -> Self {
        let label = label.into();
        Self::text(label.clone())
            .semantics(Semantics::new(Role::Button).label(label).actions([A11yAction::Focus, A11yAction::Activate]))
    }
    pub fn line(line: Line) -> Self {
        Self::lines(vec![line])
    }
    pub fn lines(lines: Vec<Line>) -> Self {
        Self::new(Kind::Lines(lines.into()))
    }
    /// Borrow large text buffers; only visible rows are copied and styled.
    pub fn borrowed_lines(lines: &'a [Line]) -> Self {
        Self::new(Kind::Lines(std::borrow::Cow::Borrowed(lines)))
    }
    pub fn bar(left: Line, right: Line) -> Self {
        Self::new(Kind::Bar(left, right))
    }
    pub fn input(prefix: Line, input: &TextInput) -> Self {
        Self::new(Kind::Input { prefix, field: input.clone(), focused: true })
    }
    /// Mark the current control (or selected list item) as focused. For inputs,
    /// also shows the visual cursor. Event routing stays with the component.
    pub fn focused(mut self, focused: bool) -> Self {
        if let Kind::Input { focused: on, .. } = &mut self.kind {
            *on = focused
        }
        let semantics = self.semantics.get_or_insert_with(|| inferred(&self.kind));
        semantics.focused = focused;
        self
    }
    pub fn image(image: ImageView) -> Self {
        Self::new(Kind::Image(image))
    }
    /// Rows are formatted only after the list's available width is known.
    pub fn list(state: &'a mut List, row: impl FnMut(usize, bool, usize) -> Line + 'a) -> Self {
        Self::new(Kind::List { state, row: Box::new(row), empty: Vec::new(), semantics: None })
    }
    /// Semantic metadata for each visible list item. Supply stable IDs for sorted/filtered data.
    pub fn item_semantics(mut self, item: impl FnMut(usize, bool) -> Semantics + 'a) -> Self {
        if let Kind::List { semantics, .. } = &mut self.kind {
            *semantics = Some(Box::new(item));
        }
        self
    }
    pub fn semantics(mut self, semantics: Semantics) -> Self {
        self.semantics = Some(semantics);
        self
    }
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.semantics.get_or_insert_with(|| inferred(&self.kind)).label = Some(label.into());
        self
    }
    pub fn empty_message(mut self, message: Line) -> Self {
        if let Kind::List { empty, .. } = &mut self.kind {
            *empty = message
        }
        self
    }
    pub fn row(children: impl IntoIterator<Item = Self>) -> Self {
        Self::new(Kind::Stack { horizontal: true, children: children.into_iter().collect(), gap: 0, separator: None })
    }
    pub fn column(children: impl IntoIterator<Item = Self>) -> Self {
        Self::new(Kind::Stack { horizontal: false, children: children.into_iter().collect(), gap: 0, separator: None })
    }
    pub fn page(header: Self, body: Self, footer: Self) -> Self {
        Self::new(Kind::Page { header: Box::new(header), body: Box::new(body), footer: Box::new(footer) })
    }
    pub fn responsive(view: impl FnOnce(Size) -> Self + 'a) -> Self {
        Self::new(Kind::Responsive(Box::new(view)))
    }
    pub fn component(component: impl Component + 'a) -> Self {
        Self::new(Kind::Component(Box::new(component)))
    }
    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }
    pub fn height(mut self, height: Length) -> Self {
        self.height = height;
        self
    }
    /// Inherits into descendants; explicit span colors take precedence.
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn gap(mut self, gap: usize) -> Self {
        if let Kind::Stack { gap: g, .. } = &mut self.kind {
            *g = gap
        }
        self
    }
    /// One-cell separators between row/column children, styled independently.
    pub fn separated(mut self, style: Style) -> Self {
        if let Kind::Stack { gap, separator, .. } = &mut self.kind {
            *gap = 1;
            *separator = Some(style)
        }
        self
    }
    pub fn render(self, cols: usize, rows: usize) -> Rendered {
        let mut renderer = Renderer {
            canvas: Canvas::new(cols, rows),
            regions: Vec::new(),
            semantics: Vec::new(),
            parent: None,
            next: 0,
        };
        let root = renderer.canvas.area();
        renderer.draw(self, root, Style::PLAIN);
        let mut frame = renderer.canvas.finish();
        // The last focused control wins (e.g. a filter field after its list).
        let focus = renderer.semantics.iter().rposition(|n| n.focused && !n.disabled);
        for (i, node) in renderer.semantics.iter_mut().enumerate() {
            node.focused = Some(i) == focus;
        }
        frame.accessibility = renderer.semantics;
        Rendered { frame, regions: renderer.regions }
    }
}

fn line_text(line: &Line) -> String {
    line.iter().map(|s| s.text.as_str()).collect()
}
fn inferred(kind: &Kind<'_>) -> Semantics {
    match kind {
        Kind::Text(value) => Semantics::new(Role::Text).label(value),
        Kind::Lines(_) => Semantics::new(Role::Group),
        Kind::Bar(left, right) => {
            Semantics::new(Role::Text).label(format!("{} {}", line_text(left), line_text(right)).trim())
        }
        Kind::Input { prefix, field, focused } => Semantics::new(Role::TextInput)
            .label(line_text(prefix))
            .value(&field.text)
            .focused(*focused)
            .actions([A11yAction::Focus, A11yAction::SetValue]),
        Kind::List { .. } => {
            Semantics::new(Role::List).actions([A11yAction::Focus, A11yAction::ScrollUp, A11yAction::ScrollDown])
        }
        Kind::Image(_) => Semantics::new(Role::Image),
        _ => Semantics::new(Role::Group),
    }
}

fn inherit(child: Style, parent: Style) -> Style {
    Style { fg: child.fg.or(parent.fg), bg: child.bg.or(parent.bg), attrs: child.attrs | parent.attrs }
}
fn styled_line(mut line: Line, style: Style) -> Line {
    for span in &mut line {
        span.style = inherit(span.style, style)
    }
    line
}
fn styled(lines: Vec<Line>, style: Style) -> Vec<Line> {
    lines.into_iter().map(|line| styled_line(line, style)).collect()
}
struct Renderer {
    canvas: Canvas,
    regions: Vec<(String, Rect)>,
    semantics: Vec<crate::types::AccessibleNode>,
    parent: Option<String>,
    next: usize,
}
impl Renderer {
    fn draw(&mut self, element: Element<'_>, area: Rect, inherited: Style) {
        if let Some(id) = &element.id {
            self.regions.push((id.clone(), area));
        }
        let style = inherit(element.style, inherited);
        if area.is_empty() {
            return;
        }
        let transparent = matches!(element.kind, Kind::Responsive(_) | Kind::Component(_) | Kind::Empty)
            && element.semantics.is_none();
        let previous = self.parent.clone();
        let mut sem = element.semantics.unwrap_or_else(|| inferred(&element.kind));
        if sem.label.is_none() {
            sem.label = inferred(&element.kind).label;
        }
        if sem.value.is_none() {
            sem.value = inferred(&element.kind).value;
        }
        let focused = sem.focused;
        if !transparent {
            self.next += 1;
            let id = element.id.unwrap_or_else(|| format!("@{}", self.next));
            let node = sem.node(id, previous.clone(), area);
            self.parent = Some(node.id.clone());
            self.semantics.push(node);
        }
        match element.kind {
            Kind::Empty => self.canvas.lines(area, &[], style),
            Kind::Text(value) => self.canvas.line(area, styled_line(vec![text(value)], style), style),
            Kind::Lines(lines) => {
                for (i, line) in lines.iter().take(area.height).enumerate() {
                    let label = line_text(&crate::widgets::clip(line, area.width));
                    if label.is_empty() {
                        continue;
                    }
                    let parent = self.parent.clone();
                    let id = format!("{}/line/{i}", parent.as_deref().unwrap_or("text"));
                    let node = Semantics::new(Role::Text).label(label).node(
                        id,
                        parent,
                        Rect::new(area.x, area.y + i, area.width, 1),
                    );
                    self.semantics.push(node);
                }
                self.canvas.lines(area, &styled(lines.iter().take(area.height).cloned().collect(), style), style)
            }
            Kind::Bar(left, right) => {
                let line = super::bar(styled_line(left, style), styled_line(right, style), area.width, style);
                self.canvas.line(area, line, style);
            }
            Kind::Input { prefix, field, focused } => {
                let prefix = styled_line(prefix, style);
                let (line, cursor) = super::input(prefix, &field, area.width, style);
                self.canvas.line(area, line, style);
                if let Some(col) = cursor.filter(|_| focused) {
                    self.canvas.cursor(area, col, 0)
                }
            }
            Kind::Image(image) => {
                self.canvas.lines(area, &[], style);
                if let Some(image) = image.fit(area.width, area.height) {
                    self.canvas.image(area, image);
                }
            }
            Kind::List { state, mut row, empty, mut semantics } => {
                let parent = self.parent.clone();
                let mut items = Vec::new();
                let mut lines = state.render_rows(area.width, area.height, style, |i, on| {
                    let line = row(i, on, area.width);
                    let sem = semantics.as_mut().map(|f| f(i, on)).unwrap_or_else(|| {
                        Semantics::new(Role::ListItem).label(line_text(&line)).selected(on).actions([
                            A11yAction::Focus,
                            A11yAction::Select,
                            A11yAction::Activate,
                        ])
                    });
                    let index = items.len();
                    let mut node = sem.node(
                        format!("{}/item/{i}", parent.as_deref().unwrap_or("list")),
                        parent.clone(),
                        Rect::new(area.x, area.y + index, area.width, 1),
                    );
                    node.focused = focused && on && !node.disabled;
                    node.position = Some(i.saturating_add(1).min(u32::MAX as usize) as u32);
                    items.push(node);
                    line
                });
                if let Some(list) = self.semantics.last_mut() {
                    list.focused = false;
                    list.set_size = Some(state.len.min(u32::MAX as usize) as u32);
                }
                for node in &mut items {
                    node.set_size = Some(state.len.min(u32::MAX as usize) as u32);
                }
                self.semantics.extend(items);
                if state.len == 0 && !lines.is_empty() {
                    lines[0] = empty;
                }
                self.canvas.lines(area, &styled(lines, style), style);
            }
            Kind::Page { header, body, footer } => {
                let page = Page::new(area);
                self.draw(*header, page.header, style);
                self.draw(*body, page.body, style);
                self.draw(*footer, page.footer, style);
            }
            Kind::Stack { horizontal, children, gap, separator } => {
                self.canvas.lines(area, &[], style);
                let sizes: Vec<_> = children.iter().map(|c| if horizontal { c.width } else { c.height }).collect();
                let regions = if horizontal { area.columns(&sizes, gap) } else { area.rows(&sizes, gap) };
                if let Some(separator) = separator {
                    for pair in regions.windows(2) {
                        let a = pair[0];
                        let b = pair[1];
                        if horizontal {
                            self.canvas.divider(
                                Rect::new(a.right(), area.y, b.x.saturating_sub(a.right()), area.height),
                                separator,
                            )
                        } else {
                            let line = styled_line(vec![text("─".repeat(area.width))], separator);
                            let gap = Rect::new(area.x, a.bottom(), area.width, b.y.saturating_sub(a.bottom()));
                            self.canvas.lines(gap, &vec![line; gap.height], separator);
                        }
                    }
                }
                for (child, area) in children.into_iter().zip(regions) {
                    self.draw(child, area, style)
                }
            }
            Kind::Responsive(view) => self.draw(view(area.into()), area, style),
            Kind::Component(mut component) => self.draw(component.view(), area, style),
        }
        self.parent = previous;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Attrs, Color};
    fn string(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }
    struct Items<'a>(&'a mut List);
    impl Component for Items<'_> {
        fn view(&mut self) -> Element<'_> {
            Element::list(self.0, |i, on, _| {
                let value = text(format!("Item {i}"));
                vec![if on { value.reverse() } else { value }]
            })
            .id("items")
        }
    }
    struct Panel {
        list: List,
        filter: TextInput,
    }
    impl Component for Panel {
        fn view(&mut self) -> Element<'_> {
            let footer = Element::input(vec![text("/")], &self.filter).id("filter");
            let list = &mut self.list;
            Element::page(
                Element::text("Header").id("header"),
                Element::responsive(move |size| {
                    let items = Element::component(Items(list));
                    if size.width >= 20 {
                        Element::row([items.width(Length::Percent(60)), Element::text("Preview").id("preview")])
                            .separated(Style::PLAIN)
                    } else {
                        items
                    }
                }),
                footer,
            )
        }
    }
    #[test]
    fn semantics_follow_virtualized_layout_and_unique_focus() {
        let mut list = List { len: 100, selected: 51, offset: 50 };
        let field = TextInput::new("query");
        let frame = Element::page(
            Element::text("Title"),
            Element::list(&mut list, |i, _, _| vec![text(format!("item {i}"))])
                .id("list")
                .focused(true)
                .item_semantics(|i, on| {
                    Semantics::new(Role::ListItem).id(format!("row:{i}")).label(format!("full item {i}")).selected(on)
                }),
            Element::input(vec![], &field).id("filter"),
        )
        .render(10, 5)
        .frame;
        let items: Vec<_> = frame.accessibility.iter().filter(|n| n.parent.as_deref() == Some("list")).collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "row:50");
        assert_eq!((items[0].col, items[0].row, items[0].cols, items[0].rows), (0, 1, 10, 1));
        assert_eq!(items[1].position, Some(52));
        assert_eq!(items[1].set_size, Some(100));
        let focus: Vec<_> = frame.accessibility.iter().filter(|n| n.focused).collect();
        assert_eq!(focus.len(), 1);
        assert_eq!(focus[0].id, "filter");
        assert_eq!(focus[0].value.as_deref(), Some("query"));
        assert!(Element::text("invisible").render(0, 0).frame.accessibility.is_empty());
    }

    #[test]
    fn custom_components_borrow_state_and_share_layout_with_hit_testing() {
        let mut ui = Panel { list: List { len: 100, selected: 99, offset: 0 }, filter: TextInput::new("abc") };
        let wide = ui.render(30, 6);
        assert_eq!(wide.area("items"), Some(Rect::new(0, 1, 17, 4)));
        assert_eq!(wide.area("preview"), Some(Rect::new(18, 1, 12, 4)));
        assert_eq!(wide.hit(18, 2).unwrap().0, "preview");
        assert_eq!(ui.list.offset, 96);
        assert_eq!(string(&wide.frame.lines[1])[..7].to_string(), "Item 96");
        assert_eq!(wide.frame.cursor, Some((4, 5)));
        let narrow = ui.render(10, 4);
        assert!(narrow.area("preview").is_none());
        assert_eq!(narrow.area("items"), Some(Rect::new(0, 1, 10, 2)));
        assert_eq!(ui.list.offset, 98);
        assert_eq!(ui.list.at_point(narrow.area("items").unwrap(), 2, 1), Some(98));
    }
    #[test]
    fn resize_rebuilds_the_tree_without_stale_cursor_or_regions() {
        let mut ui = Panel { list: List::default(), filter: TextInput::new("中") };
        let small = ui.render(1, 1);
        assert_eq!(small.frame.lines.len(), 1);
        assert!(small.frame.cursor.is_none());
        assert!(small.area("items").is_none());
        assert_eq!(small.area("filter"), Some(Rect::new(0, 0, 1, 1)));
        let large = ui.render(30, 6);
        assert!(large.area("preview").is_some());
        assert!(large.frame.cursor.is_some());
        let empty = ui.render(0, 0);
        assert!(empty.frame.lines.is_empty());
        assert!(empty.frame.cursor.is_none());
    }
    #[test]
    fn styles_and_named_nested_components_preserve_leaf_overrides() {
        let parent = Style { fg: Some(Color::Indexed(1)), bg: Some(Color::Indexed(2)), attrs: Attrs::BOLD };
        let row = Element::column([
            Element::line(vec![text("中👩‍💻").fg(Color::Indexed(3))]).id("leaf").height(Length::Fixed(1)),
            Element::text("below").id("below"),
        ])
        .id("root")
        .style(parent)
        .separated(parent)
        .render(4, 3);
        assert_eq!(string(&row.frame.lines[0]), "中👩‍💻");
        assert!(row.frame.lines[0][0].style.attrs.contains(Attrs::BOLD));
        assert!(matches!(row.frame.lines[0][0].style.fg, Some(Color::Indexed(3))));
        assert!(matches!(row.frame.lines[1][0].style.fg, Some(Color::Indexed(1))));
        assert_eq!(row.hit(0, 0).unwrap().0, "leaf");
        assert_eq!(row.hit(0, 2).unwrap().0, "below");
    }
    #[test]
    fn only_focused_input_places_a_cursor() {
        let a = TextInput::new("a");
        let b = TextInput::new("bb");
        let ui = Element::column([
            Element::input(vec![text("/")], &a).height(Length::Fixed(1)),
            Element::input(vec![text("/")], &b).focused(false).height(Length::Fixed(1)),
        ])
        .render(10, 2);
        assert_eq!(ui.frame.cursor, Some((2, 0)));
        assert_eq!(string(&ui.frame.lines[1]), "/bb       ");
        assert!(Element::input(Vec::new(), &a).focused(false).render(10, 1).frame.cursor.is_none());
    }

    #[test]
    fn declarative_images_fit_their_assigned_regions() {
        let ui = Element::row([
            Element::text("left").width(Length::Fixed(5)),
            Element::image(ImageView::new(7, (800, 400))).id("image"),
        ])
        .gap(1)
        .render(20, 6);
        assert_eq!(ui.area("image"), Some(Rect::new(6, 0, 14, 6)));
        assert_eq!(ui.frame.images.len(), 1);
        let image = &ui.frame.images[0];
        assert_eq!((image.col, image.row), (6, 0));
        assert!(image.cols <= 14 && image.rows <= 6);
    }
}
