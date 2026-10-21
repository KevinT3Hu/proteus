//! Tab layout: a binary split tree of panes (whose nodes may be stacks),
//! plus a layer of floating panes, laid out in physical pixels.
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
    fn delta(self) -> (i32, i32) {
        match self {
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
        }
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
    /// Panes sharing one area: `active` is expanded, the others show as
    /// one-row title bars above and below it.
    Stack { panes: Vec<PaneId>, active: usize },
}

/// Sizes that shape a layout, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    pub cell_w: i32,
    pub cell_h: i32,
    /// Space between two panes (the divider line sits in the middle).
    pub gap: i32,
    /// Every tiled pane gets a title row above its grid.
    pub titles: bool,
}

impl Metrics {
    fn cell(&self, axis: Axis) -> i32 {
        if axis == Axis::X { self.cell_w } else { self.cell_h }
    }
    /// Smallest pane: 2 columns × 1 row (plus its title row).
    fn min(&self, axis: Axis) -> i32 {
        match axis {
            Axis::X => 2 * self.cell_w,
            Axis::Y if self.titles => 2 * self.cell_h,
            Axis::Y => self.cell_h,
        }
    }
    /// Border width of floating panes.
    pub fn float_border(&self) -> i32 {
        (self.cell_w / 2).max(2)
    }
}

/// A pane's title row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Title {
    pub id: PaneId,
    pub rect: Rect,
    /// A stacked pane that is not expanded: the title is all that shows.
    pub collapsed: bool,
}

/// A floating pane as laid out: frame, title row and grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatFrame {
    pub id: PaneId,
    pub outer: Rect,
    pub title: Rect,
    pub content: Rect,
}

impl FloatFrame {
    /// Bottom-right handle for resizing with the mouse.
    pub fn corner(&self, m: Metrics) -> Rect {
        let s = m.cell_w.max(m.float_border() * 2);
        Rect::new(self.outer.right() - s, self.outer.bottom() - s, s, s)
    }
}

/// What a position hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Content,
    /// Title row of a tiled or stacked pane.
    Title,
    /// Title row (or border) of a floating pane: drag to move.
    FloatFrame,
    /// Resize handle of a floating pane.
    FloatCorner,
}

/// Result of laying out a tab.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    /// Grid area of every visible tiled pane.
    pub panes: Vec<(PaneId, Rect)>,
    pub titles: Vec<Title>,
    /// Gaps between split children (draw a line in the middle).
    pub dividers: Vec<Divider>,
    /// Visible floating panes, bottom to top.
    pub floats: Vec<FloatFrame>,
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
        if self.floats.iter().any(|f| f.outer.contains(x, y)) {
            return None;
        }
        self.dividers.iter().find(|d| {
            let g = d.gap;
            Rect::new(g.x - slop, g.y - slop, g.w + 2 * slop, g.h + 2 * slop).contains(x, y)
        })
    }

    /// Grid area of a visible pane (tiled or floating).
    pub fn rect(&self, id: PaneId) -> Option<Rect> {
        self.floats
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.content)
            .or_else(|| self.panes.iter().find(|p| p.0 == id).map(|p| p.1))
    }

    /// Topmost thing under a position.
    pub fn hit(&self, x: i32, y: i32, m: Metrics) -> Option<(PaneId, Part)> {
        for f in self.floats.iter().rev() {
            if !f.outer.contains(x, y) {
                continue;
            }
            let part = if f.corner(m).contains(x, y) {
                Part::FloatCorner
            } else if f.content.contains(x, y) {
                Part::Content
            } else {
                Part::FloatFrame
            };
            return Some((f.id, part));
        }
        if let Some(p) = self.panes.iter().find(|p| p.1.contains(x, y)) {
            return Some((p.0, Part::Content));
        }
        self.titles.iter().find(|t| t.rect.contains(x, y)).map(|t| (t.id, Part::Title))
    }

    #[cfg(test)]
    pub fn pane_at(&self, x: i32, y: i32) -> Option<PaneId> {
        let m = Metrics { cell_w: 1, cell_h: 1, gap: 0, titles: false };
        self.hit(x, y, m).map(|h| h.0)
    }

    /// Nearest pane from `from` in direction `dir` among `cands`: it must
    /// lie beyond the edge and overlap on the other axis; closest first,
    /// then most overlap.
    fn nearest(r: Rect, from: PaneId, cands: impl Iterator<Item = (PaneId, Rect)>, dir: Dir) -> Option<PaneId> {
        let mut best: Option<(i32, i32, PaneId)> = None;
        for (id, o) in cands {
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

    /// Neighbor among tiled panes (collapsed stack titles count as panes).
    pub fn neighbor(&self, from: PaneId, dir: Dir) -> Option<PaneId> {
        let r = self.panes.iter().find(|p| p.0 == from)?.1;
        let collapsed = self.titles.iter().filter(|t| t.collapsed).map(|t| (t.id, t.rect));
        Self::nearest(r, from, self.panes.iter().copied().chain(collapsed), dir)
    }

    /// Neighbor among floating panes, by their frames.
    pub fn float_neighbor(&self, from: PaneId, dir: Dir) -> Option<PaneId> {
        let r = self.floats.iter().find(|f| f.id == from)?.outer;
        let cands = self.floats.iter().map(|f| (f.id, f.outer));
        // Floats may overlap: measure from the centers instead of the edges.
        let c = |r: Rect| (r.x + r.w / 2, r.y + r.h / 2);
        let (cx, cy) = c(r);
        let mut best: Option<(i32, PaneId)> = None;
        for (id, o) in cands.filter(|f| f.0 != from) {
            let (ox, oy) = c(o);
            let (dx, dy) = (ox - cx, oy - cy);
            let (along, across) = match dir {
                Dir::Left => (-dx, dy.abs()),
                Dir::Right => (dx, dy.abs()),
                Dir::Up => (-dy, dx.abs()),
                Dir::Down => (dy, dx.abs()),
            };
            if along <= 0 {
                continue;
            }
            let score = along + 2 * across;
            if best.is_none_or(|(s, _)| score < s) {
                best = Some((score, id));
            }
        }
        best.map(|b| b.1)
    }
}

fn first_leaf(n: &Node) -> PaneId {
    match n {
        Node::Leaf(id) => *id,
        Node::Split { a, .. } => first_leaf(a),
        Node::Stack { panes, active } => panes[*active],
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
            Node::Stack { panes, .. } => out.extend(panes),
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
            Node::Stack { panes, .. } => panes.contains(&id),
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
        let ch = m.cell_h;
        match self {
            Node::Leaf(id) if m.titles && rect.h >= 2 * ch => {
                out.titles.push(Title { id: *id, rect: Rect::new(rect.x, rect.y, rect.w, ch), collapsed: false });
                out.panes.push((*id, Rect::new(rect.x, rect.y + ch, rect.w, rect.h - ch)));
            }
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
            Node::Stack { panes, active } => {
                // Titles above the expanded pane, its own title, its grid,
                // then titles below; at least one row of grid survives.
                let rows = (rect.h / ch).max(1);
                let below = panes.len() - 1 - active;
                let fit = |n: usize, room: i32| (n as i32).min(room.max(0)) as usize;
                let n_below = fit(below, rows - 2);
                let n_above = fit(*active, rows - 2 - n_below as i32);
                let own = rows - n_above as i32 - n_below as i32 >= 2;
                let mut y = rect.y;
                for &id in &panes[active - n_above..*active] {
                    out.titles.push(Title { id, rect: Rect::new(rect.x, y, rect.w, ch), collapsed: true });
                    y += ch;
                }
                let id = panes[*active];
                if own {
                    out.titles.push(Title { id, rect: Rect::new(rect.x, y, rect.w, ch), collapsed: false });
                    y += ch;
                }
                let bottom = rect.bottom() - n_below as i32 * ch;
                out.panes.push((id, Rect::new(rect.x, y, rect.w, (bottom - y).max(ch.min(rect.h)))));
                for (i, &id) in panes[active + 1..active + 1 + n_below].iter().enumerate() {
                    out.titles.push(Title { id, rect: Rect::new(rect.x, bottom + i as i32 * ch, rect.w, ch), collapsed: true });
                }
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
    /// A stacked target splits the whole stack.
    fn split(&mut self, target: PaneId, new: PaneId, axis: Axis) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {}
            Node::Stack { panes, .. } if panes.contains(&target) => {}
            Node::Leaf(_) | Node::Stack { .. } => return false,
            Node::Split { a, b, .. } => return a.split(target, new, axis) || b.split(target, new, axis),
        }
        let old = std::mem::replace(self, Node::Leaf(0));
        *self = Node::Split { axis, ratio: 0.5, a: Box::new(old), b: Box::new(Node::Leaf(new)) };
        true
    }

    /// Remove leaf `target`; a split's other child takes its place, a
    /// stack of two becomes a leaf. The root leaf itself cannot be removed
    /// here.
    fn remove(&mut self, target: PaneId) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Stack { panes, active } => {
                let Some(i) = panes.iter().position(|&p| p == target) else { return false };
                if panes.len() == 1 {
                    return false;
                }
                panes.remove(i);
                if i < *active || *active >= panes.len() {
                    *active = active.saturating_sub(1);
                }
                if panes.len() == 1 {
                    *self = Node::Leaf(panes[0]);
                }
                true
            }
            Node::Split { a, b, .. } => {
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
        }
    }

    /// Expand `id` in the stack that holds it.
    fn activate(&mut self, id: PaneId) {
        match self {
            Node::Stack { panes, active } => {
                if let Some(i) = panes.iter().position(|&p| p == id) {
                    *active = i;
                }
            }
            Node::Split { a, b, .. } => {
                a.activate(id);
                b.activate(id);
            }
            Node::Leaf(_) => {}
        }
    }

    /// Stack `target` with its sibling (all panes of the sibling's
    /// subtree), or unstack the stack holding it into stacked rows.
    fn toggle_stack(&mut self, target: PaneId) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Stack { panes, .. } if panes.contains(&target) => {
                let panes = std::mem::take(panes);
                let n = panes.len();
                let mut node = Node::Leaf(panes[n - 1]);
                for (i, &id) in panes[..n - 1].iter().enumerate().rev() {
                    node = Node::Split {
                        axis: Axis::Y,
                        ratio: 1.0 / (n - i) as f32,
                        a: Box::new(Node::Leaf(id)),
                        b: Box::new(node),
                    };
                }
                *self = node;
                true
            }
            Node::Stack { .. } => false,
            Node::Split { a, b, .. } => {
                let direct = |n: &Node| match n {
                    Node::Leaf(id) => *id == target,
                    Node::Stack { panes, .. } => panes.contains(&target),
                    Node::Split { .. } => false,
                };
                if direct(a) || direct(b) {
                    let mut panes = Vec::new();
                    self.leaves(&mut panes);
                    let active = panes.iter().position(|&p| p == target).unwrap_or(0);
                    *self = Node::Stack { panes, active };
                    return true;
                }
                a.toggle_stack(target) || b.toggle_stack(target)
            }
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
        let map = |id: &mut PaneId| {
            if *id == p {
                *id = q
            } else if *id == q {
                *id = p
            }
        };
        match self {
            Node::Leaf(id) => map(id),
            Node::Stack { panes, .. } => panes.iter_mut().for_each(map),
            Node::Split { a, b, .. } => {
                a.swap_ids(p, q);
                b.swap_ids(p, q);
            }
        }
    }
}

/// A floating pane: its grid's position and size in cells, relative to
/// the tab area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Float {
    pub id: PaneId,
    pub col: i32,
    pub row: i32,
    pub cols: i32,
    pub rows: i32,
}

/// One tab: a split tree, floating panes, the focused pane and zoom state.
#[derive(Debug, Clone)]
pub struct Tab {
    pub root: Node,
    /// Floating panes, bottom to top.
    pub floats: Vec<Float>,
    pub floats_visible: bool,
    pub focus: PaneId,
    /// The tiled pane focused last (focus returns here from the floats).
    pub last_tiled: PaneId,
    pub zoomed: bool,
    /// Set by the user; otherwise the tab shows its focused pane's title.
    pub name: Option<String>,
    /// Previously focused panes, most recent last (focus after a close).
    history: Vec<PaneId>,
}

impl Tab {
    pub fn new(pane: PaneId) -> Self {
        Self::with_root(Node::Leaf(pane), pane)
    }

    /// A tab with a prepared tree; `focus` should be one of its panes.
    pub fn with_root(root: Node, focus: PaneId) -> Self {
        let focus = if root.contains(focus) { focus } else { first_leaf(&root) };
        let mut t = Self {
            root,
            floats: Vec::new(),
            floats_visible: false,
            focus,
            last_tiled: focus,
            zoomed: false,
            name: None,
            history: Vec::new(),
        };
        t.root.activate(focus);
        t
    }

    /// Drag a divider: put the split border at physical position `pos`
    /// (along the divider's axis).
    pub fn drag_divider(&mut self, d: &Divider, pos: i32, m: Metrics) -> bool {
        let start = if d.axis == Axis::X { d.parent.x } else { d.parent.y };
        let avail = (d.parent.len(d.axis) - m.gap).max(1) as f32;
        let ratio = ((pos - start) as f32 - m.gap as f32 / 2.0) / avail;
        self.root.set_ratio(&d.path, ratio)
    }

    /// All panes: tiled in tree order, then floating.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut v = self.tiled();
        v.extend(self.floats.iter().map(|f| f.id));
        v
    }

    pub fn tiled(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        self.root.leaves(&mut v);
        v
    }

    pub fn contains(&self, id: PaneId) -> bool {
        self.root.contains(id) || self.is_float(id)
    }

    pub fn is_float(&self, id: PaneId) -> bool {
        self.floats.iter().any(|f| f.id == id)
    }

    pub fn focus_is_float(&self) -> bool {
        self.is_float(self.focus)
    }

    /// Layout of all tiled panes, ignoring zoom and floats.
    pub fn full_layout(&self, area: Rect, m: Metrics) -> Layout {
        let mut l = Layout::default();
        self.root.layout(area, m, &mut l);
        l
    }

    /// What is shown: the zoomed pane alone or the tiled layout, and the
    /// floating panes over it when visible.
    pub fn layout(&self, area: Rect, m: Metrics) -> Layout {
        let mut l = if self.zoomed {
            Layout { panes: vec![(self.last_tiled, area)], ..Layout::default() }
        } else {
            self.full_layout(area, m)
        };
        if self.floats_visible {
            l.floats = self.floats.iter().map(|f| float_frame(f, area, m)).collect();
        }
        l
    }

    pub fn set_focus(&mut self, id: PaneId) {
        if !self.contains(id) {
            return;
        }
        if id != self.focus {
            self.history.retain(|&p| p != self.focus && p != id);
            self.history.push(self.focus);
            self.focus = id;
        }
        if let Some(i) = self.floats.iter().position(|f| f.id == id) {
            // Focused float goes on top.
            let f = self.floats.remove(i);
            self.floats.push(f);
            self.floats_visible = true;
        } else {
            if id != self.last_tiled {
                self.zoomed = false;
            }
            self.last_tiled = id;
            self.root.activate(id);
        }
    }

    /// Whether the focused pane (at `rect`) is large enough to split.
    pub fn can_split(rect: Rect, axis: Axis, m: Metrics) -> bool {
        rect.len(axis) >= 2 * m.min(axis) + m.gap
    }

    /// Split the last focused tiled pane; the new pane is focused.
    pub fn split(&mut self, new: PaneId, axis: Axis) {
        let target = self.last_tiled;
        if self.root.split(target, new, axis) {
            self.zoomed = false;
            self.set_focus(new);
        }
    }

    /// Default place for a new float: centered, 70% of the area, offset a
    /// little per existing float.
    pub fn default_float(&self, id: PaneId, area: Rect, m: Metrics) -> Float {
        let (cols, rows) = (area.w / m.cell_w, area.h / m.cell_h);
        let (w, h) = ((cols * 7 / 10).max(2), (rows * 7 / 10).max(1));
        let n = self.floats.len() as i32;
        Float { id, col: (cols - w) / 2 + 2 * n, row: (rows - h) / 2 + n, cols: w, rows: h }
    }

    pub fn add_float(&mut self, f: Float) {
        self.floats.push(f);
        self.floats_visible = true;
        self.set_focus(f.id);
    }

    /// Show or hide the floating panes; focus follows.
    pub fn toggle_floats(&mut self) {
        if self.floats.is_empty() {
            return;
        }
        if self.floats_visible {
            self.floats_visible = false;
            let t = self.last_tiled;
            self.set_focus(t);
        } else if let Some(top) = self.floats.last().map(|f| f.id) {
            self.set_focus(top);
        }
    }

    /// Float the focused tiled pane, or embed the focused float next to
    /// the last tiled pane. Returns false if nothing changed.
    pub fn toggle_float(&mut self, area: Rect, m: Metrics) -> bool {
        let id = self.focus;
        if let Some(i) = self.floats.iter().position(|f| f.id == id) {
            let f = self.floats.remove(i);
            let target = self.full_layout(area, m).rect(self.last_tiled);
            let axis = match target {
                Some(r) if r.w >= 2 * r.h => Axis::X,
                _ => Axis::Y,
            };
            let last = self.last_tiled;
            self.root.split(last, f.id, axis);
            if self.floats.is_empty() {
                self.floats_visible = false;
            }
            self.set_focus(f.id);
            return true;
        }
        if self.root == Node::Leaf(id) || !self.root.remove(id) {
            return false;
        }
        self.zoomed = false;
        self.last_tiled = self.history.iter().rev().copied().find(|&p| self.root.contains(p)).unwrap_or(first_leaf(&self.root));
        self.root.activate(self.last_tiled);
        let f = self.default_float(id, area, m);
        self.floats.push(f);
        self.floats_visible = true;
        true
    }

    /// Remove a pane. Returns false if it was the last one (the tab should
    /// close) or not in this tab.
    pub fn remove(&mut self, id: PaneId) -> bool {
        if let Some(i) = self.floats.iter().position(|f| f.id == id) {
            self.floats.remove(i);
            if self.floats.is_empty() {
                self.floats_visible = false;
            }
        } else if self.root == Node::Leaf(id) {
            // The last tiled pane: a float takes its place, if any.
            if self.floats.is_empty() {
                return false;
            }
            let f = self.floats.remove(0);
            self.root = Node::Leaf(f.id);
            if self.floats.is_empty() {
                self.floats_visible = false;
            }
        } else if !self.root.remove(id) {
            return false;
        }
        let (root, floats) = (&self.root, &self.floats);
        self.history.retain(|&p| p != id && (root.contains(p) || floats.iter().any(|f| f.id == p)));
        if !self.root.contains(self.last_tiled) {
            self.zoomed = false;
            self.last_tiled = self.history.iter().rev().copied().find(|&p| self.root.contains(p)).unwrap_or(first_leaf(&self.root));
            self.root.activate(self.last_tiled);
        }
        if self.focus == id {
            let next = match self.history.pop() {
                Some(p) if self.is_float(p) && !self.floats_visible => self.last_tiled,
                Some(p) => p,
                None => self.last_tiled,
            };
            self.focus = next;
            self.set_focus(next);
        }
        true
    }

    pub fn focus_dir(&mut self, area: Rect, m: Metrics, dir: Dir) -> bool {
        let next = if self.focus_is_float() {
            self.layout(area, m).float_neighbor(self.focus, dir)
        } else {
            self.full_layout(area, m).neighbor(self.focus, dir)
        };
        match next {
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
        if let Some(f) = self.floats.iter_mut().find(|f| f.id == focus) {
            let (dx, dy) = dir.delta();
            let before = *f;
            // Growing left/up moves the origin along with the edge.
            if dx < 0 {
                f.col -= cells;
            }
            if dy < 0 {
                f.row -= cells;
            }
            if dx != 0 {
                f.cols = (f.cols + cells).max(2);
            }
            if dy != 0 {
                f.rows = (f.rows + cells).max(1);
            }
            clamp_float(f, area, m);
            return *f != before;
        }
        !self.zoomed && self.root.resize(area, m, focus, dir, cells)
    }

    pub fn equalize(&mut self) {
        self.root.equalize();
    }

    /// Swap the focused pane with its neighbor in `dir`; a float moves by
    /// `step` cells instead.
    pub fn swap(&mut self, area: Rect, m: Metrics, dir: Dir, step: i32) -> bool {
        let focus = self.focus;
        if let Some(f) = self.floats.iter_mut().find(|f| f.id == focus) {
            let (dx, dy) = dir.delta();
            let before = *f;
            f.col += dx * step * 2;
            f.row += dy * step;
            clamp_float(f, area, m);
            return *f != before;
        }
        match self.full_layout(area, m).neighbor(self.focus, dir) {
            Some(other) => {
                self.root.swap_ids(self.focus, other);
                true
            }
            None => false,
        }
    }

    /// Stack the focused tiled pane with its sibling, or unstack it.
    pub fn toggle_stack(&mut self) -> bool {
        let id = self.last_tiled;
        let done = self.root.toggle_stack(id);
        if done {
            self.root.activate(id);
            self.zoomed = false;
        }
        done
    }

    /// Place a float's grid at a physical position / size (mouse drags).
    #[allow(clippy::too_many_arguments)]
    pub fn set_float_rect(&mut self, id: PaneId, col: i32, row: i32, cols: i32, rows: i32, area: Rect, m: Metrics) -> bool {
        let Some(f) = self.floats.iter_mut().find(|f| f.id == id) else { return false };
        let before = *f;
        *f = Float { id, col, row, cols: cols.max(2), rows: rows.max(1) };
        clamp_float(f, area, m);
        *f != before
    }
}

/// Keep a float's frame inside the area.
fn clamp_float(f: &mut Float, area: Rect, m: Metrics) {
    let bw = m.float_border();
    let max_cols = ((area.w - 2 * bw) / m.cell_w).max(2);
    let max_rows = ((area.h - m.cell_h - bw) / m.cell_h).max(1);
    f.cols = f.cols.clamp(2, max_cols);
    f.rows = f.rows.clamp(1, max_rows);
    // Positions in cells; the frame needs a border column and a title row.
    let min_col = (bw + m.cell_w - 1) / m.cell_w;
    let max_col = ((area.w - bw) / m.cell_w - f.cols).max(min_col);
    f.col = f.col.clamp(min_col, max_col);
    f.row = f.row.clamp(1, ((area.h - bw) / m.cell_h - f.rows).max(1));
}

fn float_frame(f: &Float, area: Rect, m: Metrics) -> FloatFrame {
    let mut f = *f;
    clamp_float(&mut f, area, m);
    let bw = m.float_border();
    let content = Rect::new(area.x + f.col * m.cell_w, area.y + f.row * m.cell_h, f.cols * m.cell_w, f.rows * m.cell_h);
    let outer = Rect::new(content.x - bw, content.y - m.cell_h, content.w + 2 * bw, content.h + m.cell_h + bw);
    let title = Rect::new(content.x, content.y - m.cell_h, content.w, m.cell_h);
    FloatFrame { id: f.id, outer, title, content }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { cell_w: 10, cell_h: 20, gap: 6, titles: false };
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
        assert_eq!(t.focus, 2); // both overlap; top wins on tie
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
        assert!(t.swap(AREA, M, Dir::Left, 1));
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
        let titled = Metrics { titles: true, ..M };
        assert!(!Tab::can_split(Rect::new(0, 0, 400, 85), Axis::Y, titled));
        assert!(Tab::can_split(Rect::new(0, 0, 400, 86), Axis::Y, titled));
    }

    #[test]
    fn titles() {
        let mut t = Tab::new(1);
        t.split(2, Axis::X);
        let l = t.layout(AREA, Metrics { titles: true, ..M });
        assert_eq!(l.titles[0], Title { id: 1, rect: Rect::new(0, 0, 400, 20), collapsed: false });
        assert_eq!(l.rect(1), Some(Rect::new(0, 20, 400, 386)));
        assert_eq!(l.hit(10, 10, M), Some((1, Part::Title)));
    }

    #[test]
    fn stacks() {
        let mut t = Tab::new(1);
        t.split(2, Axis::X);
        t.split(3, Axis::Y);
        t.split(4, Axis::Y);
        // 2, 3 and 4 share the right column; stack 4 with its sibling 3.
        assert!(t.toggle_stack());
        assert_eq!(t.root, Node::Split {
            axis: Axis::X,
            ratio: 0.5,
            a: Box::new(Node::Leaf(1)),
            b: Box::new(Node::Split {
                axis: Axis::Y,
                ratio: 0.5,
                a: Box::new(Node::Leaf(2)),
                b: Box::new(Node::Stack { panes: vec![3, 4], active: 1 }),
            }),
        });
        let l = t.layout(AREA, M);
        // Collapsed 3 above, then 4's title and grid.
        let right: Vec<_> = l.titles.iter().map(|x| (x.id, x.rect.y, x.collapsed)).collect();
        assert_eq!(right, [(3, 206, true), (4, 226, false)]);
        assert_eq!(l.rect(4), Some(Rect::new(406, 246, 400, 160)));
        assert_eq!(l.rect(3), None);
        // Up from 4 reaches the collapsed 3, which then expands.
        assert!(t.focus_dir(AREA, M, Dir::Up));
        assert_eq!(t.focus, 3);
        assert_eq!(t.layout(AREA, M).rect(3).map(|r| r.y), Some(226));
        assert!(t.focus_dir(AREA, M, Dir::Up));
        assert_eq!(t.focus, 2);
        // Splitting a stacked pane splits the stack; closing leaves a leaf.
        t.set_focus(3);
        assert!(t.remove(3));
        assert!(matches!(&t.root, Node::Split { b, .. } if matches!(&**b, Node::Split { b, .. } if **b == Node::Leaf(4))));
        // Unstack restores rows.
        t.set_focus(2);
        assert!(t.toggle_stack());
        assert!(t.toggle_stack());
        let mut ids = t.tiled();
        ids.sort();
        assert_eq!(ids, [1, 2, 4]);
    }

    #[test]
    fn floats() {
        let mut t = Tab::new(1);
        let f = t.default_float(2, AREA, M);
        assert_eq!((f.cols, f.rows), (56, 14));
        t.add_float(f);
        assert!(t.focus_is_float() && t.floats_visible);
        assert_eq!(t.panes(), [1, 2]);
        let l = t.layout(AREA, M);
        let fr = l.floats[0];
        assert_eq!(fr.content, Rect::new(120, 60, 560, 280));
        assert_eq!(fr.outer, Rect::new(115, 40, 570, 305));
        assert_eq!(l.hit(130, 50, M), Some((2, Part::FloatFrame)));
        assert_eq!(l.hit(683, 343, M), Some((2, Part::FloatCorner)));
        assert_eq!(l.hit(300, 200, M), Some((2, Part::Content)));
        assert_eq!(l.hit(5, 5, M), Some((1, Part::Content)));
        // Move and resize stay inside the area.
        assert!(t.swap(AREA, M, Dir::Left, 100));
        assert_eq!(t.floats[0].col, 1);
        assert!(t.resize(AREA, M, Dir::Right, 1000));
        assert_eq!(t.floats[0].cols, 79);
        // Hide: focus returns to the tiled pane; show: back on the float.
        t.toggle_floats();
        assert_eq!((t.focus, t.layout(AREA, M).floats.len()), (1, 0));
        t.toggle_floats();
        assert_eq!(t.focus, 2);
        // Embed the float, float it again.
        assert!(t.toggle_float(AREA, M));
        assert!(t.floats.is_empty() && t.root.contains(2) && t.focus == 2);
        assert!(t.toggle_float(AREA, M));
        assert!(t.is_float(2) && t.root == Node::Leaf(1) && t.last_tiled == 1);
        // Closing the only tiled pane promotes a float.
        assert!(t.remove(1));
        assert_eq!((t.root.clone(), t.focus, t.floats.len()), (Node::Leaf(2), 2, 0));
        assert!(!t.remove(2));
    }
}
