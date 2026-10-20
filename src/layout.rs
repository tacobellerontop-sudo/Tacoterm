//! Split panes: a binary tree of panes, each split either side by side or
//! stacked, and the geometry that falls out of it.
//!
//! The tree only knows pane ids; the app owns the terminals behind them.
//! Nothing here touches a window, so layout, focus movement and resizing are
//! all testable on any platform.

/// Identifies a pane. The app hands these out; the layout never invents one.
pub type PaneId = u64;

/// How a pane is split in two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    /// Side by side; the new pane goes on the right.
    Right,
    /// Stacked; the new pane goes below.
    Down,
}

/// A direction to move focus or resize toward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// An axis-aligned rectangle in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    /// Whether the point is inside (left/top edges inclusive, right/bottom
    /// exclusive, so adjacent rects never both claim a point).
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Round the edges to whole pixels.
    fn snapped(self) -> Rect {
        let (x0, y0) = (self.x.round(), self.y.round());
        let (x1, y1) = ((self.x + self.w).round(), (self.y + self.h).round());
        Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    }

    fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

enum Node {
    Leaf(PaneId),
    Split {
        split: Split,
        /// The fraction of the space (after the gap) that `a` gets.
        ratio: f32,
        /// Left or top.
        a: Box<Node>,
        /// Right or bottom.
        b: Box<Node>,
    },
}

impl Node {
    fn contains(&self, pane: PaneId) -> bool {
        match self {
            Node::Leaf(id) => *id == pane,
            Node::Split { a, b, .. } => a.contains(pane) || b.contains(pane),
        }
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// The path to `pane` as a list of "went into `b`" flags.
    fn path_to(&self, pane: PaneId, path: &mut Vec<bool>) -> bool {
        match self {
            Node::Leaf(id) => *id == pane,
            Node::Split { a, b, .. } => {
                for (went_b, child) in [(false, a), (true, b)] {
                    path.push(went_b);
                    if child.path_to(pane, path) {
                        return true;
                    }
                    path.pop();
                }
                false
            }
        }
    }

    /// Visit every split with its area and the position of its gap.
    fn walk(&self, area: Rect, gap: f32, visit: &mut impl FnMut(&Node, Rect, Option<Rect>)) {
        match self {
            Node::Leaf(_) => visit(self, area, None),
            Node::Split { split, ratio, a, b } => {
                let (ra, divider, rb) = divide(area, *split, *ratio, gap);
                visit(self, area, Some(divider));
                a.walk(ra, gap, visit);
                b.walk(rb, gap, visit);
            }
        }
    }
}

/// Split `area` into the `a` part, the gap, and the `b` part. The cut lands
/// on a whole pixel so both sides snap the same way.
fn divide(area: Rect, split: Split, ratio: f32, gap: f32) -> (Rect, Rect, Rect) {
    match split {
        Split::Right => {
            let avail = (area.w - gap).max(0.0);
            let cut = (area.x + avail * ratio).round();
            let a = Rect::new(area.x, area.y, cut - area.x, area.h);
            let d = Rect::new(cut, area.y, gap, area.h);
            let b = Rect::new(
                cut + gap,
                area.y,
                (area.x + area.w - cut - gap).max(0.0),
                area.h,
            );
            (a, d, b)
        }
        Split::Down => {
            let avail = (area.h - gap).max(0.0);
            let cut = (area.y + avail * ratio).round();
            let a = Rect::new(area.x, area.y, area.w, cut - area.y);
            let d = Rect::new(area.x, cut, area.w, gap);
            let b = Rect::new(
                area.x,
                cut + gap,
                area.w,
                (area.y + area.h - cut - gap).max(0.0),
            );
            (a, d, b)
        }
    }
}

/// A tree of split panes. Always holds at least one pane.
pub struct Layout {
    root: Node,
}

impl Layout {
    /// A layout with a single pane filling the whole area.
    pub fn new(root: PaneId) -> Layout {
        Layout {
            root: Node::Leaf(root),
        }
    }

    /// Split `target` in two, giving each half; `new` takes the right or
    /// bottom half. Returns false (and changes nothing) if `target` is not in
    /// the layout or `new` already is.
    pub fn split(&mut self, target: PaneId, how: Split, new: PaneId) -> bool {
        if self.root.contains(new) {
            return false;
        }
        fn go(node: &mut Node, target: PaneId, how: Split, new: PaneId) -> bool {
            match node {
                Node::Leaf(id) if *id == target => {
                    *node = Node::Split {
                        split: how,
                        ratio: 0.5,
                        a: Box::new(Node::Leaf(target)),
                        b: Box::new(Node::Leaf(new)),
                    };
                    true
                }
                Node::Leaf(_) => false,
                Node::Split { a, b, .. } => go(a, target, how, new) || go(b, target, how, new),
            }
        }
        go(&mut self.root, target, how, new)
    }

    /// Remove a pane; its sibling takes its space. Returns false if it was
    /// the last pane (layout unchanged) or not found.
    pub fn remove(&mut self, pane: PaneId) -> bool {
        fn go(node: &mut Node, pane: PaneId) -> bool {
            let Node::Split { a, b, .. } = node else {
                return false;
            };
            let keep = if matches!(**a, Node::Leaf(id) if id == pane) {
                std::mem::replace(&mut **b, Node::Leaf(pane))
            } else if matches!(**b, Node::Leaf(id) if id == pane) {
                std::mem::replace(&mut **a, Node::Leaf(pane))
            } else {
                return go(a, pane) || go(b, pane);
            };
            *node = keep;
            true
        }
        go(&mut self.root, pane)
    }

    /// Every pane, in tree order (left/top first).
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.root.collect(&mut out);
        out
    }

    /// Whether `pane` is in the layout.
    pub fn contains(&self, pane: PaneId) -> bool {
        self.root.contains(pane)
    }

    pub fn len(&self) -> usize {
        self.panes().len()
    }

    /// Always false: a layout has at least one pane.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Lay out within `area`, leaving `gap` px between siblings (for a
    /// divider). Each pane rect is snapped to whole pixels. Panes come in
    /// tree order.
    pub fn rects(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.root.walk(area, gap, &mut |node, r, _| {
            if let Node::Leaf(id) = node {
                out.push((*id, r.snapped()));
            }
        });
        out
    }

    /// Divider rects (the gaps between siblings) for drawing, outermost
    /// split first. A divider taller than it is wide belongs to a
    /// [`Split::Right`].
    pub fn dividers(&self, area: Rect, gap: f32) -> Vec<Rect> {
        let mut out = Vec::new();
        self.root.walk(area, gap, &mut |_, _, d| {
            if let Some(d) = d {
                out.push(d.snapped());
            }
        });
        out
    }

    /// The neighbor of `from` in `dir`: among panes whose edge touches
    /// `from`'s edge on that side, the one sharing the longest stretch of
    /// edge, ties going to the one whose center is closest.
    pub fn neighbor(&self, area: Rect, from: PaneId, dir: Direction) -> Option<PaneId> {
        let rects = self.rects(area, 0.0);
        let f = rects.iter().find(|(id, _)| *id == from)?.1;
        let (fcx, fcy) = f.center();
        const EPS: f32 = 0.5;
        let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| a1.min(b1) - a0.max(b0);
        let mut best: Option<(PaneId, f32, f32)> = None;
        for &(id, r) in &rects {
            if id == from {
                continue;
            }
            let (touches, shared, dist) = match dir {
                Direction::Left => (
                    (r.x + r.w - f.x).abs() < EPS,
                    overlap(r.y, r.y + r.h, f.y, f.y + f.h),
                    (r.center().1 - fcy).abs(),
                ),
                Direction::Right => (
                    (f.x + f.w - r.x).abs() < EPS,
                    overlap(r.y, r.y + r.h, f.y, f.y + f.h),
                    (r.center().1 - fcy).abs(),
                ),
                Direction::Up => (
                    (r.y + r.h - f.y).abs() < EPS,
                    overlap(r.x, r.x + r.w, f.x, f.x + f.w),
                    (r.center().0 - fcx).abs(),
                ),
                Direction::Down => (
                    (f.y + f.h - r.y).abs() < EPS,
                    overlap(r.x, r.x + r.w, f.x, f.x + f.w),
                    (r.center().0 - fcx).abs(),
                ),
            };
            if !touches || shared <= 0.0 {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, s, d)) => shared > s + EPS || ((shared - s).abs() <= EPS && dist < d),
            };
            if better {
                best = Some((id, shared, dist));
            }
        }
        best.map(|(id, _, _)| id)
    }

    /// Grow/shrink `pane` toward `dir` by `amount` (a fraction of the parent,
    /// e.g. 0.05) by moving the divider of the nearest ancestor split whose
    /// orientation matches `dir` that way: Left/Up shrink the left/top side,
    /// Right/Down grow it. Ratios clamp to [0.1, 0.9]. Returns whether
    /// anything changed.
    pub fn resize(&mut self, pane: PaneId, dir: Direction, amount: f32) -> bool {
        let mut path = Vec::new();
        if !self.root.path_to(pane, &mut path) {
            return false;
        }
        let (want, delta) = match dir {
            Direction::Left => (Split::Right, -amount),
            Direction::Right => (Split::Right, amount),
            Direction::Up => (Split::Down, -amount),
            Direction::Down => (Split::Down, amount),
        };
        // Find the depth of the deepest matching split along the path.
        let mut node = &self.root;
        let mut target = None;
        for (depth, &went_b) in path.iter().enumerate() {
            let Node::Split { split, a, b, .. } = node else {
                break;
            };
            if *split == want {
                target = Some(depth);
            }
            node = if went_b { b } else { a };
        }
        let Some(target) = target else {
            return false;
        };
        let mut node = &mut self.root;
        for &went_b in &path[..target] {
            let Node::Split { a, b, .. } = node else {
                unreachable!()
            };
            node = if went_b { b } else { a };
        }
        let Node::Split { ratio, .. } = node else {
            unreachable!()
        };
        let new = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
        let changed = (new - *ratio).abs() > f32::EPSILON;
        *ratio = new;
        changed
    }

    /// The divider at a point, by its index in [`Self::dividers`], with a
    /// few pixels of slack either side so thin gaps are easy to grab.
    pub fn divider_at(&self, area: Rect, gap: f32, x: f32, y: f32, slack: f32) -> Option<usize> {
        self.dividers(area, gap).iter().position(|d| {
            Rect::new(
                d.x - slack,
                d.y - slack,
                d.w + 2.0 * slack,
                d.h + 2.0 * slack,
            )
            .contains(x, y)
        })
    }

    /// Move divider `index` (as numbered by [`Self::dividers`]) so its gap
    /// starts at `pos`: an x for a [`Split::Right`], a y for a
    /// [`Split::Down`]. Returns whether anything moved.
    pub fn drag_divider(&mut self, area: Rect, gap: f32, index: usize, pos: f32) -> bool {
        fn find(
            node: &mut Node,
            area: Rect,
            gap: f32,
            index: &mut usize,
            pos: f32,
        ) -> Option<bool> {
            let Node::Split { split, ratio, a, b } = node else {
                return None;
            };
            let (ra, _, rb) = divide(area, *split, *ratio, gap);
            if *index == 0 {
                let (start, len) = match split {
                    Split::Right => (area.x, area.w - gap),
                    Split::Down => (area.y, area.h - gap),
                };
                if len <= 0.0 {
                    return Some(false);
                }
                let new = ((pos - start) / len).clamp(MIN_RATIO, MAX_RATIO);
                let moved = (new - *ratio).abs() > f32::EPSILON;
                *ratio = new;
                return Some(moved);
            }
            *index -= 1;
            find(a, ra, gap, index, pos).or_else(|| find(b, rb, gap, index, pos))
        }
        let mut index = index;
        find(&mut self.root, area, gap, &mut index, pos).unwrap_or(false)
    }

    /// Which pane is at a point, if any (points on a divider hit none).
    pub fn pane_at(&self, area: Rect, gap: f32, x: f32, y: f32) -> Option<PaneId> {
        self.rects(area, gap)
            .into_iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(id, _)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 101.0,
        h: 61.0,
    };

    fn rect_of(l: &Layout, id: PaneId, gap: f32) -> Rect {
        l.rects(AREA, gap)
            .into_iter()
            .find(|(p, _)| *p == id)
            .unwrap()
            .1
    }

    /// 1 | 2 on top, with 2 split into 2 over 3.
    fn three() -> Layout {
        let mut l = Layout::new(1);
        assert!(l.split(1, Split::Right, 2));
        assert!(l.split(2, Split::Down, 3));
        l
    }

    #[test]
    fn single_pane_fills_the_area() {
        let l = Layout::new(7);
        assert_eq!(l.panes(), vec![7]);
        assert_eq!((l.len(), l.is_empty()), (1, false));
        assert_eq!(l.rects(AREA, 1.0), vec![(7, AREA)]);
        assert!(l.dividers(AREA, 1.0).is_empty());
    }

    #[test]
    fn side_by_side_get_equal_widths_minus_gap() {
        let mut l = Layout::new(1);
        assert!(l.split(1, Split::Right, 2));
        assert_eq!(l.panes(), vec![1, 2]);
        let r = l.rects(AREA, 1.0);
        assert_eq!(r[0], (1, Rect::new(0.0, 0.0, 50.0, 61.0)));
        assert_eq!(r[1], (2, Rect::new(51.0, 0.0, 50.0, 61.0)));
        assert_eq!(l.dividers(AREA, 1.0), vec![Rect::new(50.0, 0.0, 1.0, 61.0)]);
    }

    #[test]
    fn nested_splits() {
        let l = three();
        assert_eq!(l.panes(), vec![1, 2, 3]);
        assert_eq!(rect_of(&l, 1, 1.0), Rect::new(0.0, 0.0, 50.0, 61.0));
        assert_eq!(rect_of(&l, 2, 1.0), Rect::new(51.0, 0.0, 50.0, 30.0));
        assert_eq!(rect_of(&l, 3, 1.0), Rect::new(51.0, 31.0, 50.0, 30.0));
        assert_eq!(
            l.dividers(AREA, 1.0),
            vec![
                Rect::new(50.0, 0.0, 1.0, 61.0),
                Rect::new(51.0, 30.0, 50.0, 1.0)
            ]
        );
        // Odd sizes still snap to whole pixels and tile the area.
        let area = Rect::new(0.5, 0.0, 99.3, 40.7);
        for (_, r) in l.rects(area, 2.0) {
            assert_eq!(
                (r.x.fract(), r.y.fract(), r.w.fract(), r.h.fract()),
                (0.0, 0.0, 0.0, 0.0)
            );
        }
    }

    #[test]
    fn split_rejects_unknown_target_and_duplicate_id() {
        let mut l = three();
        assert!(!l.split(9, Split::Right, 10));
        assert!(!l.split(1, Split::Right, 3));
        assert_eq!(l.panes(), vec![1, 2, 3]);
    }

    #[test]
    fn remove_gives_space_to_sibling() {
        let mut l = three();
        assert!(l.remove(2));
        assert_eq!(l.panes(), vec![1, 3]);
        assert_eq!(rect_of(&l, 3, 1.0), Rect::new(51.0, 0.0, 50.0, 61.0));
        assert!(!l.remove(2));
        assert!(l.remove(1));
        assert_eq!(l.rects(AREA, 1.0), vec![(3, AREA)]);
        assert!(!l.remove(3));
        assert_eq!(l.panes(), vec![3]);
        // Removing a pane whose sibling is a subtree keeps the subtree.
        let mut l = three();
        assert!(l.remove(1));
        assert_eq!(l.panes(), vec![2, 3]);
        assert_eq!(rect_of(&l, 2, 1.0), Rect::new(0.0, 0.0, 101.0, 30.0));
    }

    #[test]
    fn neighbors() {
        let l = three();
        assert_eq!(l.neighbor(AREA, 1, Direction::Right), Some(2));
        assert_eq!(l.neighbor(AREA, 1, Direction::Left), None);
        assert_eq!(l.neighbor(AREA, 1, Direction::Up), None);
        assert_eq!(l.neighbor(AREA, 2, Direction::Left), Some(1));
        assert_eq!(l.neighbor(AREA, 3, Direction::Left), Some(1));
        assert_eq!(l.neighbor(AREA, 2, Direction::Down), Some(3));
        assert_eq!(l.neighbor(AREA, 3, Direction::Up), Some(2));
        assert_eq!(l.neighbor(AREA, 3, Direction::Right), None);
        assert_eq!(l.neighbor(AREA, 42, Direction::Right), None);

        // The pane sharing more edge wins.
        let mut l = three();
        assert!(l.resize(2, Direction::Down, 0.2));
        assert_eq!(l.neighbor(AREA, 1, Direction::Right), Some(2));
        assert!(l.resize(2, Direction::Up, 0.4));
        assert_eq!(l.neighbor(AREA, 1, Direction::Right), Some(3));
    }

    #[test]
    fn resize_moves_the_nearest_matching_divider_and_clamps() {
        let mut l = three();
        // 3 is under a Down split inside a Right split: Left moves the outer one.
        assert!(l.resize(3, Direction::Left, 0.1));
        assert_eq!(rect_of(&l, 1, 0.0).w, 40.0);
        assert!(l.resize(1, Direction::Right, 0.2));
        assert_eq!(rect_of(&l, 1, 0.0).w, 61.0);
        // Up/Down on 2 moves the inner split.
        assert!(l.resize(2, Direction::Down, 0.1));
        assert_eq!(rect_of(&l, 2, 0.0).h, 37.0);
        // Clamps at 0.9 and reports no change once there.
        assert!(l.resize(1, Direction::Right, 5.0));
        assert_eq!(rect_of(&l, 1, 0.0).w, (101.0f32 * 0.9).round());
        assert!(!l.resize(1, Direction::Right, 0.05));
        assert!(l.resize(1, Direction::Left, 5.0));
        assert_eq!(rect_of(&l, 1, 0.0).w, (101.0f32 * 0.1).round());
        // No matching ancestor, or unknown pane.
        assert!(!l.resize(1, Direction::Up, 0.1));
        assert!(!Layout::new(1).resize(1, Direction::Left, 0.1));
        assert!(!l.resize(9, Direction::Left, 0.1));
    }

    #[test]
    fn pane_at_points() {
        let l = three();
        assert_eq!(l.pane_at(AREA, 1.0, 10.0, 10.0), Some(1));
        assert_eq!(l.pane_at(AREA, 1.0, 49.9, 60.0), Some(1));
        assert_eq!(l.pane_at(AREA, 1.0, 50.5, 10.0), None); // on the divider
        assert_eq!(l.pane_at(AREA, 1.0, 51.0, 0.0), Some(2));
        assert_eq!(l.pane_at(AREA, 1.0, 80.0, 45.0), Some(3));
        assert_eq!(l.pane_at(AREA, 1.0, 200.0, 10.0), None);
        assert!(AREA.contains(0.0, 0.0) && !AREA.contains(101.0, 0.0));
    }

    #[test]
    fn dragging_a_divider_moves_it() {
        let mut l = Layout::new(1);
        assert!(l.split(1, Split::Right, 2));
        assert!(l.split(2, Split::Down, 3));
        let d = l.dividers(AREA, 1.0);
        assert_eq!(l.divider_at(AREA, 1.0, d[1].x + 3.0, d[1].y, 2.0), Some(1));
        assert_eq!(l.divider_at(AREA, 1.0, d[0].x, 5.0, 2.0), Some(0));
        assert!(l.drag_divider(AREA, 1.0, 0, 25.0));
        assert_eq!(l.dividers(AREA, 1.0)[0].x, 25.0);
        // Clamped so no pane vanishes.
        l.drag_divider(AREA, 1.0, 0, 0.0);
        assert_eq!(l.dividers(AREA, 1.0)[0].x, 10.0);
        assert!(!l.drag_divider(AREA, 1.0, 5, 3.0));
    }
}
