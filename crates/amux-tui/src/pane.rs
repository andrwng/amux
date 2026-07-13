//! A pure binary-space-partition pane tree for the tiled main area: split, close, directional
//! navigate, and resize. Generic over the pane payload (the app stores a `TerminalId`). No I/O,
//! no rendering — it only computes rectangles. Heavily unit-tested. See `docs/SPLITS.md`.

use ratatui::layout::Rect;

/// Movement/resize direction and split axis, shared with the wire (and the mailbox) so `amux nav`,
/// a `Ctrl+h` keypress, and a persisted layout all mean the same thing.
pub use amux_core::nav::{Axis, Dir};

pub type PaneId = u64;

enum Node<P> {
    Leaf {
        id: PaneId,
        payload: Option<P>,
    },
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Node<P>>,
        second: Box<Node<P>>,
    },
}

/// A pane's placement, for rendering and geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement<P> {
    pub id: PaneId,
    pub payload: Option<P>,
    pub rect: Rect,
    pub focused: bool,
}

/// Result of a navigation attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Moved,
    Stay,
    /// No pane to the left — the caller should move focus to the sidebar.
    ExitLeft,
}

/// The tiled pane layout: a tree of splits with a focused leaf.
pub struct PaneTree<P> {
    root: Option<Node<P>>,
    focus: PaneId,
    next_id: PaneId,
}

impl<P: Copy + PartialEq> Default for PaneTree<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Copy + PartialEq> PaneTree<P> {
    pub fn new() -> Self {
        Self {
            root: None,
            focus: 0,
            next_id: 1,
        }
    }

    fn alloc(&mut self) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn focused_payload(&self) -> Option<P> {
        self.root
            .as_ref()
            .and_then(|n| find_payload(n, self.focus))
            .flatten()
    }

    /// Every payload currently shown in a pane.
    pub fn payloads(&self) -> Vec<P> {
        let mut out = Vec::new();
        if let Some(n) = &self.root {
            collect_payloads(n, &mut out);
        }
        out
    }

    /// Put `payload` into the focused pane (creating the first pane if the tree is empty).
    pub fn open(&mut self, payload: P) {
        match &mut self.root {
            None => {
                let id = self.alloc();
                self.root = Some(Node::Leaf {
                    id,
                    payload: Some(payload),
                });
                self.focus = id;
            }
            Some(node) => set_payload(node, self.focus, Some(payload)),
        }
    }

    /// Split the focused pane; the new (empty) pane becomes focused. No-op on an empty tree.
    pub fn split(&mut self, axis: Axis) {
        if self.root.is_none() {
            return;
        }
        let new_id = self.alloc();
        let focus = self.focus;
        if let Some(root) = self.root.take() {
            self.root = Some(split_leaf(root, focus, axis, new_id));
        }
        self.focus = new_id;
    }

    /// Close the focused pane; focus moves to its sibling. Empties the tree if it was the last.
    pub fn close(&mut self) {
        self.close_id(self.focus);
    }

    /// Close the pane showing `payload`, if any. Returns whether one was found.
    pub fn close_payload(&mut self, payload: P) -> bool {
        match self.root.as_ref().and_then(|n| leaf_with(n, payload)) {
            Some(id) => {
                self.close_id(id);
                true
            }
            None => false,
        }
    }

    fn close_id(&mut self, id: PaneId) {
        if let Some(root) = self.root.take() {
            let (new_root, sibling) = close_leaf(root, id);
            self.root = new_root;
            if self.focus == id {
                self.focus = sibling
                    .or_else(|| self.root.as_ref().map(first_leaf))
                    .unwrap_or(0);
            }
        }
    }

    pub fn focus_first(&mut self) {
        if let Some(node) = &self.root {
            self.focus = first_leaf(node);
        }
    }

    /// Focus the pane holding `payload` (e.g. a mouse click landed on it). Returns whether it was
    /// found.
    pub fn focus_payload(&mut self, payload: P) -> bool {
        if let Some(node) = &self.root {
            if let Some(id) = leaf_with_payload(node, payload) {
                self.focus = id;
                return true;
            }
        }
        false
    }

    /// Move focus in `dir` within `area`; reports whether it moved, stayed, or hit the left edge.
    pub fn navigate(&mut self, dir: Dir, area: Rect) -> Nav {
        let places = self.layout(area);
        let Some(current) = places.iter().find(|p| p.id == self.focus).map(|p| p.rect) else {
            return Nav::Stay;
        };
        let best = places
            .iter()
            .filter(|p| p.id != self.focus && in_dir(dir, current, p.rect))
            .min_by_key(|p| dist(dir, current, p.rect));
        match best {
            Some(p) => {
                self.focus = p.id;
                Nav::Moved
            }
            None if dir == Dir::Left => Nav::ExitLeft,
            None => Nav::Stay,
        }
    }

    /// Grow/shrink the focused pane in `dir` by `step`, adjusting the nearest ancestor split of
    /// the matching axis.
    pub fn resize(&mut self, dir: Dir, step: f32) {
        let (axis, grow) = match dir {
            Dir::Right => (Axis::LeftRight, true),
            Dir::Left => (Axis::LeftRight, false),
            Dir::Down => (Axis::TopBottom, true),
            Dir::Up => (Axis::TopBottom, false),
        };
        let focus = self.focus;
        if let Some(root) = &mut self.root {
            resize_toward(root, focus, axis, grow, step);
        }
    }

    /// Placements for every pane within `area`.
    pub fn layout(&self, area: Rect) -> Vec<Placement<P>> {
        let mut out = Vec::new();
        if let Some(node) = &self.root {
            layout_node(node, area, self.focus, &mut out);
        }
        out
    }
}

// --- layout (de)serialization for persistence ---

use amux_core::agent::TerminalId;
use amux_proto::Layout;

impl PaneTree<TerminalId> {
    /// The tree as a persistable [`Layout`] (`None` when empty).
    pub fn to_layout(&self) -> Option<Layout> {
        self.root.as_ref().map(node_to_layout)
    }

    /// Rebuild a tree from a saved [`Layout`]; focus lands on the first pane.
    pub fn from_layout(layout: &Layout) -> Self {
        let mut next_id = 1;
        let root = layout_to_node(layout, &mut next_id);
        let focus = first_leaf(&root);
        Self {
            root: Some(root),
            focus,
            next_id,
        }
    }
}

fn node_to_layout(node: &Node<TerminalId>) -> Layout {
    match node {
        Node::Leaf { payload, .. } => Layout::Leaf { terminal: *payload },
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => Layout::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(node_to_layout(first)),
            second: Box::new(node_to_layout(second)),
        },
    }
}

fn layout_to_node(layout: &Layout, next_id: &mut PaneId) -> Node<TerminalId> {
    match layout {
        Layout::Leaf { terminal } => {
            let id = *next_id;
            *next_id += 1;
            Node::Leaf {
                id,
                payload: *terminal,
            }
        }
        Layout::Split {
            axis,
            ratio,
            first,
            second,
        } => Node::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(layout_to_node(first, next_id)),
            second: Box::new(layout_to_node(second, next_id)),
        },
    }
}

// --- tree helpers ---

fn find_payload<P: Copy>(node: &Node<P>, id: PaneId) -> Option<Option<P>> {
    match node {
        Node::Leaf { id: lid, payload } if *lid == id => Some(*payload),
        Node::Leaf { .. } => None,
        Node::Split { first, second, .. } => {
            find_payload(first, id).or_else(|| find_payload(second, id))
        }
    }
}

fn collect_payloads<P: Copy>(node: &Node<P>, out: &mut Vec<P>) {
    match node {
        Node::Leaf { payload, .. } => {
            if let Some(p) = payload {
                out.push(*p);
            }
        }
        Node::Split { first, second, .. } => {
            collect_payloads(first, out);
            collect_payloads(second, out);
        }
    }
}

fn leaf_with<P: Copy + PartialEq>(node: &Node<P>, payload: P) -> Option<PaneId> {
    match node {
        Node::Leaf {
            id,
            payload: Some(p),
        } if *p == payload => Some(*id),
        Node::Leaf { .. } => None,
        Node::Split { first, second, .. } => {
            leaf_with(first, payload).or_else(|| leaf_with(second, payload))
        }
    }
}

fn set_payload<P>(node: &mut Node<P>, id: PaneId, payload: Option<P>) {
    match node {
        Node::Leaf {
            id: lid,
            payload: p,
        } if *lid == id => *p = payload,
        Node::Leaf { .. } => {}
        Node::Split { first, second, .. } => {
            // Only one branch will match; the payload isn't Clone so hand it down carefully.
            if contains(first, id) {
                set_payload(first, id, payload);
            } else {
                set_payload(second, id, payload);
            }
        }
    }
}

fn contains<P>(node: &Node<P>, id: PaneId) -> bool {
    match node {
        Node::Leaf { id: lid, .. } => *lid == id,
        Node::Split { first, second, .. } => contains(first, id) || contains(second, id),
    }
}

fn split_leaf<P>(node: Node<P>, target: PaneId, axis: Axis, new_id: PaneId) -> Node<P> {
    match node {
        Node::Leaf { id, payload } if id == target => Node::Split {
            axis,
            ratio: 0.5,
            first: Box::new(Node::Leaf { id, payload }),
            second: Box::new(Node::Leaf {
                id: new_id,
                payload: None,
            }),
        },
        Node::Leaf { .. } => node,
        // Recurse into the existing split, preserving *its* axis (`node_axis`) while still passing
        // the caller's requested `axis` down — shadowing here was the bug that made every split
        // inherit the first one's orientation.
        Node::Split {
            axis: node_axis,
            ratio,
            first,
            second,
        } => Node::Split {
            axis: node_axis,
            ratio,
            first: Box::new(split_leaf(*first, target, axis, new_id)),
            second: Box::new(split_leaf(*second, target, axis, new_id)),
        },
    }
}

fn close_leaf<P>(node: Node<P>, target: PaneId) -> (Option<Node<P>>, Option<PaneId>) {
    match node {
        Node::Leaf { id, .. } if id == target => (None, None),
        Node::Leaf { .. } => (Some(node), None),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            if matches!(&*first, Node::Leaf { id, .. } if *id == target) {
                let focus = first_leaf(&second);
                return (Some(*second), Some(focus));
            }
            if matches!(&*second, Node::Leaf { id, .. } if *id == target) {
                let focus = first_leaf(&first);
                return (Some(*first), Some(focus));
            }
            let (nf, ff) = close_leaf(*first, target);
            let (ns, fs) = close_leaf(*second, target);
            match (nf, ns) {
                (Some(f), Some(s)) => (
                    Some(Node::Split {
                        axis,
                        ratio,
                        first: Box::new(f),
                        second: Box::new(s),
                    }),
                    ff.or(fs),
                ),
                (Some(only), None) | (None, Some(only)) => (Some(only), ff.or(fs)),
                (None, None) => (None, ff.or(fs)),
            }
        }
    }
}

fn first_leaf<P>(node: &Node<P>) -> PaneId {
    match node {
        Node::Leaf { id, .. } => *id,
        Node::Split { first, .. } => first_leaf(first),
    }
}

/// The id of the leaf whose payload equals `payload`, if any.
fn leaf_with_payload<P: Copy + PartialEq>(node: &Node<P>, payload: P) -> Option<PaneId> {
    match node {
        Node::Leaf {
            id,
            payload: Some(p),
        } if *p == payload => Some(*id),
        Node::Leaf { .. } => None,
        Node::Split { first, second, .. } => {
            leaf_with_payload(first, payload).or_else(|| leaf_with_payload(second, payload))
        }
    }
}

fn resize_toward<P>(
    node: &mut Node<P>,
    focus: PaneId,
    axis: Axis,
    grow: bool,
    step: f32,
) -> (bool, bool) {
    match node {
        Node::Leaf { id, .. } => (*id == focus, false),
        Node::Split {
            axis: node_axis,
            ratio,
            first,
            second,
        } => {
            let (in_first, handled_first) = resize_toward(first, focus, axis, grow, step);
            let (in_second, handled_second) = if in_first {
                (false, false)
            } else {
                resize_toward(second, focus, axis, grow, step)
            };
            let in_sub = in_first || in_second;
            let handled = handled_first || handled_second;
            if in_sub && !handled && *node_axis == axis {
                // Move the shared boundary in the key's screen direction (Down/Right grow the first
                // child, Up/Left the second) — so resize reads from the focused pane's perspective:
                // on the top pane J extends it down; on the bottom pane K extends it up. The sign
                // is independent of which side the focus is on (a `== in_first` here inverts the
                // bottom/right pane and makes K shrink instead of grow).
                let delta = if grow { step } else { -step };
                *ratio = (*ratio + delta).clamp(0.1, 0.9);
                return (true, true);
            }
            (in_sub, handled)
        }
    }
}

fn layout_node<P: Copy>(node: &Node<P>, rect: Rect, focus: PaneId, out: &mut Vec<Placement<P>>) {
    match node {
        Node::Leaf { id, payload } => out.push(Placement {
            id: *id,
            payload: *payload,
            rect,
            focused: *id == focus,
        }),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (r1, r2) = split_rect(rect, *axis, *ratio);
            layout_node(first, r1, focus, out);
            layout_node(second, r2, focus, out);
        }
    }
}

fn split_rect(rect: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    match axis {
        Axis::LeftRight => {
            let w1 = ((rect.width as f32 * ratio).round() as u16)
                .clamp(1, rect.width.saturating_sub(1).max(1));
            (
                Rect::new(rect.x, rect.y, w1, rect.height),
                Rect::new(
                    rect.x + w1,
                    rect.y,
                    rect.width.saturating_sub(w1),
                    rect.height,
                ),
            )
        }
        Axis::TopBottom => {
            let h1 = ((rect.height as f32 * ratio).round() as u16)
                .clamp(1, rect.height.saturating_sub(1).max(1));
            (
                Rect::new(rect.x, rect.y, rect.width, h1),
                Rect::new(
                    rect.x,
                    rect.y + h1,
                    rect.width,
                    rect.height.saturating_sub(h1),
                ),
            )
        }
    }
}

fn in_dir(dir: Dir, cur: Rect, other: Rect) -> bool {
    let overlap_v = cur.y < other.y + other.height && other.y < cur.y + cur.height;
    let overlap_h = cur.x < other.x + other.width && other.x < cur.x + cur.width;
    match dir {
        Dir::Left => other.x + other.width <= cur.x && overlap_v,
        Dir::Right => other.x >= cur.x + cur.width && overlap_v,
        Dir::Up => other.y + other.height <= cur.y && overlap_h,
        Dir::Down => other.y >= cur.y + cur.height && overlap_h,
    }
}

fn dist(dir: Dir, cur: Rect, other: Rect) -> (u32, u32) {
    let gap = match dir {
        Dir::Left => cur.x.saturating_sub(other.x + other.width),
        Dir::Right => other.x.saturating_sub(cur.x + cur.width),
        Dir::Up => cur.y.saturating_sub(other.y + other.height),
        Dir::Down => other.y.saturating_sub(cur.y + cur.height),
    } as u32;
    let center = |r: Rect| {
        (
            r.x as i32 + r.width as i32 / 2,
            r.y as i32 + r.height as i32 / 2,
        )
    };
    let (cx, cy) = center(cur);
    let (ox, oy) = center(other);
    let perp = match dir {
        Dir::Left | Dir::Right => (oy - cy).unsigned_abs(),
        Dir::Up | Dir::Down => (ox - cx).unsigned_abs(),
    };
    (gap, perp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::agent::TerminalId;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn open_into_empty_creates_one_focused_pane() {
        let mut t = PaneTree::new();
        assert!(t.is_empty());
        let a = TerminalId::new();
        t.open(a);
        assert_eq!(t.focused_payload(), Some(a));
        assert_eq!(t.payloads(), vec![a]);
        assert_eq!(t.layout(area()).len(), 1);
    }

    #[test]
    fn split_makes_two_panes_new_one_empty_and_focused() {
        let mut t = PaneTree::new();
        let a = TerminalId::new();
        t.open(a);
        t.split(Axis::LeftRight);
        assert_eq!(t.layout(area()).len(), 2);
        assert_eq!(t.focused_payload(), None);
        assert_eq!(t.payloads(), vec![a]);
        let b = TerminalId::new();
        t.open(b);
        assert_eq!(t.focused_payload(), Some(b));
        assert_eq!(t.payloads().len(), 2);
    }

    #[test]
    fn layout_roundtrips_through_serialization() {
        let mut t = PaneTree::new();
        let a = TerminalId::new();
        t.open(a);
        t.split(Axis::LeftRight);
        let b = TerminalId::new();
        t.open(b);
        t.split(Axis::TopBottom);
        let c = TerminalId::new();
        t.open(c);

        let before = t.layout(area());
        let saved = t.to_layout().expect("non-empty");
        let restored = PaneTree::<TerminalId>::from_layout(&saved);

        // Same panes at the same rectangles after a save/restore.
        let after = restored.layout(area());
        let rects_before: Vec<_> = before.iter().map(|p| (p.payload, p.rect)).collect();
        let rects_after: Vec<_> = after.iter().map(|p| (p.payload, p.rect)).collect();
        assert_eq!(rects_before, rects_after);
    }

    #[test]
    fn each_split_uses_its_requested_axis() {
        // Regression: splitting a pane that lives inside an existing split must honor the axis
        // asked for, not inherit the parent split's axis.
        let mut t = PaneTree::new();
        let a = TerminalId::new();
        t.open(a);
        t.split(Axis::LeftRight); // a | (new)
        let b = TerminalId::new();
        t.open(b); // b is the focused right pane
        t.split(Axis::TopBottom); // split b top/bottom
        let c = TerminalId::new();
        t.open(c);

        let places = t.layout(area());
        let rect = |p: TerminalId| {
            places
                .iter()
                .find(|pl| pl.payload == Some(p))
                .expect("pane present")
                .rect
        };
        let (ra, rb, rc) = (rect(a), rect(b), rect(c));
        // Left/right split: `a` is the left column, full height.
        assert_eq!(ra.x, 0);
        assert_eq!(ra.height, area().height, "a spans full height");
        assert!(rb.x > ra.x, "b/c are the right column");
        // Top/bottom split of the right column: b and c share x/width but stack vertically.
        assert_eq!(rb.x, rc.x);
        assert_eq!(rb.width, rc.width);
        assert_ne!(rb.y, rc.y, "b and c are stacked, not side by side");
    }

    #[test]
    fn close_collapses_back_to_sibling() {
        let mut t = PaneTree::new();
        let a = TerminalId::new();
        t.open(a);
        t.split(Axis::LeftRight);
        t.close();
        assert_eq!(t.layout(area()).len(), 1);
        assert_eq!(t.focused_payload(), Some(a));
        t.close();
        assert!(t.is_empty());
    }

    #[test]
    fn close_payload_removes_the_right_pane() {
        let mut t = PaneTree::new();
        let a = TerminalId::new();
        t.open(a);
        t.split(Axis::LeftRight);
        let b = TerminalId::new();
        t.open(b);
        assert!(t.close_payload(a)); // close the non-focused pane by payload
        assert_eq!(t.payloads(), vec![b]);
        assert!(!t.close_payload(TerminalId::new())); // unknown payload
    }

    #[test]
    fn navigate_moves_across_panes_and_exits_left() {
        let mut t = PaneTree::new();
        t.open(TerminalId::new());
        t.split(Axis::LeftRight);
        let a = area();
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved);
        assert_eq!(t.navigate(Dir::Left, a), Nav::ExitLeft);
        assert_eq!(t.navigate(Dir::Right, a), Nav::Moved);
        assert_eq!(t.navigate(Dir::Up, a), Nav::Stay);
    }

    #[test]
    fn resize_reads_from_the_focused_panes_perspective() {
        // Split left/right; focus lands on the new (right) pane, whose shared boundary is on its
        // left. So Left grows it (extends toward the boundary) and Right shrinks it — the sign
        // must not depend on which side the focus is on.
        let mut t = PaneTree::new();
        t.open(TerminalId::new());
        t.split(Axis::LeftRight);
        let a = area();
        let focused_width = |t: &PaneTree<TerminalId>| {
            t.layout(a)
                .into_iter()
                .find(|p| p.focused)
                .unwrap()
                .rect
                .width
        };

        let before = focused_width(&t);
        t.resize(Dir::Left, 0.1);
        let grown = focused_width(&t);
        assert!(
            grown > before,
            "Left should grow the right pane toward its boundary ({before} → {grown})"
        );
        t.resize(Dir::Right, 0.1);
        let shrunk = focused_width(&t);
        assert!(
            shrunk < grown,
            "Right should shrink the right pane ({grown} → {shrunk})"
        );
    }
}
