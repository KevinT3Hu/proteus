//! Cell-based layout and frame composition, without host calls.
//!
//! Regions are local to a surface and use zero-based cell coordinates.
//! The same regions can be used for drawing, cursor placement and hit testing.
//!
//! Implement [`Component`] and return an [`Element`] tree. The SDK owns layout,
//! frame composition, cursors and image offsets; [`Canvas`] is the renderer backend.
//!
//! ```
//! use proteus_plugin::{List, TextInput, text, types::Style};
//! use proteus_plugin::ui::{Component, Element, Length};
//!
//! struct Browser { files: List, filter: TextInput }
//! impl Component for Browser {
//!     fn view(&mut self) -> Element<'_> {
//!         Element::page(
//!             Element::text("Browser"),
//!             Element::row([
//!                 Element::list(&mut self.files, |index, selected, _width| {
//!                     let label = text(format!("Item {index}"));
//!                     vec![if selected { label.reverse() } else { label }]
//!                 }).width(Length::Percent(60)).id("files"),
//!                 Element::text("Preview").id("preview"),
//!             ]).separated(Style::PLAIN),
//!             Element::input(vec![text("/")], &self.filter),
//!         )
//!     }
//! }
//! let mut browser = Browser { files: List { len: 100, ..List::default() }, filter: TextInput::default() };
//! let rendered = browser.render(60, 12);
//! assert_eq!(rendered.frame.lines.len(), 12);
//! let files = rendered.area("files").unwrap();
//! assert_eq!(browser.files.at_point(files, files.x, files.y), Some(0));
//! // host::draw(surface_id, &rendered.frame) presents the complete UI.
//! ```
mod semantics;
pub use semantics::{Action as AccessibilityAction, Role as AccessibilityRole, Semantics};
mod declarative;
pub use declarative::{Component, Element, Rendered, Size, render};

use crate::types::{Frame, ImagePlace, Line, Span, Style};
use crate::widgets::clip;
use crate::{TextInput, pad, width};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}
impl Rect {
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self { x, y, width, height }
    }
    pub const fn size(width: usize, height: usize) -> Self {
        Self::new(0, 0, width, height)
    }
    pub fn right(self) -> usize {
        self.x.saturating_add(self.width)
    }
    pub fn bottom(self) -> usize {
        self.y.saturating_add(self.height)
    }
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
    pub fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn local(self, x: usize, y: usize) -> Option<(usize, usize)> {
        self.contains(x, y).then(|| (x - self.x, y - self.y))
    }
    pub fn intersect(self, other: Self) -> Self {
        let (x, y) = (self.x.max(other.x), self.y.max(other.y));
        Self::new(
            x,
            y,
            self.right().min(other.right()).saturating_sub(x),
            self.bottom().min(other.bottom()).saturating_sub(y),
        )
    }
    /// Fixed widths and percentages are allocated first; weighted fills share the rest.
    /// Gaps and oversubscribed fixed sizes shrink to fit, including zero-sized surfaces.
    pub fn columns(self, lengths: &[Length], gap: usize) -> Vec<Self> {
        self.split(lengths, gap, true)
    }
    pub fn rows(self, lengths: &[Length], gap: usize) -> Vec<Self> {
        self.split(lengths, gap, false)
    }
    fn split(self, lengths: &[Length], gap: usize, horizontal: bool) -> Vec<Self> {
        if lengths.is_empty() {
            return Vec::new();
        }
        let total = if horizontal { self.width } else { self.height };
        let gaps = lengths.len() - 1;
        let gap = total.checked_div(gaps).map_or(0, |n| gap.min(n));
        let available = total - gap * gaps;
        let mut left = available;
        let mut sizes = vec![0; lengths.len()];
        let weight: u128 = lengths
            .iter()
            .map(|l| match l {
                Length::Fill(w) => *w as u128,
                _ => 0,
            })
            .sum();
        for (i, l) in lengths.iter().enumerate() {
            let n = match l {
                Length::Fixed(n) => *n,
                Length::Percent(p) => (available as u128 * (*p).min(100) as u128 / 100) as usize,
                Length::Fill(_) => 0,
            };
            sizes[i] = n.min(left);
            left -= sizes[i];
        }
        let mut cumulative = 0u128;
        let mut used = 0;
        for (i, l) in lengths.iter().enumerate() {
            if let Length::Fill(w) = l {
                cumulative += *w as u128;
                let end = (left as u128 * cumulative).checked_div(weight).unwrap_or(0) as usize;
                sizes[i] = end - used;
                used = end;
            }
        }
        let mut at = if horizontal { self.x } else { self.y };
        sizes
            .into_iter()
            .map(|n| {
                let r = if horizontal {
                    Self::new(at, self.y, n, self.height)
                } else {
                    Self::new(self.x, at, self.width, n)
                };
                at = at.saturating_add(n).saturating_add(gap);
                r
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    Fixed(usize),
    Percent(u8),
    Fill(u32),
}

/// A one-row header, a growing body and a one-row footer. On a one-row
/// surface the footer wins, keeping prompts and input usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub header: Rect,
    pub body: Rect,
    pub footer: Rect,
}
impl Page {
    pub fn new(area: Rect) -> Self {
        let footer = area.height.min(1);
        let header = area.height.saturating_sub(footer).min(1);
        Self {
            header: Rect::new(area.x, area.y, area.width, header),
            body: Rect::new(
                area.x,
                area.y.saturating_add(header),
                area.width,
                area.height.saturating_sub(header + footer),
            ),
            footer: Rect::new(area.x, area.bottom().saturating_sub(footer), area.width, footer),
        }
    }
}

/// A label on the left and metadata on the right. Each side keeps its span
/// styles and links; the right side takes priority when space is tight.
pub fn bar(left: Line, right: Line, cols: usize, fill: Style) -> Line {
    let right = clip(&right, cols);
    let right_width: usize = right.iter().map(|s| width(&s.text)).sum();
    let mut line = pad(clip(&left, cols - right_width), cols - right_width, fill);
    line.extend(right);
    line
}

/// A prefixed input field, with a region-local cursor. Returns no cursor
/// when the prefix consumes all available cells.
pub fn input(prefix: Line, field: &TextInput, cols: usize, style: Style) -> (Line, Option<usize>) {
    let mut line = clip(&prefix, cols);
    let before: usize = line.iter().map(|s| width(&s.text)).sum();
    if before >= cols {
        return (line, None);
    }
    let (body, cursor) = field.render(cols - before, style);
    line.extend(body);
    (line, Some(before + cursor as usize))
}

/// An image fitted to a cell region without upscaling or changing its aspect ratio.
/// Set cell_pixels to the intended cell aspect; the default is 8×16 pixels.
#[derive(Debug, Clone, Copy)]
pub struct ImageView {
    pub image: crate::types::ImageId,
    pub pixels: (u32, u32),
    pub cell_pixels: (u32, u32),
}
impl ImageView {
    pub fn new(image: crate::types::ImageId, pixels: (u32, u32)) -> Self {
        Self { image, pixels, cell_pixels: (8, 16) }
    }
    /// Returns a region-local placement suitable for Canvas::image.
    pub fn fit(self, cols: usize, rows: usize) -> Option<ImagePlace> {
        if cols == 0 || rows == 0 || self.pixels.0 == 0 || self.pixels.1 == 0 {
            return None;
        }
        let (w, h) = (self.pixels.0 as f64, self.pixels.1 as f64);
        let (cw, ch) = (self.cell_pixels.0.max(1) as f64, self.cell_pixels.1.max(1) as f64);
        let scale = (cols as f64 * cw / w).min(rows as f64 * ch / h).min(1.0);
        Some(ImagePlace {
            image: self.image,
            col: 0,
            row: 0,
            cols: ((w * scale / cw).ceil() as usize).max(1).min(cols).min(u32::MAX as usize) as u32,
            rows: ((h * scale / ch).ceil() as usize).max(1).min(rows).min(u32::MAX as usize) as u32,
        })
    }
}

/// Build a frame by drawing into bounded regions. Child coordinates (including
/// images and the cursor) stay local; the composer applies offsets and clipping.
pub struct Canvas {
    area: Rect,
    frame: Frame,
}
impl Canvas {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            area: Rect::size(cols, rows),
            frame: Frame {
                lines: vec![Vec::new(); rows],
                cursor: None,
                background: None,
                images: Vec::new(),
                accessibility: Vec::new(),
            },
        }
    }
    pub fn area(&self) -> Rect {
        self.area
    }
    pub fn background(&mut self, color: crate::types::Color) {
        self.frame.background = Some(color);
    }
    pub fn line(&mut self, area: Rect, line: Line, fill: Style) {
        self.lines(area, &[line], fill);
    }
    /// Replaces the entire region, padding short/missing rows with `fill`.
    pub fn lines(&mut self, area: Rect, lines: &[Line], fill: Style) {
        let visible = area.intersect(self.area);
        if visible.is_empty() {
            return;
        }
        for y in visible.y..visible.bottom() {
            let line = lines.get(y - area.y).cloned().unwrap_or_default();
            let content = slice(&line, visible.x - area.x, visible.width, fill);
            let old = &self.frame.lines[y];
            let mut out = slice(old, 0, visible.x, Style::PLAIN);
            out.extend(content);
            out.extend(slice(old, visible.right(), self.area.width - visible.right(), Style::PLAIN));
            self.frame.lines[y] = out;
        }
    }
    /// A vertical separator placed in its own layout region.
    pub fn divider(&mut self, area: Rect, style: Style) {
        let area = area.intersect(self.area);
        self.lines(area, &vec![vec![Span { text: "│".repeat(area.width), style, link: None }]; area.height], style);
    }
    pub fn cursor(&mut self, area: Rect, col: usize, row: usize) {
        if col < area.width && row < area.height {
            let (x, y) = (area.x.saturating_add(col), area.y.saturating_add(row));
            if self.area.contains(x, y) {
                self.frame.cursor = Some((x as u32, y as u32));
            }
        }
    }
    /// Returns false if a placement falls outside its region. Oversized images
    /// are rejected rather than stretched by changing their requested dimensions.
    pub fn image(&mut self, area: Rect, mut image: ImagePlace) -> bool {
        let (x, y) = (image.col as usize, image.row as usize);
        let bounds = area.intersect(self.area);
        if image.cols == 0
            || image.rows == 0
            || x.saturating_add(image.cols as usize) > bounds.width
            || y.saturating_add(image.rows as usize) > bounds.height
        {
            return false;
        }
        image.col = area.x.saturating_add(x) as u32;
        image.row = area.y.saturating_add(y) as u32;
        self.frame.images.push(image);
        true
    }
    pub fn finish(self) -> Frame {
        self.frame
    }
}

/// A cell slice which pads partial wide characters instead of shifting neighbors.
fn slice(line: &Line, start: usize, cols: usize, fill: Style) -> Line {
    use unicode_segmentation::UnicodeSegmentation;
    let mut out = Vec::new();
    let mut at = 0;
    let end = start.saturating_add(cols);
    for s in line {
        let mut value = String::new();
        for c in s.text.graphemes(true) {
            let n = width(c);
            let next = at + n;
            if n == 0 {
                if at > start && at <= end {
                    value.push_str(c)
                }
            } else if at >= start && next <= end {
                value.push_str(c)
            } else if next > start && at < end {
                value.push_str(&" ".repeat(next.min(end) - at.max(start)));
            }
            at = next;
            if at > end {
                break;
            }
        }
        if !value.is_empty() {
            out.push(Span { text: value, ..s.clone() });
        }
        if at >= end {
            break;
        }
    }
    pad(out, cols, fill)
}

impl crate::List {
    /// Render visible items through a plugin-supplied row component. The SDK
    /// clips and pads the result, including empty rows after the final item.
    pub fn render_rows(
        &mut self,
        cols: usize,
        rows: usize,
        fill: Style,
        mut item: impl FnMut(usize, bool) -> Line,
    ) -> Vec<Line> {
        let mut lines: Vec<_> = self.visible(rows).map(|(i, on)| pad(clip(&item(i, on), cols), cols, fill)).collect();
        lines.resize_with(rows, || pad(Vec::new(), cols, fill));
        lines
    }
    pub fn at_point(&self, area: Rect, col: usize, row: usize) -> Option<usize> {
        let (_, row) = area.local(col, row)?;
        self.at_row(row)
    }
    /// Scroll while keeping the selection inside the viewport.
    pub fn scroll(&mut self, by: isize, rows: usize) {
        if rows == 0 {
            return;
        }
        self.offset = self.offset.saturating_add_signed(by).min(self.len.saturating_sub(rows));
        self.selected =
            self.selected.clamp(self.offset, self.offset.saturating_add(rows - 1).min(self.len.saturating_sub(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Attrs, Color};
    use crate::{List, text};
    fn string(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }
    #[test]
    fn layouts_fit_small_surfaces_and_distribute_remaining_cells() {
        let root = Rect::new(2, 3, 17, 5);
        let parts = root.columns(&[Length::Fixed(3), Length::Fill(1), Length::Fill(2)], 1);
        assert_eq!(parts, [Rect::new(2, 3, 3, 5), Rect::new(6, 3, 4, 5), Rect::new(11, 3, 8, 5)]);
        assert_eq!(root.columns(&[Length::Percent(50), Length::Fill(1)], 1)[0].width, 8);
        for cols in 0..20 {
            let parts = Rect::size(cols, 2).columns(&[Length::Fixed(30), Length::Fill(1), Length::Fixed(20)], 1);
            for r in parts {
                assert!(r.right() <= cols);
            }
        }
        assert_eq!(root.local(2, 3), Some((0, 0)));
        assert_eq!(root.local(19, 3), None);
        assert!(Rect::size(0, 0).columns(&[], 1).is_empty());
    }
    #[test]
    fn page_reserves_footer_even_on_one_row() {
        for rows in 0..5 {
            let p = Page::new(Rect::size(8, rows));
            assert_eq!(p.header.height + p.body.height + p.footer.height, rows);
            assert!(p.footer.bottom() <= rows);
        }
        let p = Page::new(Rect::size(8, 1));
        assert_eq!((p.header.height, p.body.height, p.footer.height), (0, 0, 1));
    }
    #[test]
    fn columns_clip_styles_and_wide_cells_without_corrupting_neighbors() {
        let mut canvas = Canvas::new(10, 2);
        let cells = canvas.area().columns(&[Length::Fixed(4), Length::Fill(1)], 1);
        canvas.lines(cells[0], &[vec![text("中文中文").bold()]], Style::PLAIN);
        canvas.divider(Rect::new(4, 0, 1, 2), Style::PLAIN);
        canvas.lines(cells[1], &[vec![text("right").link("https://example.com")]], Style::PLAIN);
        let frame = canvas.finish();
        assert_eq!(string(&frame.lines[0]), "中文│right");
        assert_eq!(string(&frame.lines[1]), "    │     ");
        assert!(frame.lines[0].iter().any(|s| s.style.attrs.contains(Attrs::BOLD)));
        assert!(frame.lines[0].iter().any(|s| s.link.as_deref() == Some("https://example.com")));
        let mut canvas = Canvas::new(6, 1);
        canvas.line(canvas.area(), vec![text("中abcd")], Style::PLAIN);
        canvas.line(Rect::new(1, 0, 1, 1), vec![text("x")], Style::PLAIN);
        assert_eq!(string(&canvas.finish().lines[0]), " xabcd");
        let mut canvas = Canvas::new(3, 1);
        canvas.line(canvas.area(), vec![text("👩‍💻xx")], Style::PLAIN);
        let row = &canvas.finish().lines[0];
        assert_eq!(string(row), "👩‍💻x");
        assert_eq!(row.iter().map(|s| width(&s.text)).sum::<usize>(), 3);
    }
    #[test]
    fn image_and_cursor_offsets_obey_region_bounds() {
        let mut canvas = Canvas::new(12, 8);
        let area = Rect::new(4, 2, 5, 4);
        let image = ImageView::new(7, (800, 400)).fit(area.width, area.height).unwrap();
        assert!(canvas.image(area, image));
        canvas.cursor(area, 2, 1);
        assert!(!canvas.image(area, ImagePlace { image: 8, col: 4, row: 0, cols: 2, rows: 1 }));
        canvas.cursor(area, 5, 0);
        let frame = canvas.finish();
        assert_eq!(frame.cursor, Some((6, 3)));
        assert_eq!((frame.images[0].col, frame.images[0].row), (4, 2));
        for (pixels, expected) in [((800, 400), (40, 10)), ((400, 800), (20, 20)), ((16, 16), (2, 1))] {
            let image = ImageView::new(1, pixels).fit(40, 20).unwrap();
            assert_eq!((image.cols, image.rows), expected);
        }
        assert!(ImageView::new(1, (16, 16)).fit(0, 3).is_none());
    }
    #[test]
    fn bars_and_inputs_keep_metadata_and_cursor_in_bounds() {
        let row = bar(vec![text("directory").bold()], vec![text(" pinned").fg(Color::Indexed(4))], 12, Style::PLAIN);
        assert_eq!(string(&row), "direc pinned");
        let (row, cursor) = input(vec![text("/")], &TextInput::new("abcdefgh"), 5, Style::PLAIN);
        assert_eq!(string(&row), "/fgh ");
        assert_eq!(cursor, Some(4));
        let (row, cursor) = input(vec![text("/")], &TextInput::new("x"), 1, Style::PLAIN);
        assert_eq!(string(&row), "/");
        assert_eq!(cursor, None);
    }
    #[test]
    fn list_mouse_and_wheel_use_the_drawn_region() {
        let mut list = List { len: 10, ..List::default() };
        list.scroll(3, 4);
        assert_eq!((list.offset, list.selected), (3, 3));
        let region = Rect::new(8, 2, 6, 4);
        assert_eq!(list.at_point(region, 9, 3), Some(4));
        assert_eq!(list.at_point(region, 2, 3), None);
        let rows = list.render_rows(6, 4, Style::PLAIN, |i, _| vec![text(format!("row {i}"))]);
        assert_eq!(string(&rows[0]), "row 3 ");
        list.selected = 6;
        list.scroll(-3, 4);
        assert_eq!((list.offset, list.selected), (0, 3));
        let mut empty = List::default();
        empty.scroll(3, 2);
        assert_eq!(empty.render_rows(2, 2, Style::PLAIN, |_, _| unreachable!()).len(), 2);
    }
}
