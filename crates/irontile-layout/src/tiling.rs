//! Turning a tree into rectangles.

use serde::{Deserialize, Serialize};

use crate::geom::{Axis, Direction, Rect, Size};
use crate::id::{NodeId, WindowId};
use crate::tree::{Node, Tree};

/// Knobs that affect geometry without affecting tree shape.
///
/// [`Params::default`] is the zero configuration: no gaps and no minimum size.
/// That is what [`apply`] uses, and it is the configuration under which the
/// produced rectangles exactly tile the input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    /// Space between the work area edge and the outermost windows.
    pub outer_gap: i32,
    /// Space between adjacent siblings.
    pub inner_gap: i32,
    /// Floor enforced by [`Tree::resize`]. Deliberately not enforced by
    /// [`apply_with`]: a tree can always hold more windows than a display has
    /// pixels, and refusing to tile in that case is worse than producing thin
    /// rectangles.
    ///
    /// [`Tree::resize`]: crate::Tree::resize
    pub min_window: Size,
}

impl Params {
    pub const ZERO: Params = Params {
        outer_gap: 0,
        inner_gap: 0,
        min_window: Size::ZERO,
    };

    pub const fn with_gaps(outer: i32, inner: i32) -> Params {
        Params {
            outer_gap: outer,
            inner_gap: inner,
            min_window: Size::ZERO,
        }
    }
}

/// Places every window in the tree inside `area`.
///
/// The returned rectangles are pairwise non-overlapping, all contained in
/// `area`, and their areas sum to the area of `area`: they tile it exactly.
/// Order is stable depth-first, matching [`Tree::leaves`].
///
/// [`Tree::leaves`]: crate::Tree::leaves
pub fn apply(tree: &Tree, area: Rect) -> Vec<(WindowId, Rect)> {
    apply_with(tree, area, &Params::ZERO)
}

/// Places every window in the tree inside `area`, honouring `params`.
///
/// The rectangles are always pairwise non-overlapping and contained in `area`.
/// They tile it exactly when `params` has no gaps.
pub fn apply_with(tree: &Tree, area: Rect, params: &Params) -> Vec<(WindowId, Rect)> {
    geometry(tree, area, params)
        .into_iter()
        .filter_map(|(id, rect)| tree.window_at(id).map(|w| (w, rect)))
        .collect()
}

/// Rectangles for every node, containers included, in depth-first order with
/// each parent immediately preceding its children.
///
/// Containers are exposed because the compositor needs them: hit-testing a
/// pointer drag onto a split boundary, and converting a pixel resize into a
/// weight change, both need to know how big the container is.
pub fn geometry(tree: &Tree, area: Rect, params: &Params) -> Vec<(NodeId, Rect)> {
    let mut out = Vec::with_capacity(tree.len() * 2);
    if let Some(root) = tree.root() {
        let area = area.inset(params.outer_gap.max(0));
        walk(tree, root, area, params.inner_gap.max(0), &mut out);
    }
    out
}

fn walk(tree: &Tree, id: NodeId, rect: Rect, gap: i32, out: &mut Vec<(NodeId, Rect)>) {
    out.push((id, rect));
    if let Some(Node::Split(split)) = tree.node(id) {
        let parts = divide(rect, split.axis, &split.weights, gap);
        for (&child, part) in split.children.iter().zip(parts) {
            walk(tree, child, part, gap, out);
        }
    }
}

/// Divides `rect` among `weights` along `axis`, leaving `gap` pixels between
/// neighbours.
///
/// The gaps are taken out of the extent first and the remainder is divided
/// exactly, so the parts stay inside `rect` and never overlap however small it
/// is.
fn divide(rect: Rect, axis: Axis, weights: &[u32], gap: i32) -> Vec<Rect> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    if gap <= 0 {
        return rect.split_weighted(axis, weights);
    }
    let total_gap = gap.saturating_mul(n as i32 - 1);
    let usable = (rect.extent(axis) - total_gap).max(0);
    let shrunk = match axis {
        Axis::Horizontal => Rect::new(rect.x, rect.y, usable, rect.h),
        Axis::Vertical => Rect::new(rect.x, rect.y, rect.w, usable),
    };

    // Lay the parts out with a cursor rather than by shifting each one by a
    // multiple of the gap. When the container is narrower than its gaps alone
    // would need, a fixed shift would push the later parts straight out of the
    // container; a clamped cursor collapses them against the trailing edge
    // instead, which keeps them inside and still disjoint.
    let start = rect.start(axis);
    let limit = start + rect.extent(axis).max(0);
    let mut cursor = start;
    shrunk
        .split_weighted(axis, weights)
        .into_iter()
        .map(|part| {
            let at = cursor.min(limit);
            let len = part.extent(axis).min(limit - at);
            cursor = at.saturating_add(len).saturating_add(gap);
            match axis {
                Axis::Horizontal => Rect::new(at, part.y, len, part.h),
                Axis::Vertical => Rect::new(part.x, at, part.w, len),
            }
        })
        .collect()
}

/// Picks the candidate best reached by moving `dir` away from `origin`.
///
/// Candidates clear of the origin along the axis are preferred; among those,
/// one whose perpendicular span overlaps the origin beats one that does not,
/// then nearest along the axis, then nearest perpendicular centre. Ties break
/// on the key itself so the result is deterministic.
///
/// The fallback pass, which only looks at centres, exists for overlapping
/// rectangles: tiled windows never overlap, but floating ones do.
pub fn pick_direction<T: Copy + Ord>(
    origin: Rect,
    candidates: &[(T, Rect)],
    dir: Direction,
) -> Option<T> {
    let axis = dir.axis();
    let perp = axis.other();

    let clear = |cand: &Rect| {
        if dir.is_forward() {
            cand.start(axis) >= origin.end(axis)
        } else {
            cand.end(axis) <= origin.start(axis)
        }
    };
    let beyond = |cand: &Rect| {
        let (a, b) = (cand.center(), origin.center());
        match axis {
            Axis::Horizontal if dir.is_forward() => a.x > b.x,
            Axis::Horizontal => a.x < b.x,
            Axis::Vertical if dir.is_forward() => a.y > b.y,
            Axis::Vertical => a.y < b.y,
        }
    };

    let score = |cand: &Rect| {
        let gap = if dir.is_forward() {
            cand.start(axis) - origin.end(axis)
        } else {
            origin.start(axis) - cand.end(axis)
        };
        let overlap = origin.overlap_along(*cand, perp);
        let perp_dist = match perp {
            Axis::Horizontal => (cand.center().x - origin.center().x).abs(),
            Axis::Vertical => (cand.center().y - origin.center().y).abs(),
        };
        (
            i64::from(overlap == 0),
            i64::from(gap.max(0)),
            i64::from(perp_dist),
        )
    };

    let best = |filter: &dyn Fn(&Rect) -> bool| {
        candidates
            .iter()
            .filter(|(_, r)| filter(r))
            .min_by_key(|(key, r)| (score(r), *key))
            .map(|(key, _)| *key)
    };

    best(&clear).or_else(|| best(&beyond))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::InsertTarget;

    fn tree_of(n: u64) -> Tree {
        let mut t = Tree::new();
        for i in 0..n {
            t.insert(
                WindowId(i),
                InsertTarget::Focused {
                    axis: Some(Axis::Horizontal),
                },
            )
            .unwrap();
        }
        t
    }

    #[test]
    fn empty_tree_places_nothing() {
        assert!(apply(&Tree::new(), Rect::new(0, 0, 100, 100)).is_empty());
    }

    #[test]
    fn single_window_fills_the_area() {
        let area = Rect::new(4, 8, 1280, 720);
        let placed = apply(&tree_of(1), area);
        assert_eq!(placed, vec![(WindowId(0), area)]);
    }

    #[test]
    fn row_tiles_exactly() {
        let area = Rect::new(0, 0, 1000, 500);
        let placed = apply(&tree_of(3), area);
        assert_eq!(placed.len(), 3);
        assert_eq!(
            placed.iter().map(|(_, r)| r.area()).sum::<i64>(),
            area.area()
        );
        assert!(placed.iter().all(|(_, r)| area.contains_rect(*r)));
    }

    #[test]
    fn gaps_keep_windows_apart_and_inside() {
        let area = Rect::new(0, 0, 1000, 500);
        let params = Params::with_gaps(10, 6);
        let placed = apply_with(&tree_of(4), area, &params);
        assert!(placed.iter().all(|(_, r)| area.contains_rect(*r)));
        for (i, (_, a)) in placed.iter().enumerate() {
            for (_, b) in &placed[i + 1..] {
                assert!(!a.intersects(*b), "{a:?} overlaps {b:?}");
            }
        }
        assert_eq!(placed[0].1.x, 10);
        assert_eq!(placed[0].1.right() + 6, placed[1].1.x);
    }

    #[test]
    fn geometry_lists_containers_before_children() {
        let t = tree_of(2);
        let rects = geometry(&t, Rect::new(0, 0, 100, 100), &Params::ZERO);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].0, t.root().unwrap());
    }

    #[test]
    fn direction_prefers_overlapping_neighbour() {
        let origin = Rect::new(0, 0, 100, 100);
        let candidates = [
            (1u32, Rect::new(100, 0, 100, 100)),
            (2u32, Rect::new(100, 400, 100, 100)),
        ];
        assert_eq!(
            pick_direction(origin, &candidates, Direction::Right),
            Some(1)
        );
        assert_eq!(pick_direction(origin, &candidates, Direction::Left), None);
    }
}
