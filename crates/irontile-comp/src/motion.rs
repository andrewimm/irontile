//! Where a window is drawn, which is not always where the tree put it.
//!
//! The layout engine answers one question -- which cell does this window own --
//! and it answers it instantly. Nothing here changes that: the tree is still
//! authoritative, the client is still told its destination size straight away,
//! and hit testing, the control socket and every layout test go on reading the
//! cell rather than whatever is currently on screen.
//!
//! What this adds is a second, slower answer for the renderer alone. A window
//! that has just been given a new cell keeps being drawn near its old one for a
//! few frames, catching up under a spring. Keeping the two apart is what stops
//! motion leaking into the layout: a moving window is a drawing detail, and a
//! compositor where "where is this window" has a different answer sixteen times
//! a second is a compositor whose layout cannot be reasoned about.

use std::time::Duration;

use irontile_layout::Rect;

use crate::config::OpenStyle;
use irontile_motion::{Animated, Motion};

/// A rectangle on its way to another rectangle.
#[derive(Debug)]
pub struct AnimatedRect {
    x: Animated,
    y: Animated,
    w: Animated,
    h: Animated,
    /// False until the tree has placed this window for the first time.
    ///
    /// Without this a new window would animate in from the origin, sliding in
    /// from the top-left corner of the display on its way to its cell, which is
    /// not an entrance anybody asked for. The first placement is taken as fact;
    /// every one after it is a journey.
    placed: bool,
}

impl Default for AnimatedRect {
    fn default() -> Self {
        AnimatedRect {
            x: Animated::pixels(0.0),
            y: Animated::pixels(0.0),
            w: Animated::pixels(0.0),
            h: Animated::pixels(0.0),
            placed: false,
        }
    }
}

impl AnimatedRect {
    /// Sends the rectangle towards `rect`, or puts it there outright if this is
    /// the first the window has heard of where it belongs.
    pub fn retarget(&mut self, rect: Rect) {
        if !self.placed {
            self.snap(rect);
            return;
        }
        self.x.retarget(rect.x as f32);
        self.y.retarget(rect.y as f32);
        self.w.retarget(rect.w as f32);
        self.h.retarget(rect.h as f32);
    }

    /// Puts the rectangle where it belongs immediately, cancelling any flight.
    ///
    /// Used for a window's first placement, when animation is switched off, and
    /// whenever the session goes away underneath us -- a VT switch should not
    /// be something you return from to find windows still sliding.
    pub fn snap(&mut self, rect: Rect) {
        self.x.snap(rect.x as f32);
        self.y.snap(rect.y as f32);
        self.w.snap(rect.w as f32);
        self.h.snap(rect.h as f32);
        self.placed = true;
    }

    /// Where to draw it this frame.
    ///
    /// Sizes are never allowed below one pixel: a loose spring shrinking a
    /// window passes through zero on its way to overshooting, and a zero-sized
    /// element is at best nothing to look at and at worst an invalid buffer.
    pub fn now(&self) -> Rect {
        Rect::new(
            self.x.value().round() as i32,
            self.y.value().round() as i32,
            (self.w.value().round() as i32).max(1),
            (self.h.value().round() as i32).max(1),
        )
    }

    /// Where it is heading, which is not where it is.
    ///
    /// What a carried window is moved by: each step of the drag is a delta on
    /// the destination rather than on the position, so the spring goes on
    /// trailing the pointer instead of being dragged rigidly by it.
    pub fn target(&self) -> Rect {
        Rect::new(
            self.x.target().round() as i32,
            self.y.target().round() as i32,
            (self.w.target().round() as i32).max(1),
            (self.h.target().round() as i32).max(1),
        )
    }

    /// Whether the tree has ever said where this window goes.
    ///
    /// False for a window that has appeared but not yet been placed, which is
    /// what tells an arrival from a move.
    pub fn placed(&self) -> bool {
        self.placed
    }

    /// Advances by `dt`. Returns whether anything is still moving.
    pub fn step(&mut self, motion: &Motion, dt: Duration) -> bool {
        // Every edge is stepped, not just the ones still moving, because `step`
        // on a settled value is a branch and a return.
        let x = self.x.step(motion, dt);
        let y = self.y.step(motion, dt);
        let w = self.w.step(motion, dt);
        let h = self.h.step(motion, dt);
        x || y || w || h
    }

    pub fn moving(&self) -> bool {
        self.x.moving() || self.y.moving() || self.w.moving() || self.h.moving()
    }

    /// How fast it is travelling sideways and down, in pixels per second.
    ///
    /// Nothing reads this yet. It is what a window leaning into the direction it
    /// is being thrown would be drawn from.
    pub fn velocity(&self) -> (f32, f32) {
        (self.x.velocity(), self.y.velocity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spring() -> Motion {
        Motion::spring(0.3, 0.8)
    }

    /// Runs until settled, or gives up. Returns the frames it took.
    fn settle(rect: &mut AnimatedRect, motion: &Motion) -> usize {
        let mut frames = 0;
        while rect.step(motion, Duration::from_millis(16)) && frames < 600 {
            frames += 1;
        }
        frames
    }

    #[test]
    fn a_window_appears_where_it_belongs_rather_than_flying_in() {
        let mut rect = AnimatedRect::default();
        rect.retarget(Rect::new(100, 200, 800, 600));
        // The first placement is not a journey: nothing should be moving, and it
        // should already be drawable at the right place.
        assert!(!rect.moving());
        assert_eq!(rect.now(), Rect::new(100, 200, 800, 600));
    }

    #[test]
    fn a_later_move_travels_and_arrives_exactly() {
        let mut rect = AnimatedRect::default();
        rect.retarget(Rect::new(0, 0, 400, 400));
        rect.retarget(Rect::new(500, 300, 800, 600));
        assert!(rect.moving());
        assert_ne!(rect.now(), Rect::new(500, 300, 800, 600));
        let frames = settle(&mut rect, &spring());
        assert!(frames > 1, "should have taken more than one frame");
        // Arriving *exactly* matters: a window that settles half a pixel out
        // draws a seam against its neighbour forever.
        assert_eq!(rect.now(), Rect::new(500, 300, 800, 600));
        assert!(!rect.moving());
    }

    #[test]
    fn a_shrinking_window_never_reports_an_impossible_size() {
        // A loose spring undershoots on the way down; the clamp in `now` is what
        // keeps that from becoming a zero-sized element.
        let loose = Motion::spring(0.2, 0.4);
        let mut rect = AnimatedRect::default();
        rect.retarget(Rect::new(0, 0, 1200, 900));
        rect.retarget(Rect::new(0, 0, 20, 20));
        let mut frames = 0;
        while rect.step(&loose, Duration::from_millis(16)) && frames < 600 {
            let now = rect.now();
            assert!(now.w >= 1 && now.h >= 1, "impossible size {now:?}");
            frames += 1;
        }
    }

    #[test]
    fn snapping_abandons_a_flight_in_progress() {
        let mut rect = AnimatedRect::default();
        rect.retarget(Rect::new(0, 0, 400, 400));
        rect.retarget(Rect::new(900, 900, 400, 400));
        rect.step(&spring(), Duration::from_millis(16));
        assert!(rect.moving());
        rect.snap(Rect::new(10, 10, 400, 400));
        assert!(!rect.moving());
        assert_eq!(rect.now(), Rect::new(10, 10, 400, 400));
    }

    #[test]
    fn being_told_the_same_cell_again_does_not_restart_anything() {
        // The layout is recomputed and reapplied on every commit, so a window
        // standing still is told where it belongs many times a second.
        let curve = Motion::bezier(200, [0.33, 1.0, 0.68, 1.0]);
        let mut rect = AnimatedRect::default();
        rect.retarget(Rect::new(0, 0, 400, 400));
        rect.retarget(Rect::new(800, 0, 400, 400));
        for _ in 0..4 {
            rect.step(&curve, Duration::from_millis(16));
        }
        let midway = rect.now().x;
        rect.retarget(Rect::new(800, 0, 400, 400));
        rect.step(&curve, Duration::from_millis(16));
        assert!(
            rect.now().x > midway,
            "a repeated target restarted the curve: still at {midway}"
        );
    }
}

/// The same rectangle, somewhere else.
///
/// What a desktop sliding away is made of: every window on it keeps its size and
/// its place relative to its neighbours, and the whole arrangement travels.
pub fn moved(rect: Rect, dx: i32, dy: i32) -> Rect {
    Rect::new(rect.x + dx, rect.y + dy, rect.w, rect.h)
}

/// Where a desktop sits in the order they are shown in.
///
/// By number when it has one, because that is what a bar sorts by and what
/// somebody pressing Super+3 is thinking of. Anything unnamed sorts after
/// everything named -- the rule the bar already uses -- so an unnamed desktop is
/// always to the right of a numbered one rather than wherever the order it was
/// created in happens to put it.
pub fn order(name: Option<&str>) -> u32 {
    name.and_then(|name| name.parse::<u32>().ok())
        .unwrap_or(u32::MAX)
}

/// How far the old desktop travels, and where the new one comes in from.
///
/// Going to a higher-numbered desktop moves the screen left: what was there
/// leaves to the left and what is arriving comes from the right, which is the
/// way the indicator in the bar moves. Going back does the reverse.
pub fn slide(from: u32, to: u32, span: i32) -> (i32, i32) {
    if to >= from {
        (-span, span)
    } else {
        (span, -span)
    }
}

#[cfg(test)]
mod desktops {
    use super::*;

    #[test]
    fn a_rectangle_moves_sideways_and_nothing_else() {
        let rect = Rect::new(100, 200, 800, 600);
        assert_eq!(moved(rect, 50, 0), Rect::new(150, 200, 800, 600));
        assert_eq!(moved(rect, -1000, 0), Rect::new(-900, 200, 800, 600));
        assert_eq!(moved(rect, 0, 7), Rect::new(100, 207, 800, 600));
    }

    #[test]
    fn desktops_are_ordered_by_the_number_they_are_called() {
        // The order a bar sorts them in, which is the order somebody looking at
        // the screen believes they are in -- not the order they were created,
        // which is what the ids record and which is routinely different.
        assert_eq!(order(Some("3")), 3);
        assert_eq!(order(Some("10")), 10);
        assert!(order(Some("2")) < order(Some("10")));
        // Anything not a number goes to the end rather than somewhere arbitrary.
        assert_eq!(order(Some("scratch")), u32::MAX);
        assert_eq!(order(None), u32::MAX);
        assert!(order(Some("9")) < order(Some("scratch")));
    }

    #[test]
    fn going_up_the_desktops_moves_the_screen_left() {
        // What is here leaves to the left; what is coming comes from the right.
        // The bar's indicator moves right at the same time, and a screen that
        // slid the other way would be arguing with it.
        let (leaves, arrives) = slide(1, 3, 1000);
        assert_eq!(leaves, -1000);
        assert_eq!(arrives, 1000);
    }

    #[test]
    fn going_back_down_them_moves_it_the_other_way() {
        let (leaves, arrives) = slide(5, 2, 1000);
        assert_eq!(leaves, 1000);
        assert_eq!(arrives, -1000);
    }

    #[test]
    fn the_two_always_travel_in_opposite_directions() {
        // Otherwise the desktop arriving would chase the one leaving instead of
        // taking its place.
        for (from, to) in [(1, 2), (2, 1), (1, 1), (7, 70), (u32::MAX, 1)] {
            let (leaves, arrives) = slide(from, to, 800);
            assert_eq!(leaves, -arrives, "{from} to {to}");
            assert_ne!(leaves, 0);
        }
    }
}

/// How fast a window has to be going before it leans into it at all.
///
/// Below this it is being nudged rather than thrown, and a window that deformed
/// while being placed carefully would be a window fighting the person moving it.
const LEAN_FROM: f32 = 120.0;

/// The speed at which the lean reaches its limit, in pixels a second.
const LEAN_FULL: f32 = 2200.0;

/// The most a window will ever be stretched, as a fraction of its size.
///
/// Small on purpose. The effect wants to be felt rather than seen: past about a
/// tenth it stops reading as weight and starts reading as a rendering fault.
const LEAN_LIMIT: f32 = 0.09;

/// How much to stretch a window along the direction it is travelling.
///
/// Returns a horizontal and a vertical factor whose product is one, so the
/// window keeps its area: it stretches the way it is going and narrows across
/// that, the way a thrown thing does, rather than appearing to grow.
///
/// Diagonal movement is deliberately undramatic -- the two axes cancel -- which
/// is right: something moving equally in both directions has no direction to
/// lean along.
pub fn squash((vx, vy): (f32, f32)) -> (f32, f32) {
    if !vx.is_finite() || !vy.is_finite() {
        return (1.0, 1.0);
    }
    // The difference between the axes, not the total speed, so that the lean
    // points along the movement rather than growing with it.
    let along = vx.abs() - vy.abs();
    let speed = along.abs();
    if speed <= LEAN_FROM {
        return (1.0, 1.0);
    }
    let reach = ((speed - LEAN_FROM) / (LEAN_FULL - LEAN_FROM)).clamp(0.0, 1.0);
    let lean = along.signum() * reach * LEAN_LIMIT;
    let x = 1.0 + lean;
    (x, 1.0 / x)
}

#[cfg(test)]
mod leaning {
    use super::*;

    #[test]
    fn a_window_standing_still_is_not_deformed() {
        assert_eq!(squash((0.0, 0.0)), (1.0, 1.0));
        // Nor one being placed carefully rather than thrown.
        assert_eq!(squash((40.0, 0.0)), (1.0, 1.0));
        assert_eq!(squash((0.0, -90.0)), (1.0, 1.0));
    }

    #[test]
    fn a_window_keeps_its_area_however_fast_it_goes() {
        // Stretching without narrowing would read as the window growing, which
        // is a different effect and not the one wanted.
        for velocity in [
            (400.0, 0.0),
            (-1500.0, 0.0),
            (0.0, 900.0),
            (3000.0, 200.0),
            (-8000.0, -50.0),
        ] {
            let (x, y) = squash(velocity);
            assert!(
                (x * y - 1.0).abs() < 1e-5,
                "{velocity:?} changed the area: {x} by {y}"
            );
        }
    }

    #[test]
    fn it_stretches_the_way_it_is_going() {
        let (x, y) = squash((1800.0, 0.0));
        assert!(x > 1.0, "sideways movement should widen it: {x}");
        assert!(y < 1.0);
        // The same speed the other way leans the same amount: which way along an
        // axis it is going makes no difference to how it deforms.
        assert_eq!(squash((-1800.0, 0.0)), (x, y));

        let (x, y) = squash((0.0, 1800.0));
        assert!(x < 1.0, "upward movement should narrow it: {x}");
        assert!(y > 1.0);
    }

    #[test]
    fn nothing_is_stretched_past_the_limit() {
        for speed in [3000.0, 20_000.0, f32::MAX] {
            let (x, y) = squash((speed, 0.0));
            assert!(x <= 1.0 + LEAN_LIMIT + 1e-5, "{speed} stretched to {x}");
            assert!(y >= 1.0 / (1.0 + LEAN_LIMIT) - 1e-5);
        }
    }

    #[test]
    fn moving_equally_in_both_directions_has_no_direction_to_lean_along() {
        assert_eq!(squash((1000.0, 1000.0)), (1.0, 1.0));
        assert_eq!(squash((-1000.0, 1000.0)), (1.0, 1.0));
    }

    #[test]
    fn a_velocity_that_is_not_a_number_deforms_nothing() {
        // A spring handed a NaN would otherwise turn a window inside out.
        assert_eq!(squash((f32::NAN, 0.0)), (1.0, 1.0));
        assert_eq!(squash((0.0, f32::INFINITY)), (1.0, 1.0));
    }
}

/// The same rectangle, stretched about its own centre.
///
/// Where a lean has to be applied: scaling a window about its top-left corner
/// would move it as well as deform it, so a window leaning into a drag would
/// creep away from the pointer holding it.
pub fn stretched(rect: Rect, (fx, fy): (f32, f32)) -> Rect {
    let w = ((rect.w as f32 * fx).round() as i32).max(1);
    let h = ((rect.h as f32 * fy).round() as i32).max(1);
    Rect::new(rect.x + (rect.w - w) / 2, rect.y + (rect.h - h) / 2, w, h)
}

#[cfg(test)]
mod stretching {
    use super::*;

    #[test]
    fn stretching_keeps_a_rectangle_where_it_was() {
        // The centre is what must not move: a window leaning into a drag that
        // also crept sideways would slide out from under the pointer.
        let rect = Rect::new(100, 200, 800, 600);
        let centre = |r: Rect| (r.x * 2 + r.w, r.y * 2 + r.h);
        for factors in [(1.09, 0.917), (0.917, 1.09), (1.0, 1.0)] {
            let out = stretched(rect, factors);
            let (cx, cy) = centre(out);
            let (wx, wy) = centre(rect);
            assert!((cx - wx).abs() <= 1, "{factors:?} moved it sideways");
            assert!((cy - wy).abs() <= 1, "{factors:?} moved it down");
        }
    }

    #[test]
    fn a_stretch_of_one_changes_nothing_at_all() {
        let rect = Rect::new(7, 9, 641, 403);
        assert_eq!(stretched(rect, (1.0, 1.0)), rect);
    }

    #[test]
    fn nothing_is_ever_stretched_out_of_existence() {
        // The factors are bounded, but a one-pixel window rounded down would
        // still reach zero, and a zero-sized element is not drawable.
        let thin = Rect::new(0, 0, 1, 1);
        let out = stretched(thin, (0.01, 0.01));
        assert!(out.w >= 1 && out.h >= 1, "{out:?}");
    }
}

/// A colour part way between two others.
///
/// Mixed in the straight linear space the rest of the compositor works in, not
/// in sRGB: every colour here has already been through the same conversion, so a
/// blend that guessed differently would land somewhere neither end agrees with.
pub fn mix(from: [f32; 4], to: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    let at = |i: usize| from[i] + (to[i] - from[i]) * t;
    [at(0), at(1), at(2), at(3)]
}

#[cfg(test)]
mod mixing {
    use super::*;

    #[test]
    fn the_ends_of_a_mix_are_the_colours_it_was_given() {
        let dim = [0.1, 0.1, 0.1, 1.0];
        let bright = [0.9, 0.8, 0.7, 1.0];
        assert_eq!(mix(dim, bright, 0.0), dim);
        assert_eq!(mix(dim, bright, 1.0), bright);
    }

    #[test]
    fn halfway_is_halfway_in_every_channel() {
        let out = mix([0.0, 0.0, 0.0, 0.0], [1.0, 0.5, 0.25, 1.0], 0.5);
        assert_eq!(out, [0.5, 0.25, 0.125, 0.5]);
    }

    #[test]
    fn a_progress_outside_the_range_does_not_leave_the_colours_behind() {
        // A loose spring overshoots, and a border that overshot its colour would
        // flash brighter than either end of the fade.
        let dim = [0.1, 0.1, 0.1, 1.0];
        let bright = [0.9, 0.8, 0.7, 1.0];
        assert_eq!(mix(dim, bright, 1.4), bright);
        assert_eq!(mix(dim, bright, -0.4), dim);
    }
}

/// How a window part way through arriving should be drawn.
///
/// Returns the rectangle to draw it in and how much of it to draw. Pure, so
/// every style can be asked what it looks like at any moment without a
/// compositor -- and so that "does this reach its cell exactly" is a question
/// with an answer rather than something to squint at.
pub fn arriving(style: OpenStyle, cell: Rect, at: f32) -> (Rect, f32) {
    // Past the end is where a spring spends its overshoot, and a window that
    // faded past fully opaque or grew past its cell would flicker there.
    let at = at.clamp(0.0, 1.0);
    match style {
        OpenStyle::None => (cell, 1.0),
        OpenStyle::Fade => (cell, at),
        // A shade small, growing into place, fading as it comes.
        OpenStyle::Zoom => (stretched(cell, (0.86 + 0.14 * at, 0.86 + 0.14 * at)), at),
        // From nothing at the centre. No fade: the size is the whole effect,
        // and fading as well would leave the first frames invisible anyway.
        OpenStyle::Grow => (stretched(cell, (at, at)), 1.0),
        // Up from below. Opaque before it arrives, so the movement is what is
        // seen rather than the fade.
        OpenStyle::Slide => (
            moved(cell, 0, ((1.0 - at) * 48.0).round() as i32),
            (at * 1.4).min(1.0),
        ),
    }
}

#[cfg(test)]
mod arrivals {
    use super::*;

    const CELL: Rect = Rect {
        x: 100,
        y: 200,
        w: 800,
        h: 600,
    };

    #[test]
    fn every_arrival_ends_exactly_in_the_cell() {
        // The one thing that must hold for all of them: a window that settled a
        // pixel out, or a shade transparent, would stay that way for as long as
        // it was open.
        for style in [
            OpenStyle::Zoom,
            OpenStyle::Grow,
            OpenStyle::Slide,
            OpenStyle::Fade,
            OpenStyle::None,
        ] {
            let (rect, alpha) = arriving(style, CELL, 1.0);
            assert_eq!(rect, CELL, "{style:?} does not finish in its cell");
            assert_eq!(alpha, 1.0, "{style:?} does not finish opaque");
        }
    }

    #[test]
    fn an_overshooting_spring_does_not_carry_a_window_past_its_cell() {
        // The open channel is a spring with room to overshoot, and past the end
        // there is nowhere sensible to go: bigger than the cell, or more than
        // opaque.
        for style in [OpenStyle::Zoom, OpenStyle::Grow, OpenStyle::Slide] {
            let (rect, alpha) = arriving(style, CELL, 1.2);
            assert_eq!(rect, CELL, "{style:?} overshot its cell");
            assert!(alpha <= 1.0);
        }
    }

    #[test]
    fn nothing_is_visible_before_it_starts_except_what_should_be() {
        // Growing from nothing is a size of nothing; the others are invisible by
        // being transparent. Either way the first frame shows no window.
        let (rect, alpha) = arriving(OpenStyle::Grow, CELL, 0.0);
        assert!(rect.w <= 1 && rect.h <= 1);
        assert_eq!(alpha, 1.0);
        assert_eq!(arriving(OpenStyle::Fade, CELL, 0.0).1, 0.0);
        assert_eq!(arriving(OpenStyle::Zoom, CELL, 0.0).1, 0.0);
    }

    #[test]
    fn switching_it_off_is_not_an_animation_at_all() {
        for at in [0.0, 0.3, 1.0] {
            assert_eq!(arriving(OpenStyle::None, CELL, at), (CELL, 1.0));
        }
    }

    #[test]
    fn a_window_zooming_in_stays_centred_on_its_cell() {
        let (rect, _) = arriving(OpenStyle::Zoom, CELL, 0.5);
        assert!(rect.w < CELL.w && rect.w > CELL.w * 8 / 10);
        // Centre held: growing from the corner would make it slide as well as
        // grow, which is a different effect and a worse one.
        assert_eq!(rect.x * 2 + rect.w, CELL.x * 2 + CELL.w);
        assert_eq!(rect.y * 2 + rect.h, CELL.y * 2 + CELL.h);
    }

    #[test]
    fn a_window_sliding_in_comes_from_below_and_only_below() {
        let (rect, alpha) = arriving(OpenStyle::Slide, CELL, 0.25);
        assert!(rect.y > CELL.y, "it should start lower down");
        assert_eq!(rect.x, CELL.x, "and not sideways");
        assert_eq!((rect.w, rect.h), (CELL.w, CELL.h), "nor change size");
        assert!(alpha > 0.25, "it fades in faster than it moves");
    }
}

/// Everything about where one window is drawn this frame.
///
/// One function because there are two callers and they must not disagree: the
/// renderer, which draws the window and positions its border strips, and
/// `sync_borders`, which sizes those strips. When the two computed this
/// separately, a window being carried stretched while its frame stayed the size
/// of the cell -- the content came away from its own border.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drawn {
    /// The window's own picture.
    pub content: Rect,
    /// The rectangle its frame is drawn around. The content grown by the border
    /// width, so the frame follows the window rather than the cell the tree
    /// assigned -- which are the same rectangle whenever nothing is moving.
    pub frame: Rect,
}

/// Where a window and its frame go, given everything that moves them.
pub fn drawn(
    cell: Rect,
    border: i32,
    fullscreen: bool,
    lean: Option<(f32, f32)>,
    opening: (OpenStyle, f32),
) -> (Drawn, f32) {
    // A fullscreen window covers the display outright and owns no frame.
    let content = if fullscreen { cell } else { cell.inset(border) };
    let content = match lean {
        Some(velocity) => stretched(content, squash(velocity)),
        None => content,
    };
    let (content, alpha) = arriving(opening.0, content, opening.1);
    let frame = if fullscreen {
        content
    } else {
        grown(content, border)
    };
    (Drawn { content, frame }, alpha)
}

/// A rectangle with a frame of this width around it. The inverse of `inset`.
fn grown(rect: Rect, by: i32) -> Rect {
    Rect::new(
        rect.x - by,
        rect.y - by,
        (rect.w + by * 2).max(1),
        (rect.h + by * 2).max(1),
    )
}

#[cfg(test)]
mod drawing {
    use super::*;

    const CELL: Rect = Rect {
        x: 100,
        y: 200,
        w: 800,
        h: 600,
    };
    const STILL: (OpenStyle, f32) = (OpenStyle::Zoom, 1.0);

    #[test]
    fn a_window_sitting_still_is_drawn_exactly_in_its_cell() {
        // The round trip that matters: inset by the border and grown back must
        // land on the cell the tree gave, or a settled window would sit a pixel
        // off its own neighbours forever.
        for border in [0, 1, 2, 6] {
            let (drawn, alpha) = drawn(CELL, border, false, None, STILL);
            assert_eq!(drawn.frame, CELL, "border {border}");
            assert_eq!(drawn.content, CELL.inset(border), "border {border}");
            assert_eq!(alpha, 1.0);
        }
    }

    #[test]
    fn a_leaning_window_takes_its_frame_with_it() {
        // The bug this function exists to prevent: the content stretched and the
        // frame did not, so a carried window came away from its own border.
        let (drawn, _) = drawn(CELL, 2, false, Some((2500.0, 0.0)), STILL);
        assert_ne!(drawn.content, CELL.inset(2), "it should have leaned");
        assert_eq!(
            drawn.frame,
            grown(drawn.content, 2),
            "the frame is not around the content"
        );
        // And still centred where the cell is, because the lean is about the
        // centre.
        assert_eq!(drawn.frame.x * 2 + drawn.frame.w, CELL.x * 2 + CELL.w);
    }

    #[test]
    fn an_arriving_window_takes_its_frame_with_it_too() {
        let (drawn, alpha) = drawn(CELL, 2, false, None, (OpenStyle::Zoom, 0.5));
        assert!(
            drawn.content.w < CELL.inset(2).w,
            "it should be small still"
        );
        assert_eq!(drawn.frame, grown(drawn.content, 2));
        assert!(alpha < 1.0, "and part way through fading in");
    }

    #[test]
    fn a_fullscreen_window_has_no_frame_to_keep_up_with() {
        let (drawn, _) = drawn(CELL, 2, true, None, STILL);
        assert_eq!(drawn.content, CELL);
        assert_eq!(drawn.frame, CELL, "a frame would make it not fullscreen");
    }

    #[test]
    fn a_window_that_leans_while_arriving_does_both() {
        let (drawn, alpha) = drawn(CELL, 2, false, Some((2500.0, 0.0)), (OpenStyle::Zoom, 0.6));
        assert_eq!(drawn.frame, grown(drawn.content, 2));
        assert!(alpha < 1.0);
    }
}

#[cfg(test)]
mod spaces {
    use super::*;

    /// The renderer asks in one display's coordinates and `sync_borders` asks in
    /// the whole desktop's. They must get the same rectangle either way, or the
    /// strips would be sized in one space and positioned in another -- which is
    /// the bug this whole function exists to prevent, arriving by a back door.
    #[test]
    fn the_answer_does_not_depend_on_where_the_origin_is() {
        let here = Rect::new(0, 0, 800, 600);
        let far = Rect::new(-3400, 1700, 800, 600);
        for lean in [None, Some((2500.0f32, 0.0f32))] {
            for open in [0.0, 0.37, 1.0] {
                for style in [OpenStyle::Zoom, OpenStyle::Grow, OpenStyle::Slide] {
                    let (a, _) = drawn(here, 2, false, lean, (style, open));
                    let (b, _) = drawn(far, 2, false, lean, (style, open));
                    assert_eq!(
                        (a.content.w, a.content.h),
                        (b.content.w, b.content.h),
                        "{style:?} at {open} sized differently"
                    );
                    assert_eq!(
                        (a.content.x - here.x, a.content.y - here.y),
                        (b.content.x - far.x, b.content.y - far.y),
                        "{style:?} at {open} offset differently"
                    );
                    assert_eq!(
                        (a.frame.x - here.x, a.frame.y - here.y, a.frame.w, a.frame.h),
                        (b.frame.x - far.x, b.frame.y - far.y, b.frame.w, b.frame.h),
                        "{style:?} at {open} framed differently"
                    );
                }
            }
        }
    }
}
