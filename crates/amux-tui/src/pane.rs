//! A pure binary-space-partition pane tree for the tiled main area: split, close, directional
//! navigate, and resize. No I/O, no rendering — it only computes rectangles. Heavily
//! unit-tested. See `docs/SPLITS.md`.

use amux_core::agent::AgentId;
use ratatui::layout::Rect;

pub type PaneId = u64;

/// How a split divides its space.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Children side by side (a vertical divider) — tmux `%`.
    LeftRight,
    /// Children stacked (a horizontal divider) — tmux `"`.
    TopBottom,
}

/// A movement / resize direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

enum Node {
    Leaf {
        id: PaneId,
        agent: Option<AgentId>,
    },
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// A pane's placement, for rendering and geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub id: PaneId,
    pub agent: Option<AgentId>,
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
pub struct PaneTree {
    root: Option<Node>,
    focus: PaneId,
    next_id: PaneId,
}

impl Default for PaneTree {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneTree {
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

    pub fn focused_agent(&self) -> Option<AgentId> {
        self.root
            .as_ref()
            .and_then(|n| find_agent(n, self.focus))
            .flatten()
    }

    /// Every agent currently shown in a pane.
    pub fn agents(&self) -> Vec<AgentId> {
        let mut out = Vec::new();
        if let Some(n) = &self.root {
            collect_agents(n, &mut out);
        }
        out
    }

    /// Open `agent` into the focused pane (creating the first pane if the tree is empty).
    pub fn open(&mut self, agent: AgentId) {
        match &mut self.root {
            None => {
                let id = self.alloc();
                self.root = Some(Node::Leaf {
                    id,
                    agent: Some(agent),
                });
                self.focus = id;
            }
            Some(node) => set_agent(node, self.focus, Some(agent)),
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
        let focus = self.focus;
        if let Some(root) = self.root.take() {
            let (new_root, sibling_focus) = close_leaf(root, focus);
            self.root = new_root;
            if let Some(id) = sibling_focus {
                self.focus = id;
            } else if let Some(node) = &self.root {
                self.focus = first_leaf(node);
            }
        }
    }

    /// Clear any pane showing `agent` (e.g. it was deleted), leaving those panes empty.
    pub fn remove_agent(&mut self, agent: AgentId) {
        if let Some(node) = &mut self.root {
            clear_agent(node, agent);
        }
    }

    pub fn focus_first(&mut self) {
        if let Some(node) = &self.root {
            self.focus = first_leaf(node);
        }
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

    /// Grow/shrink the focused pane in `dir` by `step` (0.0–1.0), adjusting the nearest ancestor
    /// split of the matching axis.
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
    pub fn layout(&self, area: Rect) -> Vec<Placement> {
        let mut out = Vec::new();
        if let Some(node) = &self.root {
            layout_node(node, area, self.focus, &mut out);
        }
        out
    }
}

// --- tree helpers ---

fn find_agent(node: &Node, id: PaneId) -> Option<Option<AgentId>> {
    match node {
        Node::Leaf { id: lid, agent } if *lid == id => Some(*agent),
        Node::Leaf { .. } => None,
        Node::Split { first, second, .. } => {
            find_agent(first, id).or_else(|| find_agent(second, id))
        }
    }
}

fn collect_agents(node: &Node, out: &mut Vec<AgentId>) {
    match node {
        Node::Leaf { agent, .. } => {
            if let Some(a) = agent {
                out.push(*a);
            }
        }
        Node::Split { first, second, .. } => {
            collect_agents(first, out);
            collect_agents(second, out);
        }
    }
}

fn set_agent(node: &mut Node, id: PaneId, agent: Option<AgentId>) {
    match node {
        Node::Leaf { id: lid, agent: a } if *lid == id => *a = agent,
        Node::Leaf { .. } => {}
        Node::Split { first, second, .. } => {
            set_agent(first, id, agent);
            set_agent(second, id, agent);
        }
    }
}

fn clear_agent(node: &mut Node, agent: AgentId) {
    match node {
        Node::Leaf { agent: a, .. } => {
            if *a == Some(agent) {
                *a = None;
            }
        }
        Node::Split { first, second, .. } => {
            clear_agent(first, agent);
            clear_agent(second, agent);
        }
    }
}

fn split_leaf(node: Node, target: PaneId, axis: Axis, new_id: PaneId) -> Node {
    match node {
        Node::Leaf { id, agent } if id == target => Node::Split {
            axis,
            ratio: 0.5,
            first: Box::new(Node::Leaf { id, agent }),
            second: Box::new(Node::Leaf {
                id: new_id,
                agent: None,
            }),
        },
        Node::Leaf { .. } => node,
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => Node::Split {
            axis,
            ratio,
            first: Box::new(split_leaf(*first, target, axis, new_id)),
            second: Box::new(split_leaf(*second, target, axis, new_id)),
        },
    }
}

/// Returns the (possibly-collapsed) subtree and, if a leaf was removed here, the sibling's focus.
fn close_leaf(node: Node, target: PaneId) -> (Option<Node>, Option<PaneId>) {
    match node {
        Node::Leaf { id, .. } if id == target => (None, None),
        Node::Leaf { .. } => (Some(node), None),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            // Direct child is the target → collapse to the sibling.
            if matches!(&*first, Node::Leaf { id, .. } if *id == target) {
                let focus = first_leaf(&second);
                return (Some(*second), Some(focus));
            }
            if matches!(&*second, Node::Leaf { id, .. } if *id == target) {
                let focus = first_leaf(&first);
                return (Some(*first), Some(focus));
            }
            // Recurse.
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

fn first_leaf(node: &Node) -> PaneId {
    match node {
        Node::Leaf { id, .. } => *id,
        Node::Split { first, .. } => first_leaf(first),
    }
}

/// Adjust the nearest matching-axis ancestor of the focused leaf. Returns
/// `(focus_in_subtree, already_handled)`.
fn resize_toward(
    node: &mut Node,
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
                // Grow the focused child: +step if it's `first`, -step if `second`.
                let delta = if grow == in_first { step } else { -step };
                *ratio = (*ratio + delta).clamp(0.1, 0.9);
                return (true, true);
            }
            (in_sub, handled)
        }
    }
}

fn layout_node(node: &Node, rect: Rect, focus: PaneId, out: &mut Vec<Placement>) {
    match node {
        Node::Leaf { id, agent } => out.push(Placement {
            id: *id,
            agent: *agent,
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
            let w1 = ((rect.width as f32 * ratio).round() as u16).clamp(1, rect.width.max(1));
            let w1 = w1.min(rect.width.saturating_sub(1)).max(1.min(rect.width));
            (
                Rect::new(rect.x, rect.y, w1, rect.height),
                Rect::new(rect.x + w1, rect.y, rect.width - w1, rect.height),
            )
        }
        Axis::TopBottom => {
            let h1 = ((rect.height as f32 * ratio).round() as u16).clamp(1, rect.height.max(1));
            let h1 = h1
                .min(rect.height.saturating_sub(1))
                .max(1.min(rect.height));
            (
                Rect::new(rect.x, rect.y, rect.width, h1),
                Rect::new(rect.x, rect.y + h1, rect.width, rect.height - h1),
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

    fn area() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn open_into_empty_creates_one_focused_pane() {
        let mut t = PaneTree::new();
        assert!(t.is_empty());
        let a = AgentId::new();
        t.open(a);
        assert!(!t.is_empty());
        assert_eq!(t.focused_agent(), Some(a));
        assert_eq!(t.agents(), vec![a]);
        assert_eq!(t.layout(area()).len(), 1);
    }

    #[test]
    fn split_makes_two_panes_new_one_empty_and_focused() {
        let mut t = PaneTree::new();
        let a = AgentId::new();
        t.open(a);
        t.split(Axis::LeftRight);
        let places = t.layout(area());
        assert_eq!(places.len(), 2);
        assert_eq!(t.focused_agent(), None); // new pane is empty
        assert_eq!(t.agents(), vec![a]); // only the original has an agent
        let b = AgentId::new();
        t.open(b); // fill the focused (new) pane
        assert_eq!(t.focused_agent(), Some(b));
        assert_eq!(t.agents().len(), 2);
    }

    #[test]
    fn close_collapses_back_to_sibling() {
        let mut t = PaneTree::new();
        let a = AgentId::new();
        t.open(a);
        t.split(Axis::LeftRight); // focus on new empty pane
        t.close(); // close the empty one → back to `a`
        assert_eq!(t.layout(area()).len(), 1);
        assert_eq!(t.focused_agent(), Some(a));
        t.close(); // close the last one → empty
        assert!(t.is_empty());
    }

    #[test]
    fn navigate_moves_across_panes_and_exits_left() {
        let mut t = PaneTree::new();
        t.open(AgentId::new()); // pane 1 (left)
        t.split(Axis::LeftRight); // pane 2 (right), focused
        let a = area();
        // Focused is the right pane; moving left lands on the left pane.
        assert_eq!(t.navigate(Dir::Left, a), Nav::Moved);
        // Moving left again — no pane further left → exit to the sidebar.
        assert_eq!(t.navigate(Dir::Left, a), Nav::ExitLeft);
        // Back right onto the second pane.
        assert_eq!(t.navigate(Dir::Right, a), Nav::Moved);
        // Nothing above.
        assert_eq!(t.navigate(Dir::Up, a), Nav::Stay);
    }

    #[test]
    fn resize_changes_pane_widths() {
        let mut t = PaneTree::new();
        t.open(AgentId::new());
        t.split(Axis::LeftRight); // focus = right pane, ratio 0.5
        let a = area();
        let right_before = t
            .layout(a)
            .into_iter()
            .find(|p| p.focused)
            .unwrap()
            .rect
            .width;
        // Grow the focused (right) pane's width.
        t.resize(Dir::Right, 0.1);
        let right_after = t
            .layout(a)
            .into_iter()
            .find(|p| p.focused)
            .unwrap()
            .rect
            .width;
        assert!(
            right_after > right_before,
            "focused pane should widen ({right_before} → {right_after})"
        );
    }
}
