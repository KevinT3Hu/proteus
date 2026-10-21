//! Tab layout: a binary split tree of panes, laid out in physical pixels.
//!
//! Pure geometry; the GUI owns the panes themselves and maps ids to them.

pub type PaneId = u64;

/// Direction a split divides: `X` puts children side by side, `Y` stacks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn axis(self) -> Axis {
        match self {
            Dir::Left | Dir::Right => Axis::X,
            Dir::Up | Dir::Down => Axis::Y,
        }
    }
    /// Towards the second child of a split.
    fn forward(self) -> bool {
        matches!(self, Dir::Right | Dir::Down)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    fn len(&self, axis: Axis) -> i32 {
        if axis == Axis::X { self.w } else { self.h }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(PaneId),
    Split { axis: Axis, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// Sizes that shape a layout, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    pub cell_w: i32,
    pub cell_h: i32,
    /// Space between two panes (the divider line sits in the middle).
    pub gap: i32,
}

impl Metrics {
    fn cell(&self, axis: Axis) -> i32 {
        if axis == Axis::X { self.cell_w } else { self.cell_h }
    }
    /// Smallest pane: 2 columns × 1 row.
    fn min(&self, axis: Axis) -> i32 {
        if axis == Axis::X { 2 * self.cell_w } else { self.cell_h }
    }
}

/// Result of laying out a tab.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    /// Grid area of every visible pane.
    pub panes: Vec<(PaneId, Rect)>,
    /// Gaps between split children (draw a line in the middle).
    pub dividers: Vec<Divider>,
}

/// The gap between the two children of a split.
#[derive(Debug, Clone, PartialEq)]
pub struct Divider {
    pub axis: Axis,
    pub gap: Rect,
    /// Area of the whole split (for turning a drag position into a ratio).
    pub parent: Rect,
    /// Route from the root to the split: `false` = first child.
    pub path: Vec<bool>,
}

impl Layout {
    /// Divider under a physical position; the hit area extends `slop`
    /// pixels beyond the gap.
    pub fn divider_at(&self, x: i32, y: i32, slop: i32) -> Option<&Divider> {
        self.dividers.iter().find(|d| {
            let g = d.gap;
            Rect::new(g.x - slop, g.y - slop, g.w + 2 * slop, g.h + 2 * slop).contains(x, y)
        })
    }
}

impl Layout {
    pub fn rect(&self, id: PaneId) -> Option<Rect> {
        self.panes.iter().find(|p| p.0 == id).map(|p| p.1)
    }
    pub fn pane_at(&self, x: i32, y: i32) -> Option<PaneId> {
        self.panes.iter().find(|p| p.1.contains(x, y)).map(|p| p.0)
    }

    /// Nearest pane from `from` in direction `dir`: it must lie beyond the
    /// edge and overlap on the other axis; closest first, then most overlap.
    pub fn neighbor(&self, from: PaneId, dir: Dir) -> Option<PaneId> {
        let r = self.rect(from)?;
        let mut best: Option<(i32, i32, PaneId)> = None;
        for &(id, o) in &self.panes {
            if id == from {
                continue;
            }
            let (dist, overlap) = match dir {
                Dir::Left => (r.x - o.right(), overlap(r.y, r.bottom(), o.y, o.bottom())),
                Dir::Right => (o.x - r.right(), overlap(r.y, r.bottom(), o.y, o.bottom())),
                Dir::Up => (r.y - o.bottom(), overlap(r.x, r.right(), o.x, o.right())),
                Dir::Down => (o.y - r.bottom(), overlap(r.x, r.right(), o.x, o.right())),
            };
            if dist < 0 || overlap <= 0 {
                continue;
            }
            if best.is_none_or(|(d, ov, _)| (dist, -overlap) < (d, -ov)) {
                best = Some((dist, overlap, id));
            }
        }
        best.map(|b| b.2)
    }
}

fn first_leaf(n: &Node) -> PaneId {
    match n {
        Node::Leaf(id) => *id,
        Node::Split { a, .. } => first_leaf(a),
    }
}

fn overlap(a0: i32, a1: i32, b0: i32, b1: i32) -> i32 {
    a1.min(b1) - a0.max(b0)
}

impl Node {
    pub fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// Size of the first child along `axis`, snapped to whole cells and
    /// clamped so both children keep their minimum size.
    fn split_at(axis: Axis, ratio: f32, rect: Rect, m: Metrics) -> i32 {
        let avail = rect.len(axis) - m.gap;
        let cell = m.cell(axis).max(1);
        let want = ((avail as f32 * ratio) / cell as f32).round() as i32 * cell;
        let max = (avail - m.min(axis)).max(0);
        want.clamp(m.min(axis).min(max), max)
    }

    fn children(axis: Axis, ratio: f32, rect: Rect, m: Metrics) -> (Rect, Rect, Rect) {
        let s = Self::split_at(axis, ratio, rect, m);
        match axis {
            Axis::X => (
                Rect::new(rect.x, rect.y, s, rect.h),
                Rect::new(rect.x + s, rect.y, m.gap, rect.h),
                Rect::new(rect.x + s + m.gap, rect.y, rect.w - s - m.gap, rect.h),
            ),
            Axis::Y => (
                Rect::new(rect.x, rect.y, rect.w, s),
                Rect::new(rect.x, rect.y + s, rect.w, m.gap),
                Rect::new(rect.x, rect.y + s + m.gap, rect.w, rect.h - s - m.gap),
            ),
        }
    }

    pub fn layout(&self, rect: Rect, m: Metrics, out: &mut Layout) {
        self.layout_at(rect, m, out, &mut Vec::new());
    }

    fn layout_at(&self, rect: Rect, m: Metrics, out: &mut Layout, path: &mut Vec<bool>) {
        match self {
            Node::Leaf(id) => out.panes.push((*id, rect)),
            Node::Split { axis, ratio, a, b } => {
                let (ra, gap, rb) = Self::children(*axis, *ratio, rect, m);
                path.push(false);
                a.layout_at(ra, m, out, path);
                path.pop();
                out.dividers.push(Divider { axis: *axis, gap, parent: rect, path: path.clone() });
                path.push(true);
                b.layout_at(rb, m, out, path);
                path.pop();
            }
        }
    }

    /// Set the ratio of the split at `path`.
    fn set_ratio(&mut self, path: &[bool], value: f32) -> bool {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => {
                *ratio = value.clamp(0.0, 1.0);
                true
            }
            (Node::Split { a, b, .. }, Some((&second, rest))) => {
                if second { b.set_ratio(rest, value) } else { a.set_ratio(rest, value) }
            }
            _ => false,
        }
    }

    /// Replace leaf `target` with a split holding it and `new` (second).
    fn split(&mut self, target: PaneId, new: PaneId, axis: Axis) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split { axis, ratio: 0.5, a: Box::new(Node::Leaf(target)), b: Box::new(Node::Leaf(new)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, axis) || b.split(target, new, axis),
        }
    }

    /// Remove leaf `target`; its sibling takes the parent's place. Returns
    /// false if not found. The root leaf itself cannot be removed here.
    fn remove(&mut self, target: PaneId) -> bool {
        let Node::Split { a, b, .. } = self else { return false };
        let keep = if **a == Node::Leaf(target) {
            Some(std::mem::replace(&mut **b, Node::Leaf(0)))
        } else if **b == Node::Leaf(target) {
            Some(std::mem::replace(&mut **a, Node::Leaf(0)))
        } else {
            None
        };
        match keep {
            Some(n) => {
                *self = n;
                true
            }
            None => a.remove(target) || b.remove(target),
        }
    }

    /// Move the border on the `dir` side of `target` by `cells` cells
    /// (positive = grow the pane). Uses the innermost split that has such a
    /// border.
    fn resize(&mut self, rect: Rect, m: Metrics, target: PaneId, dir: Dir, cells: i32) -> bool {
        let Node::Split { axis, ratio, a, b } = self else { return false };
        let (ra, _, rb) = Self::children(*axis, *ratio, rect, m);
        let in_a = a.contains(target);
        let done = if in_a { a.resize(ra, m, target, dir, cells) } else { b.resize(rb, m, target, dir, cells) };
        if done {
            return true;
        }
        if *axis != dir.axis() || in_a != dir.forward() {
            return false;
        }
        let avail = (rect.len(*axis) - m.gap).max(1) as f32;
        let delta = (cells * m.cell(*axis)) as f32 / avail;
        // Growing towards the second child moves the border forward.
        let signed = if in_a { delta } else { -delta };
        let old = Self::split_at(*axis, *ratio, rect, m);
        *ratio = (*ratio + signed).clamp(0.0, 1.0);
        // Snap so a step always moves at least one cell (or hits the limit).
        let new = Self::split_at(*axis, *ratio, rect, m);
        if new == old {
            *ratio = (old as f32 + signed.signum() * m.cell(*axis) as f32) / avail;
        }
        true
    }

    /// Equal shares for every pane in each run of same-axis splits.
    fn equalize(&mut self) {
        if let Node::Split { axis, ratio, a, b } = self {
            let (wa, wb) = (a.weight(*axis), b.weight(*axis));
            *ratio = wa as f32 / (wa + wb) as f32;
            a.equalize();
            b.equalize();
        }
    }

    fn weight(&self, axis: Axis) -> usize {
        match self {
            Node::Split { axis: x, a, b, .. } if *x == axis => a.weight(axis) + b.weight(axis),
            _ => 1,
        }
    }

    fn swap_ids(&mut self, p: PaneId, q: PaneId) {
        match self {
            Node::Leaf(id) if *id == p => *id = q,
            Node::Leaf(id) if *id == q => *id = p,
            Node::Leaf(_) => {}
            Node::Split { a, b, .. } => {
                a.swap_ids(p, q);
                b.swap_ids(p, q);
            }
        }
    }
}

/// One tab: a split tree, the focused pane and zoom state.
#[derive(Debug, Clone)]
pub struct Tab {
    pub root: Node,
    pub focus: PaneId,
    pub zoomed: bool,
    /// Set by the user; otherwise the tab shows its focused pane's title.
    pub name: Option<String>,
    /// Previously focused panes, most recent last (focus after a close).
    history: Vec<PaneId>,
}

impl Tab {
    pub fn new(pane: PaneId) -> Self {
        Self { root: Node::Leaf(pane), focus: pane, zoomed: false, name: None, history: Vec::new() }
    }

    /// A tab with a prepared tree; `focus` must be one of its panes.
    pub fn with_root(root: Node, focus: PaneId) -> Self {
        let focus = if root.contains(focus) { focus } else { first_leaf(&root) };
        Self { root, focus, zoomed: false, name: None, history: Vec::new() }
    }

    /// Drag a divider: put the split border at physical position `pos`
    /// (along the divider's axis).
    pub fn drag_divider(&mut self, d: &Divider, pos: i32, m: Metrics) -> bool {
        let start = if d.axis == Axis::X { d.parent.x } else { d.parent.y };
        let avail = (d.parent.len(d.axis) - m.gap).max(1) as f32;
        let ratio = ((pos - start) as f32 - m.gap as f32 / 2.0) / avail;
        self.root.set_ratio(&d.path, ratio)
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        self.root.leaves(&mut v);
        v
    }

    /// Layout of all panes, ignoring zoom.
    pub fn full_layout(&self, area: Rect, m: Metrics) -> Layout {
        let mut l = Layout::default();
        self.root.layout(area, m, &mut l);
        l
    }

    /// What is shown: the zoomed pane alone, or the full layout.
    pub fn layout(&self, area: Rect, m: Metrics) -> Layout {
        if self.zoomed {
            Layout { panes: vec![(self.focus, area)], dividers: Vec::new() }
        } else {
            self.full_layout(area, m)
        }
    }

    pub fn set_focus(&mut self, id: PaneId) {
        if id != self.focus && self.root.contains(id) {
            self.history.retain(|&p| p != self.focus && p != id);
            self.history.push(self.focus);
            self.focus = id;
            self.zoomed = false;
        }
    }

    /// Whether the focused pane (at `rect`) is large enough to split.
    pub fn can_split(rect: Rect, axis: Axis, m: Metrics) -> bool {
        rect.len(axis) >= 2 * m.min(axis) + m.gap
    }

    /// Split the focused pane; the new pane is focused.
    pub fn split(&mut self, new: PaneId, axis: Axis) {
        let focus = self.focus;
        if self.root.split(focus, new, axis) {
            self.zoomed = false;
            self.set_focus(new);
        }
    }

    /// Remove a pane. Returns false if it was the last one (the tab should
    /// close) or not in this tab.
    pub fn remove(&mut self, id: PaneId) -> bool {
        if self.root == Node::Leaf(id) || !self.root.remove(id) {
            return false;
        }
        self.history.retain(|&p| p != id);
        if self.focus == id {
            self.zoomed = false;
            self.focus = match self.history.pop() {
                Some(p) => p,
                None => self.panes()[0],
            };
        }
        true
    }

    pub fn focus_dir(&mut self, area: Rect, m: Metrics, dir: Dir) -> bool {
        match self.full_layout(area, m).neighbor(self.focus, dir) {
            Some(id) => {
                self.set_focus(id);
                true
            }
            None => false,
        }
    }

    /// Grow (`cells > 0`) or shrink the focused pane on its `dir` side.
    pub fn resize(&mut self, area: Rect, m: Metrics, dir: Dir, cells: i32) -> bool {
        let focus = self.focus;
        !self.zoomed && self.root.resize(area, m, focus, dir, cells)
    }

    pub fn equalize(&mut self) {
        self.root.equalize();
    }

    /// Swap the focused pane with its neighbor in `dir`.
    pub fn swap(&mut self, area: Rect, m: Metrics, dir: Dir) -> bool {
        match self.full_layout(area, m).neighbor(self.focus, dir) {
            Some(other) => {
                self.root.swap_ids(self.focus, other);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { cell_w: 10, cell_h: 20, gap: 6 };
    const AREA: Rect = Rect { x: 0, y: 0, w: 806, h: 406 };

    #[test]
    fn split_layout_snaps_to_cells() {
        let mut t = Tab::new(1);
        t.split(2, Axis::X);
        assert_eq!(t.focus, 2);
        let l = t.layout(AREA, M);
        assert_eq!(l.rect(1), Some(Rect::new(0, 0, 400, 406)));
        assert_eq!(l.rect(2), Some(Rect::new(406, 0, 400, 406)));
        assert_eq!(l.dividers.len(), 1);
        assert_eq!((l.dividers[0].axis, l.dividers[0].gap), (Axis::X, Rect::new(400, 0, 6, 406)));
        assert_eq!(l.divider_at(398, 10, 3).map(|d| d.path.clone()), Some(vec![]));
        t.split(3, Axis::Y);
        let l = t.layout(AREA, M);
        assert_eq!(l.dividers[1].path, vec![true]);
        assert_eq!(l.rect(2), Some(Rect::new(406, 0, 400, 200)));
        assert_eq!(l.rect(3), Some(Rect::new(406, 206, 400, 200)));
        assert_eq!(l.pane_at(500, 300), Some(3));
        assert_eq!(l.pane_at(403, 300), None);
    }

    #[test]
    fn neighbors_and_focus_history() {
        let mut t = Tab::new(1);
        t.split(2, Axis::X);
        t.split(3, Axis::Y);
        assert!(t.focus_dir(AREA, M, Dir::Left));
        assert_eq!(t.focus, 1);
        assert!(!t.focus_dir(AREA, M, Dir::Left));
        assert!(t.focus_dir(AREA, M, Dir::Right));
        assert_eq!(t.focus, 2); // closest, then most overlap: both overlap; top wins on tie
        t.set_focus(3);
        assert!(t.focus_dir(AREA, M, Dir::Up));
        assert_eq!(t.focus, 2);
        t.set_focus(3);
        // Closing the focused pane returns to the previous one.
        assert!(t.remove(3));
        assert_eq!(t.focus, 2);
        assert_eq!(t.panes(), vec![1, 2]);
        assert!(t.remove(2));
        assert_eq!(t.root, Node::Leaf(1));
        assert!(!t.remove(1));
    }

    #[test]
    fn resize_equalize_swap_zoom() {
        let mut t = Tab::new(1);
        t.split(2, Axis::X);
        // Pane 2 is on the right: growing left moves the border left by 3 cells.
        assert!(t.resize(AREA, M, Dir::Left, 3));
        assert_eq!(t.layout(AREA, M).rect(2).unwrap().w, 430);
        assert!(!t.resize(AREA, M, Dir::Right, 3)); // window edge
        assert!(t.resize(AREA, M, Dir::Left, -1));
        assert_eq!(t.layout(AREA, M).rect(2).unwrap().w, 420);
        // Clamped to the minimum width of the other pane.
        assert!(t.resize(AREA, M, Dir::Left, 1000));
        assert_eq!(t.layout(AREA, M).rect(1).unwrap().w, 20);
        t.split(3, Axis::X);
        t.equalize();
        let l = t.layout(AREA, M);
        let w: Vec<i32> = l.panes.iter().map(|p| p.1.w).collect();
        assert!(w.iter().all(|&x| (x - w[0]).abs() <= 10), "{w:?}");
        assert!(t.swap(AREA, M, Dir::Left));
        assert_eq!(t.panes(), vec![1, 3, 2]);
        assert_eq!(t.focus, 3);
        t.zoomed = true;
        assert_eq!(t.layout(AREA, M).panes, vec![(3, AREA)]);
        assert!(!t.resize(AREA, M, Dir::Left, 1));
    }

    #[test]
    fn drag() {
        let mut t = Tab::new(1);
        t.split(2, Axis::X);
        let d = t.layout(AREA, M).dividers[0].clone();
        // Border (middle of the gap) dragged to x=203: left pane 200 wide.
        assert!(t.drag_divider(&d, 203, M));
        assert_eq!(t.layout(AREA, M).rect(1).unwrap().w, 200);
        assert!(t.drag_divider(&d, -50, M));
        assert_eq!(t.layout(AREA, M).rect(1).unwrap().w, 20);
    }

    #[test]
    fn split_limits() {
        assert!(Tab::can_split(Rect::new(0, 0, 46, 20), Axis::X, M));
        assert!(!Tab::can_split(Rect::new(0, 0, 45, 20), Axis::X, M));
        assert!(!Tab::can_split(Rect::new(0, 0, 400, 45), Axis::Y, M));
    }
}
