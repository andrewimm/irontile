//! Integer geometry primitives.
//!
//! Every coordinate in this crate is an integer in the global logical pixel
//! space. There are no floats anywhere in the layout state, which is what makes
//! "the produced rectangles exactly tile the input rectangle" an exact
//! property rather than one that holds up to some epsilon, and what keeps the
//! state bit-for-bit reproducible across a host/guest boundary.

use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The direction along which a container arranges its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Children are placed side by side along x.
    Horizontal,
    /// Children are stacked along y.
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

impl Size {
    pub const ZERO: Self = Self { w: 0, h: 0 };

    pub const fn new(w: i32, h: i32) -> Self {
        Self { w, h }
    }

    pub const fn along(&self, axis: Axis) -> i32 {
        match axis {
            Axis::Horizontal => self.w,
            Axis::Vertical => self.h,
        }
    }
}

impl Axis {
    pub const fn other(self) -> Self {
        match self {
            Axis::Horizontal => Axis::Vertical,
            Axis::Vertical => Axis::Horizontal,
        }
    }
}

impl Direction {
    pub const fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Horizontal,
            Direction::Up | Direction::Down => Axis::Vertical,
        }
    }

    /// True for the directions that increase a coordinate, so that a child at a
    /// higher index within a container lies in the "forward" direction.
    pub const fn is_forward(self) -> bool {
        matches!(self, Direction::Right | Direction::Down)
    }

    pub const fn opposite(self) -> Self {
        match self {
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
        }
    }

    pub const ALL: [Direction; 4] = [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ];
}

impl Rect {
    pub const ZERO: Self = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn from_parts(origin: Point, size: Size) -> Self {
        Self {
            x: origin.x,
            y: origin.y,
            w: size.w,
            h: size.h,
        }
    }

    pub const fn origin(&self) -> Point {
        Point::new(self.x, self.y)
    }

    pub const fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }

    /// Exclusive right edge.
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// Exclusive bottom edge.
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub const fn area(&self) -> i64 {
        if self.w <= 0 || self.h <= 0 {
            0
        } else {
            (self.w as i64) * (self.h as i64)
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Extent along `axis`.
    pub const fn extent(&self, axis: Axis) -> i32 {
        match axis {
            Axis::Horizontal => self.w,
            Axis::Vertical => self.h,
        }
    }

    /// Position of the leading edge along `axis`.
    pub const fn start(&self, axis: Axis) -> i32 {
        match axis {
            Axis::Horizontal => self.x,
            Axis::Vertical => self.y,
        }
    }

    /// Position of the (exclusive) trailing edge along `axis`.
    pub const fn end(&self, axis: Axis) -> i32 {
        self.start(axis) + self.extent(axis)
    }

    pub const fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    pub const fn contains_rect(&self, other: Rect) -> bool {
        if other.is_empty() {
            // A degenerate rectangle is contained as long as its origin is not
            // outside us; it covers no area to fall out of bounds.
            return other.x >= self.x
                && other.y >= self.y
                && other.x <= self.right()
                && other.y <= self.bottom();
        }
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// True only when the rectangles share positive area. Degenerate
    /// rectangles never intersect anything, which keeps the non-overlap
    /// invariant meaningful when a container is too small to give every child
    /// a nonzero share.
    pub const fn intersects(&self, other: Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn intersection(&self, other: Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    /// Length of the overlap of the two rectangles' projections onto `axis`.
    pub fn overlap_along(&self, other: Rect, axis: Axis) -> i32 {
        let start = self.start(axis).max(other.start(axis));
        let end = self.end(axis).min(other.end(axis));
        (end - start).max(0)
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn inset(&self, by: i32) -> Rect {
        self.inset_by(by, by, by, by)
    }

    /// Shrinks by the given amounts on each edge. Negative amounts grow.
    ///
    /// When the insets exceed the extent the result is empty, and its origin is
    /// pulled back to the far edge rather than past it, so a degenerate result
    /// is still contained in the original.
    pub fn inset_by(&self, top: i32, right: i32, bottom: i32, left: i32) -> Rect {
        let mut x = self.x + left;
        let mut y = self.y + top;
        if left > 0 {
            x = x.min(self.right().max(self.x));
        }
        if top > 0 {
            y = y.min(self.bottom().max(self.y));
        }
        Rect::new(
            x,
            y,
            (self.w - left - right).max(0),
            (self.h - top - bottom).max(0),
        )
    }

    /// Divides this rectangle along `axis` in proportion to `weights`.
    ///
    /// The returned parts are contiguous and sum back to exactly this
    /// rectangle: boundaries are computed as truncated cumulative fractions of
    /// the full extent, so rounding error accumulates into at most one pixel
    /// per boundary and never escapes the total. All-zero weights are treated
    /// as equal weights.
    pub fn split_weighted(&self, axis: Axis, weights: &[u32]) -> Vec<Rect> {
        let n = weights.len();
        if n == 0 {
            return Vec::new();
        }
        let extent = i128::from(self.extent(axis).max(0));
        let total: i128 = weights.iter().map(|&w| i128::from(w)).sum();
        let uniform = total == 0;

        let mut out = Vec::with_capacity(n);
        let mut acc: i128 = 0;
        let mut prev: i32 = 0;
        for (i, &w) in weights.iter().enumerate() {
            acc += if uniform { 1 } else { i128::from(w) };
            let next = if i + 1 == n {
                // Pin the final boundary so the parts sum exactly, regardless of
                // how the intermediate divisions truncated.
                self.extent(axis).max(0)
            } else {
                let denom = if uniform { n as i128 } else { total };
                (extent * acc / denom) as i32
            };
            let len = next - prev;
            out.push(match axis {
                Axis::Horizontal => Rect::new(self.x + prev, self.y, len, self.h),
                Axis::Vertical => Rect::new(self.x, self.y + prev, self.w, len),
            });
            prev = next;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_is_exact_and_contiguous() {
        let area = Rect::new(0, 0, 1000, 600);
        let parts = area.split_weighted(Axis::Horizontal, &[1, 1, 1]);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.iter().map(|r| r.w).sum::<i32>(), 1000);
        assert_eq!(parts[0].right(), parts[1].x);
        assert_eq!(parts[1].right(), parts[2].x);
        assert_eq!(parts[2].right(), area.right());
    }

    #[test]
    fn split_honours_weights() {
        let area = Rect::new(10, 20, 900, 100);
        let parts = area.split_weighted(Axis::Horizontal, &[2, 1]);
        assert_eq!(parts[0], Rect::new(10, 20, 600, 100));
        assert_eq!(parts[1], Rect::new(610, 20, 300, 100));
    }

    #[test]
    fn split_survives_more_children_than_pixels() {
        let area = Rect::new(0, 0, 3, 50);
        let parts = area.split_weighted(Axis::Horizontal, &[1; 7]);
        assert_eq!(parts.iter().map(|r| r.w).sum::<i32>(), 3);
        assert!(parts.iter().all(|r| r.w >= 0));
    }

    #[test]
    fn zero_weights_split_evenly() {
        let area = Rect::new(0, 0, 100, 10);
        let parts = area.split_weighted(Axis::Horizontal, &[0, 0]);
        assert_eq!(parts[0].w, 50);
        assert_eq!(parts[1].w, 50);
    }

    #[test]
    fn an_oversized_inset_stays_inside() {
        let area = Rect::new(0, 0, 1, 1);
        let inset = area.inset(2);
        assert!(inset.is_empty());
        assert!(area.contains_rect(inset), "{inset:?} escapes {area:?}");
    }

    #[test]
    fn degenerate_rects_do_not_intersect() {
        let a = Rect::new(0, 0, 0, 100);
        let b = Rect::new(0, 0, 100, 100);
        assert!(!a.intersects(b));
        assert!(b.intersects(Rect::new(50, 50, 10, 10)));
    }
}
