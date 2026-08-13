//! A pure binary-space-partition pane tree for the tiled main area: split, close, directional
//! navigate, and resize. Generic over the pane payload (the app stores a `TerminalId`). No I/O,
//! no rendering — it only computes rectangles. Heavily unit-tested. See `docs/SPLITS.md`.

use std::collections::HashMap;

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
    /// Monotonic focus clock + per-pane stamp of when it was last focused (by any means).
    /// Used by [`navigate`](Self::navigate) to prefer returning where the user last worked when
    /// several candidates tie geometrically. Session-only — never persisted in a `Layout`.
    clock: u64,
    last_focus: HashMap<PaneId, u64>,
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
            clock: 0,
            last_focus: HashMap::new(),
        }
    }

    /// Move focus to `id`, stamping it as the most recently focused pane. Every path that
    /// changes focus goes through here so recency reflects *any* visit (keys, mouse, splits).
    fn set_focus(&mut self, id: PaneId) {
        self.focus = id;
        self.clock += 1;
        self.last_focus.insert(id, self.clock);
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
                self.set_focus(id);
            }
            Some(node) => set_payload(node, self.focus, Some(payload)),
        }
    }

    /// Give every empty pane a payload from `next`, returning them in assignment order.
    ///
    /// Used when restoring a layout the daemon persisted across a restart: the shell terminals
    /// died with the old daemon, so their leaves arrive blank and each needs a freshly spawned
    /// one. A blank leaf already means "a pane waiting for its terminal" — it is exactly the state
    /// [`split`](Self::split) leaves behind until its `SpawnShell` lands — so this reuses that
    /// meaning rather than inventing a second kind of empty pane.
    pub fn fill_blanks(&mut self, mut next: impl FnMut() -> P) -> Vec<P> {
        let mut filled = Vec::new();
        if let Some(root) = &mut self.root {
            fill_blank_leaves(root, &mut next, &mut filled);
        }
        filled
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
        self.set_focus(new_id);
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
            self.last_focus.remove(&id);
            if self.focus == id {
                match sibling.or_else(|| self.root.as_ref().map(first_leaf)) {
                    Some(next) => self.set_focus(next),
                    // The tree is now empty: nothing to focus, so nothing to stamp.
                    None => self.focus = 0,
                }
            }
        }
    }

    /// Focus the pane the user worked in most recently — for returning from the sidebar so focus
    /// lands where they left off, the way [`navigate`](Self::navigate) does between panes. Falls
    /// back to the first leaf when the tree has no history (a freshly restored layout).
    pub fn focus_most_recent(&mut self) {
        if let Some(node) = &self.root {
            let id = most_recent_leaf(node, &self.last_focus);
            self.set_focus(id);
        }
    }

    /// Focus the pane holding `payload` (e.g. a mouse click landed on it). Returns whether it was
    /// found.
    pub fn focus_payload(&mut self, payload: P) -> bool {
        if let Some(node) = &self.root {
            if let Some(id) = leaf_with_payload(node, payload) {
                self.set_focus(id);
                return true;
            }
        }
        false
    }

    /// Move focus in `dir` within `area`; reports whether it moved, stayed, or hit the left edge.
    /// Among candidates the nearest wins; ties prefer the most recently focused pane ("go back
    /// where I was"), then center alignment for panes with no history (a restored layout).
    pub fn navigate(&mut self, dir: Dir, area: Rect) -> Nav {
        let places = self.layout(area);
        let Some(current) = places.iter().find(|p| p.id == self.focus).map(|p| p.rect) else {
            return Nav::Stay;
        };
        let best = places
            .iter()
            .filter(|p| p.id != self.focus && in_dir(dir, current, p.rect))
            .min_by_key(|p| {
                let (gap, perp) = dist(dir, current, p.rect);
                let recency = self.last_focus.get(&p.id).copied().unwrap_or(0);
                (gap, std::cmp::Reverse(recency), perp)
            });
        match best {
            Some(p) => {
                self.set_focus(p.id);
                Nav::Moved
            }
            None if dir == Dir::Left => Nav::ExitLeft,
            None => Nav::Stay,
        }
    }

    /// Grow/shrink the focused pane in `dir` by `step`, adjusting the nearest ancestor split of
    /// the matching axis.
    pub fn resize(&mut self, dir: Dir, step: f32) {
        let (axis, grow) = axis_and_grow(dir);
        let delta = if grow { step } else { -step };
        self.adjust_boundary(axis, move |ratio| ratio + delta);
    }

    /// Snap the same boundary [`resize`](Self::resize) would move to the next clean stop in `dir`
    /// (quarters and thirds) — the coarse, capital-HJKL counterpart to the fine nudge. Stops are
    /// fractions of the moved split's own slot, so nested and top-level splits behave alike.
    pub fn resize_snap(&mut self, dir: Dir) {
        let (axis, grow) = axis_and_grow(dir);
        self.adjust_boundary(axis, move |ratio| snap_ratio(ratio, grow));
    }

    /// Walk to the nearest matching-axis split above the focused pane and remap its ratio through
    /// `adjust`, clamped to a visible range. Both resize paths share this so they always land on
    /// the identical boundary — they differ only in how they move it.
    fn adjust_boundary(&mut self, axis: Axis, adjust: impl Fn(f32) -> f32) {
        let focus = self.focus;
        if let Some(root) = &mut self.root {
            resize_toward(root, focus, axis, &adjust);
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

    /// Rebuild a tree from a saved [`Layout`]; focus lands on the first pane. Focus history is
    /// not persisted, so only that initial focus carries a recency stamp — directional ties in
    /// a fresh restore fall back to center alignment until the user starts moving around.
    pub fn from_layout(layout: &Layout) -> Self {
        let mut next_id = 1;
        let root = layout_to_node(layout, &mut next_id);
        let focus = first_leaf(&root);
        let mut tree = Self {
            root: Some(root),
            focus: 0,
            next_id,
            clock: 0,
            last_focus: HashMap::new(),
        };
        tree.set_focus(focus);
        tree
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

/// Depth-first, first-pane-first: assign a payload to each empty leaf, collecting what was
/// assigned so the caller can spawn exactly one terminal per pane.
fn fill_blank_leaves<P: Copy>(node: &mut Node<P>, next: &mut impl FnMut() -> P, out: &mut Vec<P>) {
    match node {
        Node::Leaf { payload, .. } => {
            if payload.is_none() {
                let value = next();
                out.push(value);
                *payload = Some(value);
            }
        }
        Node::Split { first, second, .. } => {
            fill_blank_leaves(first, next, out);
            fill_blank_leaves(second, next, out);
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

/// The leaf with the largest recency stamp; ties (and a fully unstamped tree) resolve to the
/// leftmost, matching [`first_leaf`]. Every visit stamps through `set_focus`, so this is the pane
/// currently focused whenever the focus is a live leaf.
fn most_recent_leaf<P>(node: &Node<P>, stamps: &HashMap<PaneId, u64>) -> PaneId {
    match node {
        Node::Leaf { id, .. } => *id,
        Node::Split { first, second, .. } => {
            let a = most_recent_leaf(first, stamps);
            let b = most_recent_leaf(second, stamps);
            let stamp = |id| stamps.get(id).copied().unwrap_or(0);
            if stamp(&b) > stamp(&a) {
                b
            } else {
                a
            }
        }
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

/// Direction → (split axis, does the boundary move toward larger ratios?). Down/Right grow the
/// first child (ratio up); Up/Left grow the second (ratio down). The sign is independent of which
/// side the focus is on, so resize reads from the focused pane's perspective: on the top pane J
/// extends it down; on the bottom pane K extends it up.
fn axis_and_grow(dir: Dir) -> (Axis, bool) {
    match dir {
        Dir::Right => (Axis::LeftRight, true),
        Dir::Left => (Axis::LeftRight, false),
        Dir::Down => (Axis::TopBottom, true),
        Dir::Up => (Axis::TopBottom, false),
    }
}

/// Ratio stops the capital-HJKL snap lands on — quarters and thirds, bracketed by the 0.1/0.9
/// clamp bounds so an edge press still moves *somewhere* until the boundary is pinned. Ascending.
const SNAP_STOPS: [f32; 7] = [0.1, 0.25, 1.0 / 3.0, 0.5, 2.0 / 3.0, 0.75, 0.9];

/// Snap `ratio` to the next stop in the move direction: `grow` climbs to the next-larger stop,
/// shrink drops to the next-smaller. The epsilon keeps a press off the stop it already sits on
/// (so landing exactly on 0.5 and pressing again advances); at the far end the ratio is returned
/// unchanged.
fn snap_ratio(ratio: f32, grow: bool) -> f32 {
    const EPS: f32 = 0.01;
    if grow {
        SNAP_STOPS
            .iter()
            .copied()
            .find(|&s| s > ratio + EPS)
            .unwrap_or(ratio)
    } else {
        SNAP_STOPS
            .iter()
            .rev()
            .copied()
            .find(|&s| s < ratio - EPS)
            .unwrap_or(ratio)
    }
}

fn resize_toward<P>(
    node: &mut Node<P>,
    focus: PaneId,
    axis: Axis,
    adjust: &dyn Fn(f32) -> f32,
) -> (bool, bool) {
    match node {
        Node::Leaf { id, .. } => (*id == focus, false),
        Node::Split {
            axis: node_axis,
            ratio,
            first,
            second,
        } => {
            let (in_first, handled_first) = resize_toward(first, focus, axis, adjust);
            let (in_second, handled_second) = if in_first {
                (false, false)
            } else {
                resize_toward(second, focus, axis, adjust)
            };
            let in_sub = in_first || in_second;
            let handled = handled_first || handled_second;
            if in_sub && !handled && *node_axis == axis {
                *ratio = adjust(*ratio).clamp(0.1, 0.9);
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

    /// Restoring a daemon-persisted layout hands us a tree whose shell leaves are blank (their
    /// PTYs died with the old daemon). Every one gets a fresh terminal; occupied leaves — notably
    /// the agent's primary, whose id is durable — must be left exactly as they are.
    #[test]
    fn fill_blanks_populates_only_empty_leaves() {
        let mut t = PaneTree::new();
        let primary = TerminalId::new();
        t.open(primary);
        t.split(Axis::LeftRight); // a blank pane
        t.split(Axis::TopBottom); // and another

        let filled = t.fill_blanks(TerminalId::new);
        assert_eq!(filled.len(), 2, "one terminal per blank pane");
        assert_ne!(filled[0], filled[1], "each pane gets its own terminal");

        let payloads = t.payloads();
        assert_eq!(payloads.len(), 3);
        assert!(
            payloads.contains(&primary),
            "the primary keeps its terminal: {payloads:?}"
        );
        for t in &filled {
            assert!(payloads.contains(t), "filled terminal {t:?} is in a pane");
        }
    }

    /// A tree with nothing blank is untouched, and a tree with no panes at all is not a crash.
    #[test]
    fn fill_blanks_is_a_no_op_when_nothing_is_empty() {
        let mut t = PaneTree::new();
        assert!(t.fill_blanks(TerminalId::new).is_empty(), "empty tree");

        let a = TerminalId::new();
        t.open(a);
        assert!(t.fill_blanks(TerminalId::new).is_empty(), "nothing blank");
        assert_eq!(t.payloads(), vec![a]);
    }

    /// The round trip that matters: save a split layout, restore it with the shell leaf blanked
    /// (what the daemon does across a restart), refill, and get the same geometry back.
    #[test]
    fn a_restored_layout_refills_its_shell_pane() {
        let mut original = PaneTree::new();
        let primary = TerminalId::new();
        original.open(primary);
        original.split(Axis::LeftRight);
        original.open(TerminalId::new()); // the shell
        let saved = original.to_layout().expect("non-empty");

        // The daemon blanks every non-primary leaf on reload.
        let blanked = match saved {
            Layout::Split {
                axis,
                ratio,
                first,
                second: _,
            } => Layout::Split {
                axis,
                ratio,
                first,
                second: Box::new(Layout::Leaf { terminal: None }),
            },
            other => other,
        };

        let mut restored = PaneTree::<TerminalId>::from_layout(&blanked);
        let filled = restored.fill_blanks(TerminalId::new);
        assert_eq!(filled.len(), 1, "the shell pane needs one new terminal");
        assert_eq!(restored.layout(area()).len(), 2, "still two panes");
        assert!(restored.payloads().contains(&primary));
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

    /// A full-width top pane over three bottom panes (b1 | b2 | b3), built interactively.
    /// With `area()` the bottom rects are b1 x0..50, b2 x50..75, b3 x75..100, so the top pane's
    /// center (x=50) is nearest b2's center — the pane pure center-alignment picks going Down.
    fn top_over_three() -> (PaneTree<TerminalId>, TerminalId, [TerminalId; 3]) {
        let mut t = PaneTree::new();
        let top = TerminalId::new();
        t.open(top);
        t.split(Axis::TopBottom);
        let b1 = TerminalId::new();
        t.open(b1);
        t.split(Axis::LeftRight);
        let b2 = TerminalId::new();
        t.open(b2);
        t.split(Axis::LeftRight);
        let b3 = TerminalId::new();
        t.open(b3);
        (t, top, [b1, b2, b3])
    }

    #[test]
    fn navigate_returns_to_the_most_recently_focused_pane() {
        // Walk to the bottom-left pane, go up to the full-width top pane, come back down:
        // focus must return to bottom-left, not the center-aligned middle pane.
        let (mut t, top, [b1, _, _]) = top_over_three();
        let a = area();
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved); // b3 → b2
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved); // b2 → b1
        assert_eq!(t.focused_payload(), Some(b1));
        assert_eq!(t.navigate(Dir::Up, a), Nav::Moved);
        assert_eq!(t.focused_payload(), Some(top));
        assert_eq!(t.navigate(Dir::Down, a), Nav::Moved);
        assert_eq!(
            t.focused_payload(),
            Some(b1),
            "Down must return where I was"
        );
    }

    #[test]
    fn mouse_focus_counts_for_navigation_recency() {
        // Focus moved by focus_payload (a click) must stamp recency just like navigation.
        let (mut t, top, [b1, _, b3]) = top_over_three();
        let a = area();
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved);
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved);
        assert_eq!(t.focused_payload(), Some(b1));
        assert!(t.focus_payload(b3)); // "click" the bottom-right pane
        assert_eq!(t.navigate(Dir::Up, a), Nav::Moved);
        assert_eq!(t.focused_payload(), Some(top));
        assert_eq!(t.navigate(Dir::Down, a), Nav::Moved);
        assert_eq!(
            t.focused_payload(),
            Some(b3),
            "the click was the last visit"
        );
    }

    #[test]
    fn restored_layout_falls_back_to_center_alignment() {
        // A tree restored from a saved layout has no focus history (only the restored focus
        // itself), so Down from the top pane keeps today's center-alignment choice.
        let (t, top, [_, b2, _]) = top_over_three();
        let saved = t.to_layout().expect("non-empty");
        let mut restored = PaneTree::<TerminalId>::from_layout(&saved);
        let a = area();
        assert_eq!(
            restored.focused_payload(),
            Some(top),
            "first leaf is the top pane"
        );
        assert_eq!(restored.navigate(Dir::Down, a), Nav::Moved);
        assert_eq!(
            restored.focused_payload(),
            Some(b2),
            "no history → center-aligned pane"
        );
    }

    #[test]
    fn focus_most_recent_returns_to_the_last_worked_pane() {
        // Regression: leaving the panes for the sidebar (Nav::ExitLeft) doesn't touch the tree's
        // focus, so coming back must land on the pane last worked in — not the first leaf.
        let (mut t, _top, [b1, _, _]) = top_over_three();
        let a = area();
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved); // b3 → b2
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved); // b2 → b1
        assert_eq!(t.navigate(Dir::Left, a), Nav::ExitLeft); // hand off to the sidebar
        assert_eq!(t.focused_payload(), Some(b1), "exit-left leaves focus put");
        t.focus_most_recent(); // return from the sidebar
        assert_eq!(
            t.focused_payload(),
            Some(b1),
            "back to where I was, not the first pane"
        );
    }

    #[test]
    fn focus_most_recent_falls_back_to_first_leaf_without_history() {
        // A restored layout has no per-pane history beyond its initial focus, so focus_most_recent
        // lands on the first leaf.
        let (t, top, _) = top_over_three();
        let saved = t.to_layout().expect("non-empty");
        let mut restored = PaneTree::<TerminalId>::from_layout(&saved);
        restored.focus_most_recent();
        assert_eq!(restored.focused_payload(), Some(top));
    }

    #[test]
    fn nearer_pane_beats_more_recent_pane() {
        // Three stacked rows: a / m / b. From the top, Down must pick the adjacent middle row
        // even when the bottom row was focused more recently — gap dominates recency.
        let mut t = PaneTree::new();
        let a_pane = TerminalId::new();
        t.open(a_pane);
        t.split(Axis::TopBottom);
        let m = TerminalId::new();
        t.open(m);
        t.split(Axis::TopBottom);
        let b = TerminalId::new();
        t.open(b); // focused last — the most recent stamp
        assert!(t.focus_payload(a_pane));
        let ar = area();
        assert_eq!(t.navigate(Dir::Down, ar), Nav::Moved);
        assert_eq!(
            t.focused_payload(),
            Some(m),
            "adjacent row wins over recency"
        );
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

    #[test]
    fn snap_ratio_ratchets_through_clean_stops() {
        let third = 1.0 / 3.0;
        let two_thirds = 2.0 / 3.0;
        // `grow` (Right/Down) climbs to the next-larger stop; shrink drops to the next-smaller. A
        // press never sticks on the stop it already sits on, off-stop ratios round to the next
        // clean stop, and the 0.1/0.9 clamp bounds are terminal — growing past 0.9 or shrinking
        // past 0.1 stays put.
        let cases = [
            (0.5, true, two_thirds),
            (0.5, false, third),
            (third, true, 0.5),
            (third, false, 0.25),
            (two_thirds, true, 0.75),
            (two_thirds, false, 0.5),
            (0.25, false, 0.1),
            (0.75, true, 0.9),
            (0.9, true, 0.9),
            (0.1, false, 0.1),
            (0.3, true, third),
            (0.3, false, 0.25),
            (0.55, true, two_thirds),
            (0.55, false, 0.5),
        ];
        for (ratio, grow, want) in cases {
            let got = snap_ratio(ratio, grow);
            assert!(
                (got - want).abs() < 1e-6,
                "snap_ratio({ratio}, grow={grow}) = {got}, want {want}"
            );
        }
    }

    #[test]
    fn resize_snap_lands_the_split_on_a_clean_stop() {
        // Width 120 so every stop is an exact integer column (⅓·120 = 40, ⅔·120 = 80).
        let a = Rect::new(0, 0, 120, 40);
        let mut t = PaneTree::new();
        t.open(TerminalId::new());
        t.split(Axis::LeftRight); // focus lands on the right pane (ratio starts at 0.5)
        let focused_width = |t: &PaneTree<TerminalId>| {
            t.layout(a)
                .into_iter()
                .find(|p| p.focused)
                .unwrap()
                .rect
                .width
        };
        assert_eq!(focused_width(&t), 60, "starts at half the row");
        // Left grows the right pane toward its boundary: first child's ratio 0.5 → ⅓, so the
        // right pane goes 60 → 80.
        t.resize_snap(Dir::Left);
        assert_eq!(focused_width(&t), 80, "snapped to two-thirds of the row");
        // Right shrinks it one stop back: ratio ⅓ → 0.5, right pane 80 → 60.
        t.resize_snap(Dir::Right);
        assert_eq!(focused_width(&t), 60, "snapped back to half");
    }

    #[test]
    fn resize_snap_moves_the_nearest_matching_axis_split() {
        // Two nested left/right splits. Snapping horizontally must move the *inner* boundary the
        // focused pane sits against, never the outer one.
        let a = Rect::new(0, 0, 120, 40);
        let mut t = PaneTree::new();
        let left = TerminalId::new();
        t.open(left);
        t.split(Axis::LeftRight); // outer split; the new right region is focused (and blank)
        let right_region = TerminalId::new();
        t.open(right_region); // fill it
        t.split(Axis::LeftRight); // split that region again; the new rightmost pane is focused
        let rightmost = TerminalId::new();
        t.open(rightmost);
        let width = |t: &PaneTree<TerminalId>, p: TerminalId| {
            t.layout(a)
                .into_iter()
                .find(|x| x.payload == Some(p))
                .unwrap()
                .rect
                .width
        };
        assert_eq!(width(&t, left), 60, "outer split starts at half");
        // Snap-Left grows the focused (rightmost) pane against the inner boundary: the inner split
        // goes 0.5 → ⅓, shrinking its first child (`right_region`) to a third of the 60-wide right
        // region. The outer split is left alone.
        t.resize_snap(Dir::Left);
        assert_eq!(width(&t, left), 60, "outer split untouched");
        assert_eq!(
            width(&t, right_region),
            20,
            "inner split snapped to a third of its own slot"
        );
    }
}
