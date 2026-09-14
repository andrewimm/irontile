//! The split-container tree.
//!
//! One tree describes the tiling arrangement of a single desktop. It knows
//! nothing about displays, floating windows, or focus policy; those live a
//! layer up. What it guarantees is that the windows it holds can always be
//! turned into a set of rectangles that exactly tile any rectangle you hand it.
//!
//! Two shape invariants are maintained eagerly rather than checked lazily,
//! because they are what keep that guarantee cheap:
//!
//! - every container holds at least two children, so a removal that would leave
//!   a container with one child collapses the container instead;
//! - child sizes are integer weights, never ratios, so dividing a rectangle is
//!   exact arithmetic with no accumulated drift.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{LayoutError, TreeInvariant};
use crate::geom::{Axis, Direction, Rect};
use crate::id::{NodeId, WindowId};
use crate::tiling::{Params, geometry, pick_direction};

/// The share handed to each child of a freshly created container.
///
/// Large enough that resizing has fine granularity without needing to rescale,
/// small enough that sums stay far from overflow even in deep trees.
pub const DEFAULT_WEIGHT: u32 = 1 << 16;

/// The floor a child's weight is clamped to. Zero would make a child
/// unrecoverable, since no resize could ever give it space back.
pub const MIN_WEIGHT: u32 = 1;

/// A single window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaf {
    pub window: WindowId,
}

/// A container that divides its rectangle among its children along one axis.
///
/// `children` and `weights` are parallel and always the same length. The fields
/// are public because this type is part of the serialized state, but a tree is
/// only ever mutated through [`Tree`]'s methods; a tree obtained by
/// deserialization should be passed through [`Tree::validate`] before use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Split {
    pub axis: Axis,
    pub children: Vec<NodeId>,
    pub weights: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Node {
    Leaf(Leaf),
    Split(Split),
}

impl Node {
    pub fn is_leaf(&self) -> bool {
        matches!(self, Node::Leaf(_))
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Node::Split(_))
    }

    pub fn as_leaf(&self) -> Option<&Leaf> {
        match self {
            Node::Leaf(l) => Some(l),
            Node::Split(_) => None,
        }
    }

    pub fn as_split(&self) -> Option<&Split> {
        match self {
            Node::Split(s) => Some(s),
            Node::Leaf(_) => None,
        }
    }

    pub fn window(&self) -> Option<WindowId> {
        self.as_leaf().map(|l| l.window)
    }
}

/// Where a new window should land.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InsertTarget {
    /// Relative to the focused node.
    ///
    /// `axis: None` means "become a sibling in whatever container the focus
    /// already lives in", which produces a row or column. `axis: Some(a)` means
    /// "split the focused leaf along `a`", except that when the focus already
    /// sits in a container with that axis the new window joins it rather than
    /// adding a redundant level of nesting.
    Focused { axis: Option<Axis> },
    /// Adjacent to a specific node, on the given side.
    Beside { of: NodeId, dir: Direction },
    /// At a specific index within a specific container.
    Into { parent: NodeId, index: usize },
    /// Relative to the root, with the same axis semantics as `Focused`.
    Root { axis: Option<Axis> },
}

impl Default for InsertTarget {
    fn default() -> Self {
        InsertTarget::Focused { axis: None }
    }
}

/// The outcome of removing a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    /// The (now dead) handle of the removed leaf.
    pub node: NodeId,
    /// Where focus landed, if anywhere.
    pub new_focus: Option<NodeId>,
}

/// A detached subtree, carrying no handles.
///
/// This is how a whole container moves between desktops or displays without
/// being flattened into loose windows on the way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subtree {
    Leaf {
        window: WindowId,
    },
    Split {
        axis: Axis,
        children: Vec<Subtree>,
        weights: Vec<u32>,
    },
}

impl Subtree {
    pub fn leaf(window: WindowId) -> Self {
        Subtree::Leaf { window }
    }

    /// Every window in the subtree, in depth-first order.
    pub fn windows(&self) -> Vec<WindowId> {
        let mut out = Vec::new();
        self.collect_windows(&mut out);
        out
    }

    fn collect_windows(&self, out: &mut Vec<WindowId>) {
        match self {
            Subtree::Leaf { window } => out.push(*window),
            Subtree::Split { children, .. } => {
                for c in children {
                    c.collect_windows(out);
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    parent: Option<NodeId>,
    node: Node,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Slot {
    generation: u32,
    entry: Option<Entry>,
}

/// The tiling arrangement of one desktop.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    slots: Vec<Slot>,
    free: Vec<u32>,
    root: Option<NodeId>,
    focus: Option<NodeId>,
    /// Reverse index from window to leaf. A `BTreeMap` rather than a hash map so
    /// that serialization and iteration order are deterministic.
    windows: BTreeMap<WindowId, NodeId>,
}

/// An insertion site resolved against the current tree shape.
enum Site {
    AsRoot,
    Into {
        parent: NodeId,
        index: usize,
    },
    Wrap {
        target: NodeId,
        axis: Axis,
        before: bool,
    },
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- queries -------------------------------------------------------

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Number of windows in the tree.
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.entry(id).map(|e| &e.node)
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.entry(id).and_then(|e| e.parent)
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        match self.entry(id).map(|e| &e.node) {
            Some(Node::Split(s)) => &s.children,
            _ => &[],
        }
    }

    pub fn weights(&self, id: NodeId) -> &[u32] {
        match self.entry(id).map(|e| &e.node) {
            Some(Node::Split(s)) => &s.weights,
            _ => &[],
        }
    }

    pub fn axis(&self, id: NodeId) -> Option<Axis> {
        self.split(id).map(|s| s.axis)
    }

    pub fn window_at(&self, id: NodeId) -> Option<WindowId> {
        self.node(id).and_then(Node::window)
    }

    pub fn node_of(&self, window: WindowId) -> Option<NodeId> {
        self.windows.get(&window).copied()
    }

    pub fn contains(&self, window: WindowId) -> bool {
        self.windows.contains_key(&window)
    }

    /// Every window, in stable depth-first order.
    pub fn windows(&self) -> impl Iterator<Item = (NodeId, WindowId)> + '_ {
        self.leaves().into_iter()
    }

    /// Depth-first list of leaves. The order is the one [`crate::apply`]
    /// produces, so tests can compare whole vectors.
    pub fn leaves(&self) -> Vec<(NodeId, WindowId)> {
        let mut out = Vec::with_capacity(self.windows.len());
        if let Some(root) = self.root {
            self.collect_leaves(root, &mut out);
        }
        out
    }

    fn collect_leaves(&self, id: NodeId, out: &mut Vec<(NodeId, WindowId)>) {
        match self.node(id) {
            Some(Node::Leaf(l)) => out.push((id, l.window)),
            Some(Node::Split(s)) => {
                for &c in &s.children {
                    self.collect_leaves(c, out);
                }
            }
            None => {}
        }
    }

    /// The leftmost leaf of the subtree rooted at `id`.
    pub fn first_leaf(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(Node::Split(s)) = self.node(cur) {
            match s.children.first() {
                Some(&c) => cur = c,
                None => break,
            }
        }
        cur
    }

    pub fn is_ancestor(&self, ancestor: NodeId, of: NodeId) -> bool {
        let mut cur = self.parent(of);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.parent(c);
        }
        false
    }

    // ---- focus ---------------------------------------------------------

    pub fn focus(&self) -> Option<NodeId> {
        self.focus
    }

    pub fn focused_window(&self) -> Option<WindowId> {
        self.focus.and_then(|f| self.window_at(f))
    }

    pub fn set_focus(&mut self, id: NodeId) -> Result<(), LayoutError> {
        self.check(id)?;
        self.focus = Some(id);
        Ok(())
    }

    pub fn clear_focus(&mut self) {
        self.focus = None;
    }

    /// The container holding the focused node, for operations that act on a
    /// whole group rather than one window.
    pub fn focus_parent(&self) -> Option<NodeId> {
        self.focus.and_then(|f| self.parent(f))
    }

    /// The leaf reached by moving `dir` from `from`, chosen geometrically:
    /// among the leaves lying that way, the nearest one whose perpendicular
    /// span overlaps the origin wins.
    pub fn neighbor(
        &self,
        from: NodeId,
        dir: Direction,
        area: Rect,
        params: &Params,
    ) -> Option<NodeId> {
        let rects = geometry(self, area, params);
        let origin = rects.iter().find(|(id, _)| *id == from).map(|(_, r)| *r)?;
        let candidates: Vec<(NodeId, Rect)> = rects
            .into_iter()
            .filter(|(id, _)| *id != from && self.node(*id).is_some_and(Node::is_leaf))
            .collect();
        pick_direction(origin, &candidates, dir)
    }

    // ---- structural mutation -------------------------------------------

    /// Inserts `window` and gives it focus.
    ///
    /// Focus moves because a newly mapped window taking focus is the
    /// overwhelmingly common case; a caller with a different policy overrides
    /// it with [`Tree::set_focus`] afterwards.
    pub fn insert(&mut self, window: WindowId, at: InsertTarget) -> Result<NodeId, LayoutError> {
        if self.windows.contains_key(&window) {
            return Err(LayoutError::WindowAlreadyManaged(window));
        }
        // Resolve before allocating, so a rejected insert leaves no trace.
        let site = self.resolve(at)?;
        let leaf = self.alloc(Node::Leaf(Leaf { window }), None);
        self.windows.insert(window, leaf);
        self.place(leaf, site);
        self.focus = Some(leaf);
        Ok(leaf)
    }

    pub fn remove(&mut self, window: WindowId) -> Result<Removed, LayoutError> {
        let id = self
            .windows
            .get(&window)
            .copied()
            .ok_or(LayoutError::UnknownWindow(window))?;
        let replacement = self.sibling_leaf(id);
        self.windows.remove(&window);
        self.unlink(id);
        self.dealloc(id);
        if self.focus.is_none_or(|f| !self.is_live(f)) {
            self.focus = replacement.filter(|&f| self.is_live(f));
        }
        Ok(Removed {
            node: id,
            new_focus: self.focus,
        })
    }

    /// Moves a node one step in `dir`.
    ///
    /// Within a container the node trades places with its neighbour, carrying
    /// its weight along so the window keeps its size. When it is already at the
    /// edge of every ancestor that runs along `dir`, it escapes outward into
    /// the nearest ancestor that does. Returns [`LayoutError::AtEdge`] when the
    /// move would leave the tree, which is the signal a caller with an output
    /// arrangement uses to hand the window to the next display.
    pub fn move_node(&mut self, id: NodeId, dir: Direction) -> Result<(), LayoutError> {
        self.check(id)?;
        let axis = dir.axis();
        let forward = dir.is_forward();

        let mut child = id;
        let plan = loop {
            let Some(parent) = self.parent(child) else {
                return Err(LayoutError::AtEdge(dir));
            };
            let split = self.split(parent).expect("a node's parent is a container");
            if split.axis == axis {
                let i = index_of(&split.children, child);
                if child == id {
                    // Still in the node's own container: try a plain reorder.
                    if forward && i + 1 < split.children.len() {
                        break Plan::Swap {
                            parent,
                            a: i,
                            b: i + 1,
                        };
                    }
                    if !forward && i > 0 {
                        break Plan::Swap {
                            parent,
                            a: i,
                            b: i - 1,
                        };
                    }
                    // At the edge of this container; keep climbing.
                } else {
                    // We climbed past at least one container, so the node
                    // escapes to sit beside the ancestor it came from.
                    break Plan::Reparent {
                        container: parent,
                        index: i,
                    };
                }
            }
            child = parent;
        };

        match plan {
            Plan::Swap { parent, a, b } => {
                let split = self.split_mut(parent).expect("container");
                split.children.swap(a, b);
                split.weights.swap(a, b);
            }
            Plan::Reparent { container, index } => {
                // `container` is a strict ancestor of the node's parent, so it
                // keeps at least two children across the unlink and its child
                // at `index` is only ever replaced in place by a collapse.
                self.unlink(id);
                let weight = self.fair_weight(container);
                let at = if forward { index + 1 } else { index };
                self.link(container, at, id, weight);
            }
        }
        Ok(())
    }

    /// Exchanges the positions of two nodes. Each adopts the other's weight, so
    /// the two windows trade sizes as well as places and no rescaling is needed
    /// when the containers use different weight magnitudes.
    pub fn swap(&mut self, a: NodeId, b: NodeId) -> Result<(), LayoutError> {
        self.check(a)?;
        self.check(b)?;
        if a == b {
            return Ok(());
        }
        if self.is_ancestor(a, b) || self.is_ancestor(b, a) {
            return Err(LayoutError::OverlappingSubtrees(a, b));
        }
        let (pa, pb) = (self.parent(a), self.parent(b));
        // Both indices are read before either write: when the two nodes share a
        // parent, the first write would otherwise make `b` findable at `a`'s old
        // index and the second would undo it.
        let ia = pa.map(|p| index_of(self.children(p), a));
        let ib = pb.map(|p| index_of(self.children(p), b));
        match (pa, ia) {
            (Some(p), Some(i)) => self.split_mut(p).expect("container").children[i] = b,
            _ => self.root = Some(b),
        }
        match (pb, ib) {
            (Some(p), Some(i)) => self.split_mut(p).expect("container").children[i] = a,
            _ => self.root = Some(a),
        }
        self.set_parent(a, pb);
        self.set_parent(b, pa);
        Ok(())
    }

    /// Detaches a subtree, leaving the tree valid behind it.
    pub fn detach(&mut self, id: NodeId) -> Result<Subtree, LayoutError> {
        self.check(id)?;
        let sub = self.to_subtree(id);
        let replacement = self.sibling_leaf(id);
        for w in sub.windows() {
            self.windows.remove(&w);
        }
        self.unlink(id);
        self.free_subtree(id);
        if self.focus.is_none_or(|f| !self.is_live(f)) {
            self.focus = replacement.filter(|&f| self.is_live(f));
        }
        Ok(sub)
    }

    /// Reattaches a detached subtree and focuses its first window.
    pub fn attach(&mut self, sub: Subtree, at: InsertTarget) -> Result<NodeId, LayoutError> {
        for w in sub.windows() {
            if self.windows.contains_key(&w) {
                return Err(LayoutError::WindowAlreadyManaged(w));
            }
        }
        let site = self.resolve(at)?;
        let node = self.build(&sub, None);
        self.place(node, site);
        self.focus = Some(self.first_leaf(node));
        Ok(node)
    }

    // ---- sizing ---------------------------------------------------------

    /// Grows the node's share by `delta_px` on its `dir` edge, taking the space
    /// from the neighbour on that side.
    ///
    /// The adjustment is applied at the nearest ancestor that actually runs
    /// along `dir` and has a neighbour there, matching what a user means by
    /// "make this wider" when the window is nested several levels deep.
    /// `within` is the rectangle the tree is currently laid out in; it is what
    /// turns a pixel delta into a weight delta.
    pub fn resize(
        &mut self,
        id: NodeId,
        dir: Direction,
        delta_px: i32,
        within: Rect,
        params: &Params,
    ) -> Result<(), LayoutError> {
        self.check(id)?;
        let axis = dir.axis();
        let forward = dir.is_forward();

        let mut child = id;
        let (container, grow, shrink) = loop {
            let Some(parent) = self.parent(child) else {
                return Err(LayoutError::AtEdge(dir));
            };
            let split = self.split(parent).expect("container");
            if split.axis == axis {
                let i = index_of(&split.children, child);
                if forward && i + 1 < split.children.len() {
                    break (parent, i, i + 1);
                }
                if !forward && i > 0 {
                    break (parent, i, i - 1);
                }
            }
            child = parent;
        };

        if delta_px == 0 {
            return Ok(());
        }
        let rects = geometry(self, within, params);
        let container_extent = rects
            .iter()
            .find(|(n, _)| *n == container)
            .map(|(_, r)| r.extent(axis))
            .unwrap_or(0);

        let split = self.split(container).expect("container");
        let n = split.children.len();
        // Gaps are carved out before the remainder is divided, so they are not
        // part of what the weights apportion.
        let gaps = params.inner_gap.max(0).saturating_mul(n as i32 - 1);
        let extent = i128::from((container_extent - gaps).max(0));
        if extent <= 0 {
            return Ok(());
        }
        let total: i128 = split.weights.iter().map(|&w| i128::from(w)).sum();

        // Work in boundary pixels rather than in weights. The shared edge of
        // the two children sits at a truncated cumulative fraction of the
        // extent, so solving for the weight that puts the edge exactly where it
        // was asked to go makes a resize pixel-exact in both directions;
        // adjusting the weights directly loses a pixel to truncation.
        let lo = grow.min(shrink);
        let cum: i128 = split.weights[..=lo].iter().map(|&w| i128::from(w)).sum();
        let edge = (extent * cum / total) as i32;
        let shift = if forward { delta_px } else { -delta_px };
        let target = (edge.saturating_add(shift)).clamp(0, extent as i32);
        let cum_target = ceil_div128(i128::from(target) * total, extent);
        let delta = if forward {
            cum_target - cum
        } else {
            cum - cum_target
        };

        // Translate the configured minimum window size into the same weight
        // space, so neither side can be resized out of existence.
        let min_px = i128::from(params.min_window.along(axis).max(0));
        let floor = ceil_div128(min_px * total, extent).max(i128::from(MIN_WEIGHT));

        let grow_w = i128::from(split.weights[grow]);
        let shrink_w = i128::from(split.weights[shrink]);
        let lower = floor - grow_w;
        let upper = shrink_w - floor;
        if lower > upper {
            // One of the two is already under the floor; refuse to make it worse.
            return Ok(());
        }
        let delta = delta.clamp(lower, upper);
        if delta == 0 {
            return Ok(());
        }

        let split = self.split_mut(container).expect("container");
        split.weights[grow] = clamp_weight(grow_w + delta);
        split.weights[shrink] = clamp_weight(shrink_w - delta);
        Ok(())
    }

    pub fn set_weights(&mut self, container: NodeId, weights: Vec<u32>) -> Result<(), LayoutError> {
        self.check(container)?;
        let split = self
            .split(container)
            .ok_or(LayoutError::NotAContainer(container))?;
        if split.children.len() != weights.len() {
            return Err(LayoutError::IndexOutOfRange);
        }
        let split = self.split_mut(container).expect("container");
        split.weights = weights.into_iter().map(|w| w.max(MIN_WEIGHT)).collect();
        Ok(())
    }

    /// Gives every child of the container an equal share.
    pub fn equalize(&mut self, container: NodeId) -> Result<(), LayoutError> {
        self.check(container)?;
        let split = self
            .split_mut(container)
            .ok_or(LayoutError::NotAContainer(container))?;
        split.weights.iter_mut().for_each(|w| *w = DEFAULT_WEIGHT);
        Ok(())
    }

    pub fn set_axis(&mut self, container: NodeId, axis: Axis) -> Result<(), LayoutError> {
        self.check(container)?;
        let split = self
            .split_mut(container)
            .ok_or(LayoutError::NotAContainer(container))?;
        split.axis = axis;
        Ok(())
    }

    pub fn toggle_axis(&mut self, container: NodeId) -> Result<(), LayoutError> {
        self.check(container)?;
        let split = self
            .split_mut(container)
            .ok_or(LayoutError::NotAContainer(container))?;
        split.axis = split.axis.other();
        Ok(())
    }

    /// Splices a container's children into its parent, distributing the
    /// container's own share among them. Dissolving the root is a no-op, since
    /// there is nowhere for the children to go.
    pub fn dissolve(&mut self, container: NodeId) -> Result<(), LayoutError> {
        self.check(container)?;
        if self.split(container).is_none() {
            return Err(LayoutError::NotAContainer(container));
        }
        let Some(parent) = self.parent(container) else {
            return Ok(());
        };
        let (children, weights) = {
            let s = self.split(container).expect("container");
            (s.children.clone(), s.weights.clone())
        };
        let at = index_of(self.children(parent), container);
        let share = i128::from(self.weights(parent)[at]);
        let total: i128 = weights.iter().map(|&w| i128::from(w)).sum::<i128>().max(1);

        {
            let s = self.split_mut(parent).expect("container");
            s.children.remove(at);
            s.weights.remove(at);
            for (offset, (&c, &w)) in children.iter().zip(weights.iter()).enumerate() {
                let scaled = ((share * i128::from(w) / total) as u32).max(MIN_WEIGHT);
                s.children.insert(at + offset, c);
                s.weights.insert(at + offset, scaled);
            }
        }
        for &c in &children {
            self.set_parent(c, Some(parent));
        }
        if self.focus == Some(container) {
            self.focus = children.first().copied();
        }
        if let Some(s) = self.split_mut(container) {
            s.children.clear();
            s.weights.clear();
        }
        self.dealloc(container);
        Ok(())
    }

    // ---- validation -----------------------------------------------------

    /// Checks every structural invariant.
    ///
    /// Property tests call this after each operation; it is also the gate a
    /// deserialized tree should pass before being trusted.
    pub fn validate(&self) -> Result<(), TreeInvariant> {
        let live: Vec<NodeId> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.entry.is_some())
            .map(|(i, s)| NodeId::new(i as u32, s.generation))
            .collect();

        let root = match self.root {
            None => {
                if !live.is_empty() {
                    return Err(TreeInvariant::BadRoot(None));
                }
                if let Some((w, _)) = self.windows.iter().next() {
                    return Err(TreeInvariant::WindowIndexMismatch(*w));
                }
                if let Some(f) = self.focus {
                    return Err(TreeInvariant::BadFocus(f));
                }
                return Ok(());
            }
            Some(r) if !self.is_live(r) => return Err(TreeInvariant::BadRoot(Some(r))),
            Some(r) => r,
        };

        let mut seen: BTreeSet<NodeId> = BTreeSet::new();
        let mut leaves: BTreeMap<WindowId, NodeId> = BTreeMap::new();
        let mut stack = vec![(root, None::<NodeId>)];
        while let Some((id, expected_parent)) = stack.pop() {
            if !seen.insert(id) {
                return Err(TreeInvariant::MultipleParents(id));
            }
            let entry = self.entry(id).ok_or(TreeInvariant::Orphan(id))?;
            if entry.parent != expected_parent {
                return Err(TreeInvariant::ParentMismatch {
                    node: id,
                    recorded: entry.parent,
                    actual: expected_parent,
                });
            }
            match &entry.node {
                Node::Leaf(l) => {
                    if leaves.insert(l.window, id).is_some() {
                        return Err(TreeInvariant::DuplicateWindow(l.window));
                    }
                }
                Node::Split(s) => {
                    if s.children.len() != s.weights.len() {
                        return Err(TreeInvariant::WeightArity {
                            container: id,
                            children: s.children.len(),
                            weights: s.weights.len(),
                        });
                    }
                    if s.children.len() < 2 {
                        return Err(TreeInvariant::UndersizedContainer {
                            container: id,
                            children: s.children.len(),
                        });
                    }
                    if let Some(i) = s.weights.iter().position(|&w| w == 0) {
                        return Err(TreeInvariant::ZeroWeight {
                            container: id,
                            index: i,
                        });
                    }
                    for &c in &s.children {
                        if !self.is_live(c) {
                            return Err(TreeInvariant::DanglingChild {
                                parent: id,
                                child: c,
                            });
                        }
                        stack.push((c, Some(id)));
                    }
                }
            }
        }

        if let Some(&orphan) = live.iter().find(|id| !seen.contains(id)) {
            return Err(TreeInvariant::Orphan(orphan));
        }
        if leaves != self.windows {
            let mismatch = leaves
                .keys()
                .find(|w| self.windows.get(w) != leaves.get(w))
                .or_else(|| self.windows.keys().find(|w| !leaves.contains_key(w)))
                .copied()
                .expect("unequal maps differ somewhere");
            return Err(TreeInvariant::WindowIndexMismatch(mismatch));
        }
        if let Some(f) = self.focus
            && !self.is_live(f)
        {
            return Err(TreeInvariant::BadFocus(f));
        }
        Ok(())
    }

    // ---- internals ------------------------------------------------------

    fn entry(&self, id: NodeId) -> Option<&Entry> {
        let slot = self.slots.get(id.index as usize)?;
        (slot.generation == id.generation).then_some(slot.entry.as_ref())?
    }

    fn entry_mut(&mut self, id: NodeId) -> Option<&mut Entry> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.entry.as_mut()
    }

    fn is_live(&self, id: NodeId) -> bool {
        self.entry(id).is_some()
    }

    fn check(&self, id: NodeId) -> Result<(), LayoutError> {
        self.is_live(id)
            .then_some(())
            .ok_or(LayoutError::StaleNode(id))
    }

    fn split(&self, id: NodeId) -> Option<&Split> {
        self.entry(id).and_then(|e| e.node.as_split())
    }

    fn split_mut(&mut self, id: NodeId) -> Option<&mut Split> {
        match self.entry_mut(id).map(|e| &mut e.node) {
            Some(Node::Split(s)) => Some(s),
            _ => None,
        }
    }

    fn set_parent(&mut self, id: NodeId, parent: Option<NodeId>) {
        if let Some(e) = self.entry_mut(id) {
            e.parent = parent;
        }
    }

    fn alloc(&mut self, node: Node, parent: Option<NodeId>) -> NodeId {
        let entry = Some(Entry { parent, node });
        match self.free.pop() {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                slot.generation = slot.generation.wrapping_add(1);
                slot.entry = entry;
                NodeId::new(index, slot.generation)
            }
            None => {
                let index = u32::try_from(self.slots.len()).expect("node arena fits in u32");
                self.slots.push(Slot {
                    generation: 0,
                    entry,
                });
                NodeId::new(index, 0)
            }
        }
    }

    fn dealloc(&mut self, id: NodeId) {
        if let Some(slot) = self.slots.get_mut(id.index as usize)
            && slot.generation == id.generation
        {
            slot.entry = None;
            self.free.push(id.index);
        }
    }

    fn free_subtree(&mut self, id: NodeId) {
        let children: Vec<NodeId> = self.children(id).to_vec();
        for c in children {
            self.free_subtree(c);
        }
        if let Some(w) = self.window_at(id) {
            self.windows.remove(&w);
        }
        self.dealloc(id);
    }

    /// The share a node joining `container` should get: the average of what is
    /// already there, so it ends up with roughly its fair fraction and the
    /// existing children shrink proportionally.
    fn fair_weight(&self, container: NodeId) -> u32 {
        let weights = self.weights(container);
        if weights.is_empty() {
            return DEFAULT_WEIGHT;
        }
        let total: u64 = weights.iter().map(|&w| u64::from(w)).sum();
        ((total / weights.len() as u64) as u32).max(MIN_WEIGHT)
    }

    fn link(&mut self, parent: NodeId, index: usize, child: NodeId, weight: u32) {
        if let Some(split) = self.split_mut(parent) {
            let index = index.min(split.children.len());
            split.children.insert(index, child);
            split.weights.insert(index, weight.max(MIN_WEIGHT));
        }
        self.set_parent(child, Some(parent));
    }

    /// Removes `id` from its parent, collapsing a container left with a single
    /// child. The node itself stays allocated and detached.
    fn unlink(&mut self, id: NodeId) {
        let Some(parent) = self.parent(id) else {
            if self.root == Some(id) {
                self.root = None;
                if self.focus == Some(id) {
                    self.focus = None;
                }
            }
            return;
        };
        let remaining = {
            let split = self
                .split_mut(parent)
                .expect("a node's parent is a container");
            let i = index_of(&split.children, id);
            split.children.remove(i);
            split.weights.remove(i);
            split.children.len()
        };
        self.set_parent(id, None);
        if remaining == 1 {
            self.collapse(parent);
        }
    }

    /// Replaces a single-child container with that child, in place.
    fn collapse(&mut self, container: NodeId) {
        let child = self.children(container)[0];
        let grandparent = self.parent(container);
        match grandparent {
            Some(g) => {
                let i = index_of(self.children(g), container);
                // The child inherits the container's weight slot, so the rest of
                // the grandparent's layout is undisturbed.
                self.split_mut(g).expect("container").children[i] = child;
            }
            None => self.root = Some(child),
        }
        self.set_parent(child, grandparent);
        if self.focus == Some(container) {
            self.focus = Some(child);
        }
        if let Some(s) = self.split_mut(container) {
            s.children.clear();
            s.weights.clear();
        }
        self.dealloc(container);
    }

    /// A leaf that survives the removal of `id` and sits next to where it was.
    fn sibling_leaf(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.parent(id)?;
        let children = self.children(parent);
        let i = children.iter().position(|&c| c == id)?;
        let sibling = children
            .get(i + 1)
            .or_else(|| i.checked_sub(1).and_then(|j| children.get(j)))?;
        Some(self.first_leaf(*sibling))
    }

    fn resolve(&self, at: InsertTarget) -> Result<Site, LayoutError> {
        let Some(root) = self.root else {
            return Ok(Site::AsRoot);
        };
        match at {
            InsertTarget::Root { axis } => self.resolve_anchor(root, axis),
            InsertTarget::Focused { axis } => {
                let anchor = self.focus.filter(|&f| self.is_live(f)).unwrap_or(root);
                self.resolve_anchor(anchor, axis)
            }
            InsertTarget::Beside { of, dir } => {
                self.check(of)?;
                let axis = dir.axis();
                let before = !dir.is_forward();
                if let Some(p) = self.parent(of) {
                    let split = self.split(p).expect("container");
                    if split.axis == axis {
                        let i = index_of(&split.children, of);
                        let index = if before { i } else { i + 1 };
                        return Ok(Site::Into { parent: p, index });
                    }
                }
                Ok(Site::Wrap {
                    target: of,
                    axis,
                    before,
                })
            }
            InsertTarget::Into { parent, index } => {
                self.check(parent)?;
                let split = self
                    .split(parent)
                    .ok_or(LayoutError::NotAContainer(parent))?;
                if index > split.children.len() {
                    return Err(LayoutError::IndexOutOfRange);
                }
                Ok(Site::Into { parent, index })
            }
        }
    }

    fn resolve_anchor(&self, anchor: NodeId, axis: Option<Axis>) -> Result<Site, LayoutError> {
        let node = self.node(anchor).ok_or(LayoutError::StaleNode(anchor))?;
        if let Node::Split(s) = node {
            // An explicit axis is a request about splitting a window; aimed at a
            // container it just means "join this group".
            return Ok(Site::Into {
                parent: anchor,
                index: s.children.len(),
            });
        }
        match (self.parent(anchor), axis) {
            (Some(p), None) => {
                let i = index_of(self.children(p), anchor);
                Ok(Site::Into {
                    parent: p,
                    index: i + 1,
                })
            }
            (Some(p), Some(a)) => {
                let split = self.split(p).expect("container");
                if split.axis == a {
                    // Nesting a split inside a container of the same axis would
                    // add a level that renders identically.
                    let i = index_of(&split.children, anchor);
                    Ok(Site::Into {
                        parent: p,
                        index: i + 1,
                    })
                } else {
                    Ok(Site::Wrap {
                        target: anchor,
                        axis: a,
                        before: false,
                    })
                }
            }
            // A lone root leaf has no container to join, so one has to be made.
            // Callers that care about the axis pass it explicitly.
            (None, axis) => Ok(Site::Wrap {
                target: anchor,
                axis: axis.unwrap_or(Axis::Horizontal),
                before: false,
            }),
        }
    }

    fn place(&mut self, node: NodeId, site: Site) {
        match site {
            Site::AsRoot => {
                self.root = Some(node);
                self.set_parent(node, None);
            }
            Site::Into { parent, index } => {
                let w = self.fair_weight(parent);
                self.link(parent, index, node, w);
            }
            Site::Wrap {
                target,
                axis,
                before,
            } => {
                self.wrap(target, axis, node, before);
            }
        }
    }

    fn wrap(&mut self, target: NodeId, axis: Axis, node: NodeId, before: bool) -> NodeId {
        let parent = self.parent(target);
        let children = if before {
            vec![node, target]
        } else {
            vec![target, node]
        };
        let container = self.alloc(
            Node::Split(Split {
                axis,
                children,
                weights: vec![DEFAULT_WEIGHT; 2],
            }),
            parent,
        );
        match parent {
            Some(p) => {
                let i = index_of(self.children(p), target);
                // The new container takes over the target's weight slot.
                self.split_mut(p).expect("container").children[i] = container;
            }
            None => self.root = Some(container),
        }
        self.set_parent(target, Some(container));
        self.set_parent(node, Some(container));
        container
    }

    fn to_subtree(&self, id: NodeId) -> Subtree {
        match self.node(id) {
            Some(Node::Leaf(l)) => Subtree::Leaf { window: l.window },
            Some(Node::Split(s)) => Subtree::Split {
                axis: s.axis,
                children: s.children.iter().map(|&c| self.to_subtree(c)).collect(),
                weights: s.weights.clone(),
            },
            // Unreachable for a live handle; a dead one degrades to an empty
            // container rather than panicking inside a query.
            None => Subtree::Split {
                axis: Axis::Horizontal,
                children: Vec::new(),
                weights: Vec::new(),
            },
        }
    }

    fn build(&mut self, sub: &Subtree, parent: Option<NodeId>) -> NodeId {
        match sub {
            Subtree::Leaf { window } => {
                let id = self.alloc(Node::Leaf(Leaf { window: *window }), parent);
                self.windows.insert(*window, id);
                id
            }
            Subtree::Split {
                axis,
                children,
                weights,
            } => {
                let id = self.alloc(
                    Node::Split(Split {
                        axis: *axis,
                        children: Vec::new(),
                        weights: Vec::new(),
                    }),
                    parent,
                );
                let built: Vec<NodeId> = children.iter().map(|c| self.build(c, Some(id))).collect();
                let mut weights: Vec<u32> = weights.iter().map(|&w| w.max(MIN_WEIGHT)).collect();
                weights.resize(built.len(), DEFAULT_WEIGHT);
                let s = self.split_mut(id).expect("container");
                s.children = built;
                s.weights = weights;
                id
            }
        }
    }
}

enum Plan {
    Swap { parent: NodeId, a: usize, b: usize },
    Reparent { container: NodeId, index: usize },
}

/// Integer division rounding up. `denominator` is always positive here.
fn ceil_div128(numerator: i128, denominator: i128) -> i128 {
    if denominator <= 0 {
        return 0;
    }
    (numerator + denominator - 1) / denominator
}

fn clamp_weight(w: i128) -> u32 {
    w.clamp(i128::from(MIN_WEIGHT), i128::from(u32::MAX)) as u32
}

fn index_of(children: &[NodeId], id: NodeId) -> usize {
    children
        .iter()
        .position(|&c| c == id)
        .expect("a linked node is listed by its parent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiling::apply;

    fn w(n: u64) -> WindowId {
        WindowId(n)
    }

    /// A row of `n` windows, all siblings of one horizontal container.
    fn row(n: u64) -> Tree {
        let mut t = Tree::new();
        for i in 0..n {
            t.insert(
                w(i),
                InsertTarget::Focused {
                    axis: Some(Axis::Horizontal),
                },
            )
            .unwrap();
        }
        t.validate().unwrap();
        t
    }

    fn order(t: &Tree) -> Vec<u64> {
        t.leaves().into_iter().map(|(_, w)| w.0).collect()
    }

    #[test]
    fn first_window_becomes_the_root_leaf() {
        let t = row(1);
        assert_eq!(t.len(), 1);
        assert!(t.node(t.root().unwrap()).unwrap().is_leaf());
        assert_eq!(t.focused_window(), Some(w(0)));
    }

    #[test]
    fn same_axis_inserts_do_not_nest() {
        let t = row(4);
        let root = t.root().unwrap();
        // All four are siblings rather than a right-leaning chain of splits.
        assert_eq!(t.children(root).len(), 4);
        assert_eq!(order(&t), vec![0, 1, 2, 3]);
    }

    #[test]
    fn alternating_axes_nest() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(2),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        t.validate().unwrap();
        let root = t.root().unwrap();
        assert_eq!(t.axis(root), Some(Axis::Horizontal));
        assert_eq!(t.children(root).len(), 2);
        let nested = t.children(root)[1];
        assert_eq!(t.axis(nested), Some(Axis::Vertical));
        assert_eq!(order(&t), vec![0, 1, 2]);
    }

    #[test]
    fn no_axis_appends_to_the_focused_container() {
        let mut t = row(2);
        t.insert(w(9), InsertTarget::Focused { axis: None })
            .unwrap();
        t.validate().unwrap();
        assert_eq!(t.children(t.root().unwrap()).len(), 3);
        assert_eq!(order(&t), vec![0, 1, 9]);
    }

    #[test]
    fn insert_beside_respects_the_side() {
        let mut t = row(2);
        let first = t.node_of(w(0)).unwrap();
        t.insert(
            w(7),
            InsertTarget::Beside {
                of: first,
                dir: Direction::Left,
            },
        )
        .unwrap();
        t.validate().unwrap();
        assert_eq!(order(&t), vec![7, 0, 1]);
    }

    #[test]
    fn duplicate_insert_is_rejected_without_side_effects() {
        let mut t = row(2);
        let before = t.clone();
        assert_eq!(
            t.insert(w(0), InsertTarget::default()),
            Err(LayoutError::WindowAlreadyManaged(w(0)))
        );
        assert_eq!(t, before);
    }

    #[test]
    fn rejected_insert_leaves_no_orphan() {
        let mut t = row(2);
        let stale = t.node_of(w(1)).unwrap();
        t.remove(w(1)).unwrap();
        assert!(
            t.insert(
                w(5),
                InsertTarget::Beside {
                    of: stale,
                    dir: Direction::Up
                }
            )
            .is_err()
        );
        t.validate().unwrap();
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn removing_the_last_window_empties_the_tree() {
        let mut t = row(1);
        t.remove(w(0)).unwrap();
        t.validate().unwrap();
        assert!(t.is_empty());
        assert_eq!(t.focus(), None);
    }

    #[test]
    fn removal_collapses_a_single_child_container() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(2),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        t.remove(w(2)).unwrap();
        t.validate().unwrap();
        // The vertical container that held 1 and 2 is gone entirely.
        let root = t.root().unwrap();
        assert_eq!(t.children(root).len(), 2);
        assert!(
            t.children(root)
                .iter()
                .all(|&c| t.node(c).unwrap().is_leaf())
        );
    }

    #[test]
    fn removal_moves_focus_to_a_neighbour() {
        let mut t = row(3);
        let removed = t.remove(w(1)).unwrap();
        assert_eq!(removed.new_focus, t.node_of(w(2)));
        t.validate().unwrap();
    }

    #[test]
    fn removal_does_not_disturb_unrelated_focus() {
        let mut t = row(3);
        t.set_focus(t.node_of(w(0)).unwrap()).unwrap();
        t.remove(w(2)).unwrap();
        assert_eq!(t.focused_window(), Some(w(0)));
    }

    #[test]
    fn move_reorders_within_a_container() {
        let mut t = row(3);
        let node = t.node_of(w(0)).unwrap();
        t.move_node(node, Direction::Right).unwrap();
        t.validate().unwrap();
        assert_eq!(order(&t), vec![1, 0, 2]);
    }

    #[test]
    fn move_carries_the_weight_along() {
        let mut t = row(2);
        let root = t.root().unwrap();
        t.set_weights(root, vec![3 * DEFAULT_WEIGHT, DEFAULT_WEIGHT])
            .unwrap();
        let wide = apply(&t, Rect::new(0, 0, 800, 100))[0].1.w;
        let node = t.node_of(w(0)).unwrap();
        t.move_node(node, Direction::Right).unwrap();
        // Window 0 is now on the right but keeps the share it had.
        let placed = apply(&t, Rect::new(0, 0, 800, 100));
        assert_eq!(placed[1].0, w(0));
        assert_eq!(placed[1].1.w, wide);
    }

    #[test]
    fn move_at_the_edge_reports_at_edge() {
        let mut t = row(2);
        let node = t.node_of(w(0)).unwrap();
        assert_eq!(
            t.move_node(node, Direction::Left),
            Err(LayoutError::AtEdge(Direction::Left))
        );
        // A perpendicular move out of a single container is equally impossible.
        assert_eq!(
            t.move_node(node, Direction::Up),
            Err(LayoutError::AtEdge(Direction::Up))
        );
    }

    #[test]
    fn move_escapes_a_nested_container() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(2),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        // 2 sits below 1 inside a vertical container; pushing it right must
        // lift it out into the horizontal root.
        let node = t.node_of(w(2)).unwrap();
        t.move_node(node, Direction::Right).unwrap();
        t.validate().unwrap();
        assert_eq!(order(&t), vec![0, 1, 2]);
        let root = t.root().unwrap();
        assert_eq!(t.children(root).len(), 3);
    }

    #[test]
    fn swap_exchanges_places() {
        let mut t = row(3);
        let (a, b) = (t.node_of(w(0)).unwrap(), t.node_of(w(2)).unwrap());
        t.swap(a, b).unwrap();
        t.validate().unwrap();
        assert_eq!(order(&t), vec![2, 1, 0]);
    }

    #[test]
    fn swap_refuses_nested_nodes() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        let root = t.root().unwrap();
        let leaf = t.node_of(w(0)).unwrap();
        assert!(matches!(
            t.swap(root, leaf),
            Err(LayoutError::OverlappingSubtrees(_, _))
        ));
    }

    #[test]
    fn resize_moves_space_between_neighbours() {
        let mut t = row(2);
        let area = Rect::new(0, 0, 1000, 100);
        let node = t.node_of(w(0)).unwrap();
        t.resize(node, Direction::Right, 100, area, &Params::ZERO)
            .unwrap();
        t.validate().unwrap();
        let placed = apply(&t, area);
        assert_eq!(placed[0].1.w, 600);
        assert_eq!(placed[1].1.w, 400);
        assert_eq!(placed[0].1.w + placed[1].1.w, 1000);
    }

    #[test]
    fn resize_applies_at_the_nearest_matching_ancestor() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(2),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        let area = Rect::new(0, 0, 1000, 400);
        // Window 2 is in a vertical container, so widening it has to act on the
        // horizontal root two levels up.
        let node = t.node_of(w(2)).unwrap();
        t.resize(node, Direction::Left, 100, area, &Params::ZERO)
            .unwrap();
        let placed = apply(&t, area);
        assert_eq!(placed[0].1.w, 400);
        assert_eq!(placed[2].1.w, 600);
    }

    #[test]
    fn resize_respects_the_minimum_window_size() {
        let mut t = row(2);
        let area = Rect::new(0, 0, 1000, 100);
        let params = Params {
            min_window: crate::geom::Size::new(300, 0),
            ..Params::ZERO
        };
        let node = t.node_of(w(0)).unwrap();
        t.resize(node, Direction::Right, 900, area, &params)
            .unwrap();
        let placed = apply(&t, area);
        assert!(
            placed[1].1.w >= 300,
            "neighbour shrank to {}",
            placed[1].1.w
        );
    }

    #[test]
    fn resize_at_the_edge_reports_at_edge() {
        let mut t = row(2);
        let node = t.node_of(w(0)).unwrap();
        assert_eq!(
            t.resize(
                node,
                Direction::Left,
                50,
                Rect::new(0, 0, 100, 100),
                &Params::ZERO
            ),
            Err(LayoutError::AtEdge(Direction::Left))
        );
    }

    #[test]
    fn detach_and_attach_preserve_the_subtree() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(2),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        let nested = t.parent(t.node_of(w(2)).unwrap()).unwrap();

        let sub = t.detach(nested).unwrap();
        t.validate().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(sub.windows(), vec![w(1), w(2)]);

        let mut other = Tree::new();
        other.insert(w(8), InsertTarget::default()).unwrap();
        other
            .attach(
                sub,
                InsertTarget::Root {
                    axis: Some(Axis::Horizontal),
                },
            )
            .unwrap();
        other.validate().unwrap();
        assert_eq!(order(&other), vec![8, 1, 2]);
    }

    #[test]
    fn dissolve_splices_children_into_the_parent() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(2),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        let nested = t.parent(t.node_of(w(2)).unwrap()).unwrap();
        t.dissolve(nested).unwrap();
        t.validate().unwrap();
        assert_eq!(t.children(t.root().unwrap()).len(), 3);
        assert_eq!(order(&t), vec![0, 1, 2]);
    }

    #[test]
    fn stale_handles_are_rejected_rather_than_reused() {
        let mut t = row(2);
        let stale = t.node_of(w(1)).unwrap();
        t.remove(w(1)).unwrap();
        // Refill the arena so the freed slot is occupied by something else.
        t.insert(
            w(5),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        assert_eq!(t.set_focus(stale), Err(LayoutError::StaleNode(stale)));
    }

    #[test]
    fn neighbor_follows_geometry() {
        let mut t = Tree::new();
        t.insert(
            w(0),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(1),
            InsertTarget::Focused {
                axis: Some(Axis::Horizontal),
            },
        )
        .unwrap();
        t.insert(
            w(2),
            InsertTarget::Focused {
                axis: Some(Axis::Vertical),
            },
        )
        .unwrap();
        let area = Rect::new(0, 0, 1000, 1000);
        let zero = t.node_of(w(0)).unwrap();
        assert_eq!(
            t.neighbor(zero, Direction::Right, area, &Params::ZERO),
            t.node_of(w(1))
        );
        assert_eq!(t.neighbor(zero, Direction::Left, area, &Params::ZERO), None);
        let one = t.node_of(w(1)).unwrap();
        assert_eq!(
            t.neighbor(one, Direction::Down, area, &Params::ZERO),
            t.node_of(w(2))
        );
    }

    #[test]
    fn a_tree_round_trips_through_json() {
        let t = row(5);
        let encoded = serde_json::to_string(&t).unwrap();
        let decoded: Tree = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded, t);
    }
}
