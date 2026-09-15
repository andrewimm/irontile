//! Appearance and behaviour knobs that belong to the compositor rather than to
//! the layout engine.

use irontile_layout::{Params, Size};

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Drawn as a solid quad behind each window; the window itself is inset by
    /// this much. That is the whole of the decoration.
    pub border_width: i32,
    pub border_focused: Paint,
    pub border_unfocused: Paint,
    pub background: [f32; 4],
    /// Space between adjacent windows.
    pub inner_gap: i32,
    /// Space between the work area edge and the outermost windows.
    pub outer_gap: i32,
    /// Floor a directional resize will not shrink a window past.
    pub min_window: Size,
    /// Pixels per keypress for a directional resize.
    pub resize_step: i32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            border_width: 2,
            border_focused: Paint::solid([0.36, 0.60, 0.84, 1.0]),
            border_unfocused: Paint::solid([0.16, 0.17, 0.20, 1.0]),
            background: [0.07, 0.07, 0.09, 1.0],
            inner_gap: 4,
            outer_gap: 4,
            min_window: Size::new(48, 48),
            resize_step: 40,
        }
    }
}

impl Theme {
    /// Gaps the layout engine should leave. The border is drawn inside a
    /// window's own cell, so it costs no gap of its own.
    pub fn layout_params(&self) -> Params {
        Params {
            outer_gap: self.outer_gap,
            inner_gap: self.inner_gap,
            min_window: self.min_window,
        }
    }
}

/// What a border is painted with: one colour, or a gradient across the window.
///
/// Stops are held straight rather than premultiplied, because that is what the
/// configuration file says and what interpolating between two half-transparent
/// colours has to be done in to look right. [`Paint::at`] premultiplies on the
/// way out, which is what the renderer's blend function expects.
#[derive(Clone, Debug, PartialEq)]
pub struct Paint {
    /// At least one. Evenly spaced from the start of the gradient to its end.
    stops: Vec<[f32; 4]>,
    /// Degrees clockwise from straight up, so 0 runs bottom to top, 90 runs
    /// left to right, and 45 runs from the bottom-left corner to the top-right.
    /// This is the convention CSS uses.
    angle: f32,
}

impl Default for Paint {
    fn default() -> Self {
        Paint::solid([0.0, 0.0, 0.0, 1.0])
    }
}

impl Paint {
    pub fn solid(color: [f32; 4]) -> Self {
        Paint {
            stops: vec![color],
            angle: 0.0,
        }
    }

    /// A gradient, or a solid colour if only one stop is given. No stops at all
    /// is not a colour, so it falls back to a solid black rather than failing
    /// to paint anything at all.
    pub fn gradient(stops: Vec<[f32; 4]>, angle: f32) -> Self {
        if stops.is_empty() {
            return Paint::default();
        }
        Paint { stops, angle }
    }

    pub fn stops(&self) -> &[[f32; 4]] {
        &self.stops
    }

    pub fn angle(&self) -> f32 {
        self.angle
    }

    /// Whether this is a single colour, and so needs no subdivision to draw.
    pub fn is_solid(&self) -> bool {
        self.stops.len() < 2
    }

    /// The premultiplied colour `t` of the way along the gradient, where `t` is
    /// clamped to `0..=1`.
    pub fn at(&self, t: f32) -> [f32; 4] {
        let straight = self.straight_at(t);
        let alpha = straight[3];
        [
            straight[0] * alpha,
            straight[1] * alpha,
            straight[2] * alpha,
            alpha,
        ]
    }

    fn straight_at(&self, t: f32) -> [f32; 4] {
        let last = self.stops.len() - 1;
        if last == 0 {
            return self.stops[0];
        }
        let scaled = t.clamp(0.0, 1.0) * last as f32;
        let lower = (scaled.floor() as usize).min(last - 1);
        let mix = scaled - lower as f32;
        let (a, b) = (self.stops[lower], self.stops[lower + 1]);
        [
            a[0] + (b[0] - a[0]) * mix,
            a[1] + (b[1] - a[1]) * mix,
            a[2] + (b[2] - a[2]) * mix,
            a[3] + (b[3] - a[3]) * mix,
        ]
    }

    /// How far along the gradient a point inside a `w` by `h` box is.
    ///
    /// The gradient runs along the axis the angle names, and is normalized so
    /// that it spans the whole box whatever that angle is: the corners are
    /// always 0 and 1, so a 45 degree gradient reaches its last colour exactly
    /// at the opposite corner rather than partway along an edge.
    pub fn position(&self, x: f32, y: f32, w: f32, h: f32) -> f32 {
        let radians = self.angle.to_radians();
        let (dx, dy) = (radians.sin(), -radians.cos());
        // The projection of the box onto the gradient axis, which is what the
        // extent has to be measured against.
        let span = w * dx.abs() + h * dy.abs();
        if span <= 0.0 {
            return 0.0;
        }
        // Measured from the corner the gradient starts at, so that t is 0
        // there whichever way round the axis points.
        let origin_x = if dx < 0.0 { w } else { 0.0 };
        let origin_y = if dy < 0.0 { h } else { 0.0 };
        (((x - origin_x) * dx + (y - origin_y) * dy) / span).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Paint;

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn a_solid_paint_is_the_same_colour_all_the_way_along() {
        let paint = Paint::solid([0.2, 0.4, 0.6, 1.0]);
        assert!(paint.is_solid());
        assert!(close(paint.at(0.0), paint.at(1.0)));
    }

    #[test]
    fn a_gradient_runs_from_its_first_stop_to_its_last() {
        let paint = Paint::gradient(vec![[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]], 45.0);
        assert!(!paint.is_solid());
        assert!(close(paint.at(0.0), [1.0, 0.0, 0.0, 1.0]));
        assert!(close(paint.at(1.0), [0.0, 0.0, 1.0, 1.0]));
        assert!(close(paint.at(0.5), [0.5, 0.0, 0.5, 1.0]));
    }

    #[test]
    fn three_stops_are_spaced_evenly_and_a_value_past_the_end_is_held() {
        let paint = Paint::gradient(
            vec![
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 0.0, 0.0, 1.0],
                [1.0, 1.0, 1.0, 1.0],
            ],
            0.0,
        );
        assert!(
            close(paint.at(0.5), [1.0, 0.0, 0.0, 1.0]),
            "the middle stop"
        );
        assert!(close(paint.at(2.0), [1.0, 1.0, 1.0, 1.0]), "clamped");
    }

    #[test]
    fn the_angle_says_which_corner_the_gradient_starts_at() {
        // The CSS convention: zero points up, and it turns clockwise.
        let up = Paint::gradient(vec![[0.0; 4], [1.0; 4]], 0.0);
        assert!(up.position(0.0, 100.0, 200.0, 100.0) < 0.001, "the bottom");
        assert!(up.position(0.0, 0.0, 200.0, 100.0) > 0.999, "the top");

        let across = Paint::gradient(vec![[0.0; 4], [1.0; 4]], 90.0);
        assert!(across.position(0.0, 50.0, 200.0, 100.0) < 0.001, "the left");
        assert!(
            across.position(200.0, 50.0, 200.0, 100.0) > 0.999,
            "the right"
        );

        // A diagonal reaches its last colour at the opposite corner rather
        // than partway along an edge, whatever the window's proportions.
        let diagonal = Paint::gradient(vec![[0.0; 4], [1.0; 4]], 45.0);
        assert!(diagonal.position(0.0, 100.0, 200.0, 100.0) < 0.001);
        assert!(diagonal.position(200.0, 0.0, 200.0, 100.0) > 0.999);
    }

    #[test]
    fn alpha_is_premultiplied_on_the_way_out() {
        // The renderer blends with ONE, ONE_MINUS_SRC_ALPHA, so a colour handed
        // over straight would come out far too bright wherever it is not
        // opaque -- and at full alpha the two are identical, so the mistake
        // would only appear once someone wrote a see-through border.
        let paint = Paint::solid([1.0, 0.5, 0.0, 0.5]);
        assert!(close(paint.at(0.0), [0.5, 0.25, 0.0, 0.5]));
    }

    #[test]
    fn interpolation_happens_before_premultiplying() {
        // Mixing premultiplied colours pulls the result toward whichever stop
        // is more opaque, which is not what a gradient between two colours of
        // different transparency should look like.
        let paint = Paint::gradient(vec![[1.0, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 1.0]], 0.0);
        let middle = paint.at(0.5);
        assert!(close(middle, [0.5, 0.0, 0.0, 0.5]), "{middle:?}");
    }

    #[test]
    fn a_gradient_with_no_colours_at_all_is_still_a_colour() {
        // Nothing to paint with is not a reason to leave a window unbordered.
        assert!(Paint::gradient(Vec::new(), 45.0).is_solid());
    }
}
