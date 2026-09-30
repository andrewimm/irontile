//! Springs and curves for everything in irontile that moves.
//!
//! Three separate programs animate things -- the compositor moves windows, the
//! bar changes a title, the notification daemon slides a panel -- and they are
//! separate processes that never share memory. What they do share is the
//! feeling of the motion, so the mechanism lives here once instead of being
//! copied into each of them and drifting apart.
//!
//! Nothing here knows the name of a single animation. A compositor channel
//! called `workspace` and a bar channel called `title` describe different
//! surfaces owned by different programs, and each program declares its own and
//! reads them from its own configuration file. This crate only supplies the
//! two things they all need: a [`Motion`], which is the settings a person
//! tuned, and an [`Animated`], which is one number on its way somewhere.
//!
//! The two are kept apart on purpose. A hundred windows animating at once share
//! one `Motion` between them and hold their own `Animated` state, which is also
//! what makes a live configuration reload possible: replace the settings and
//! every value already in flight carries on under the new ones.
//!
//! # Springs and curves are not interchangeable
//!
//! A curve is a promise about time: it takes exactly this long and follows
//! exactly this shape. Retargeting one mid-flight has to restart it, because a
//! curve has no memory of how fast it was already going -- so an interrupted
//! curve visibly stutters.
//!
//! A spring is a promise about force. It keeps its velocity when the target
//! moves, so a window redirected halfway through a move continues smoothly into
//! the new direction. That is why springs are the better default for anything a
//! person can interrupt, which on a tiling desktop is nearly everything, and
//! why curves remain available for the things that should feel mechanical.

#![forbid(unsafe_code)]

use std::time::Duration;

use serde::Deserialize;

/// The largest step the spring integrator will take at once.
///
/// A semi-implicit Euler integration of a stiff spring diverges when the step
/// grows large next to the oscillation period, and the frame time is not ours
/// to choose: a missed vblank or a slow-motion time scale can hand us a tenth
/// of a second. Splitting the step keeps a stiff spring stable at any frame
/// rate, at the cost of a few extra multiplications nobody will measure.
const MAX_STEP: f32 = 1.0 / 240.0;

/// The stiffest spring that may be configured, as a period in seconds.
///
/// A response of zero is an infinitely stiff spring, which is division by zero
/// dressed as a preference. Values are clamped rather than refused: a
/// configuration file with a silly number in it should give a desktop that
/// moves oddly, not a session that will not start.
const MIN_RESPONSE: f32 = 0.02;

/// How a value travels to its target.
///
/// Held by value and copied freely; it is settings, not state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Motion {
    /// A damped harmonic oscillator, described the way a designer thinks about
    /// one rather than the way the mathematics is written.
    Spring {
        /// Roughly how long one oscillation takes, in seconds. Smaller is
        /// snappier.
        response: f32,
        /// 1.0 arrives as fast as possible without passing the target. Below
        /// that it overshoots and comes back; above it eases in slowly.
        damping_ratio: f32,
    },
    /// A cubic bezier over a fixed duration, as a stylesheet writes one.
    Bezier {
        duration: Duration,
        /// The two control points, `[x1, y1, x2, y2]`. The x values are clamped
        /// into range when parsed so the curve stays a function of time; the y
        /// values are not, because overshooting past the target and settling
        /// back is a shape worth having.
        points: [f32; 4],
    },
}

impl Default for Motion {
    fn default() -> Self {
        // A middling spring: quick enough not to feel slow, soft enough to
        // read as movement rather than a jump.
        Motion::spring(0.34, 0.8)
    }
}

impl Motion {
    /// A spring from a response period and a damping ratio.
    pub fn spring(response: f32, damping_ratio: f32) -> Motion {
        Motion::Spring {
            response: sane(response).max(MIN_RESPONSE),
            damping_ratio: sane(damping_ratio).clamp(0.0, 4.0),
        }
    }

    /// A curve from a duration in milliseconds and one of the named shapes.
    pub fn curve(duration_ms: u32, curve: Curve) -> Motion {
        Motion::bezier(duration_ms, curve.points())
    }

    /// A curve from a duration in milliseconds and four control values.
    pub fn bezier(duration_ms: u32, points: [f32; 4]) -> Motion {
        Motion::Bezier {
            // Zero would divide by the duration on the first step.
            duration: Duration::from_millis(u64::from(duration_ms.max(1))),
            points: [
                sane(points[0]).clamp(0.0, 1.0),
                sane(points[1]),
                sane(points[2]).clamp(0.0, 1.0),
                sane(points[3]),
            ],
        }
    }

    /// The spring constants the integrator actually uses: stiffness, then
    /// damping, for a unit mass.
    ///
    /// Public because it is what a person tuning a spring wants to see written
    /// down next to the friendlier numbers they typed.
    pub fn constants(&self) -> Option<(f32, f32)> {
        match *self {
            Motion::Spring {
                response,
                damping_ratio,
            } => {
                let omega = std::f32::consts::TAU / response.max(MIN_RESPONSE);
                Some((omega * omega, 2.0 * damping_ratio * omega))
            }
            Motion::Bezier { .. } => None,
        }
    }
}

/// Replaces a value that is not a number with zero.
///
/// `NaN` in a spring is permanent: it spreads to the velocity on the first step
/// and the value never recovers, so a window would simply vanish. Configuration
/// is not the place to find that out.
fn sane(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// The curves worth having a name for.
///
/// Four numbers describe any cubic bezier, and four numbers are also completely
/// opaque: `[0.16, 1.0, 0.3, 1.0]` says nothing about what it does. These are
/// the same set a stylesheet or a design tool offers, under the names they are
/// known by, so that a configuration file can be read as well as written.
/// Anything not here is still expressible as its control points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    /// Quick to leave, slow to arrive. The one to reach for when in doubt.
    #[default]
    OutCubic,
    /// Sharper than cubic: most of the distance is covered immediately.
    OutQuint,
    /// Sharper still, and the last of it is almost imperceptible.
    OutExpo,
    /// Eases away gently, then covers most of the distance in a rush before
    /// settling. Gentler off the mark than any of the ease-outs and past them
    /// by a quarter of the way through, which is what makes it read as
    /// deliberate rather than merely quick.
    Emphasized,
    /// Eases at both ends. For something travelling a long way, where leaving
    /// abruptly would look like a jump cut.
    InOutCubic,
    /// Passes the target and settles back. A spring's overshoot on a fixed
    /// schedule.
    OutBack,
    /// No easing at all. Mostly useful for proving that easing is what makes
    /// the difference.
    Linear,
}

impl Curve {
    /// The two control points, `[x1, y1, x2, y2]`.
    pub fn points(self) -> [f32; 4] {
        match self {
            Curve::OutCubic => [0.33, 1.0, 0.68, 1.0],
            Curve::OutQuint => [0.22, 1.0, 0.36, 1.0],
            Curve::OutExpo => [0.16, 1.0, 0.3, 1.0],
            Curve::Emphasized => [0.2, 0.0, 0.0, 1.0],
            Curve::InOutCubic => [0.65, 0.0, 0.35, 1.0],
            Curve::OutBack => [0.34, 1.56, 0.64, 1.0],
            Curve::Linear => [0.0, 0.0, 1.0, 1.0],
        }
    }
}

/// A cubic bezier easing, evaluated at `x` in 0..=1.
///
/// The control points give y as a function of a parameter, not of x, so this
/// solves for the parameter first -- Newton where the curve is steep enough to
/// trust a derivative, bisection where it is not.
pub fn ease(points: [f32; 4], x: f32) -> f32 {
    let [x1, y1, x2, y2] = points;
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let (cx, cy) = (3.0 * x1, 3.0 * y1);
    let (bx, by) = (3.0 * (x2 - x1) - cx, 3.0 * (y2 - y1) - cy);
    let (ax, ay) = (1.0 - cx - bx, 1.0 - cy - by);
    let at = |t: f32| ((ax * t + bx) * t + cx) * t;
    let slope = |t: f32| (3.0 * ax * t + 2.0 * bx) * t + cx;

    let mut t = x;
    for _ in 0..8 {
        let error = at(t) - x;
        if error.abs() < 1e-6 {
            return ((ay * t + by) * t + cy) * t;
        }
        let d = slope(t);
        if d.abs() < 1e-6 {
            break;
        }
        t -= error / d;
    }
    // Newton left the interval or stalled on a flat stretch, which the curves
    // that pause in the middle do. Bisection cannot fail here because x is
    // monotone in t whenever the control x values are in range, which parsing
    // guarantees.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    t = x;
    for _ in 0..40 {
        let value = at(t);
        if (value - x).abs() < 1e-6 {
            break;
        }
        if value < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) / 2.0;
    }
    ((ay * t + by) * t + cy) * t
}

/// One number on its way somewhere.
///
/// Holds no settings: every call that advances it is handed the [`Motion`] to
/// use, so the same value can be driven by whatever the configuration currently
/// says without being rebuilt.
#[derive(Clone, Copy, Debug)]
pub struct Animated {
    value: f32,
    velocity: f32,
    target: f32,
    /// Where the current flight started. Only a curve reads it; a spring has no
    /// use for where it came from.
    from: f32,
    elapsed: Duration,
    moving: bool,
    epsilon: f32,
}

impl Animated {
    /// A value that is already where it wants to be.
    ///
    /// `epsilon` is how close counts as arrived, in whatever this value is
    /// measured in. It decides when a spring stops being integrated, and so
    /// when the screen can stop being redrawn -- too small and a window animates
    /// forever over distances nobody can see.
    pub fn new(value: f32, epsilon: f32) -> Animated {
        Animated {
            value,
            velocity: 0.0,
            target: value,
            from: value,
            elapsed: Duration::ZERO,
            moving: false,
            epsilon,
        }
    }

    /// A value measured in logical pixels, settling a tenth of a pixel out.
    pub fn pixels(value: f32) -> Animated {
        Animated::new(value, 0.1)
    }

    /// A value running from 0 to 1, such as an opacity or a progress.
    pub fn unit(value: f32) -> Animated {
        Animated::new(value, 0.001)
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    /// How fast it is currently travelling, per second.
    ///
    /// Read by anything that wants the motion itself to be visible: a window
    /// leaning into the direction it is being thrown, for instance.
    pub fn velocity(&self) -> f32 {
        self.velocity
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    /// Whether this value still needs stepping, and so whether whatever draws
    /// it still needs redrawing.
    pub fn moving(&self) -> bool {
        self.moving
    }

    /// Sends the value somewhere new.
    ///
    /// Asking for the target it already has does nothing at all, which matters
    /// more than it looks: the compositor recomputes and reapplies the whole
    /// layout on every commit, so a window sitting still is told where it
    /// belongs many times a second. Restarting a curve each time would leave it
    /// permanently at its first frame.
    pub fn retarget(&mut self, target: f32) {
        if target == self.target {
            return;
        }
        self.target = target;
        self.from = self.value;
        self.elapsed = Duration::ZERO;
        self.moving = true;
    }

    /// Puts the value somewhere immediately, cancelling any flight.
    ///
    /// What a VT switch does to everything on screen: coming back to a session
    /// that resumes half-finished animations from before it went away would be
    /// worse than coming back to a settled one.
    pub fn snap(&mut self, value: f32) {
        self.value = value;
        self.target = value;
        self.from = value;
        self.velocity = 0.0;
        self.elapsed = Duration::ZERO;
        self.moving = false;
    }

    /// Moves the value without moving its target, so it springs back.
    ///
    /// A window sent to the next desktop is drawn as though it were still on
    /// the old one and travels in from off screen: the target never changed,
    /// only where the journey starts.
    pub fn nudge(&mut self, delta: f32) {
        if delta == 0.0 {
            return;
        }
        self.value += delta;
        self.from = self.value;
        self.elapsed = Duration::ZERO;
        self.moving = true;
    }

    /// Adds velocity without moving the value or its target.
    ///
    /// An impulse: the shove that makes something already settled wobble.
    /// Meaningless to a curve, which has no momentum to add to, so it is
    /// ignored there rather than faked.
    pub fn kick(&mut self, velocity: f32) {
        if velocity == 0.0 {
            return;
        }
        self.velocity += velocity;
        self.from = self.value;
        self.elapsed = Duration::ZERO;
        self.moving = true;
    }

    /// Advances by `dt` under `motion`. Returns whether it is still moving.
    pub fn step(&mut self, motion: &Motion, dt: Duration) -> bool {
        if !self.moving {
            return false;
        }
        match *motion {
            Motion::Spring {
                response,
                damping_ratio,
            } => self.step_spring(response, damping_ratio, dt.as_secs_f32()),
            Motion::Bezier { duration, points } => self.step_curve(duration, points, dt),
        }
        self.moving
    }

    fn step_spring(&mut self, response: f32, damping_ratio: f32, dt: f32) {
        let omega = std::f32::consts::TAU / response.max(MIN_RESPONSE);
        let stiffness = omega * omega;
        let damping = 2.0 * damping_ratio * omega;
        let mut remaining = dt;
        while remaining > 0.0 {
            let h = remaining.min(MAX_STEP);
            let force = -stiffness * (self.value - self.target) - damping * self.velocity;
            self.velocity += force * h;
            self.value += self.velocity * h;
            remaining -= h;
        }
        if (self.value - self.target).abs() < self.epsilon
            && self.velocity.abs() < self.epsilon * 10.0
        {
            self.value = self.target;
            self.velocity = 0.0;
            self.moving = false;
        }
    }

    fn step_curve(&mut self, duration: Duration, points: [f32; 4], dt: Duration) {
        let previous = self.value;
        self.elapsed = self.elapsed.saturating_add(dt);
        let progress = (self.elapsed.as_secs_f32() / duration.as_secs_f32()).min(1.0);
        self.value = self.from + (self.target - self.from) * ease(points, progress);
        // Reported rather than integrated, so that anything reading velocity
        // gets an answer from a curve as well as from a spring.
        let seconds = dt.as_secs_f32();
        self.velocity = if seconds > 0.0 {
            (self.value - previous) / seconds
        } else {
            0.0
        };
        if progress >= 1.0 {
            self.value = self.target;
            self.velocity = 0.0;
            self.moving = false;
        }
    }
}

/// The shape of a motion as it is written in a configuration file.
///
/// Every field is optional and carries a default, so a table may say as little
/// as `kind = "bezier"`. Converted into a [`Motion`], which is the only form
/// anything else sees: clamping happens once, here, rather than being
/// rediscovered by each caller.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct MotionFile {
    kind: Kind,
    response: f32,
    damping_ratio: f32,
    duration_ms: u32,
    /// A curve by name, for the ordinary case.
    curve: Option<Curve>,
    /// Four control values, for a curve that has no name.
    points: Option<[f32; 4]>,
}

impl Default for MotionFile {
    fn default() -> Self {
        MotionFile {
            kind: Kind::Spring,
            response: 0.34,
            damping_ratio: 0.8,
            duration_ms: 240,
            curve: None,
            points: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    #[default]
    Spring,
    Bezier,
}

impl TryFrom<MotionFile> for Motion {
    type Error = String;

    fn try_from(file: MotionFile) -> Result<Motion, String> {
        match file.kind {
            Kind::Spring => Ok(Motion::spring(file.response, file.damping_ratio)),
            Kind::Bezier => {
                let points = match (file.curve, file.points) {
                    // Both would mean one of them is being ignored, and which
                    // one is not something anybody should have to guess.
                    (Some(_), Some(_)) => {
                        return Err("name a curve or give points, not both".into());
                    }
                    (Some(curve), None) => curve.points(),
                    (None, Some(points)) => points,
                    (None, None) => Curve::default().points(),
                };
                Ok(Motion::bezier(file.duration_ms, points))
            }
        }
    }
}

impl<'de> Deserialize<'de> for Motion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let file = MotionFile::deserialize(d)?;
        Motion::try_from(file).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs an animation at a steady frame rate until it settles, returning the
    /// values it passed through and how long it took.
    fn run(motion: &Motion, from: f32, to: f32, hz: f32) -> (Vec<f32>, Duration) {
        let dt = Duration::from_secs_f32(1.0 / hz);
        let mut value = Animated::pixels(from);
        value.retarget(to);
        let mut seen = Vec::new();
        let mut elapsed = Duration::ZERO;
        // Generous: a badly damped spring is allowed to take its time, but
        // nothing may run forever.
        while value.step(motion, dt) && elapsed < Duration::from_secs(30) {
            seen.push(value.value());
            elapsed += dt;
        }
        (seen, elapsed)
    }

    #[test]
    fn a_critically_damped_spring_never_passes_its_target() {
        let motion = Motion::spring(0.3, 1.0);
        let (seen, _) = run(&motion, 0.0, 100.0, 60.0);
        assert!(!seen.is_empty());
        for value in &seen {
            assert!(*value <= 100.0 + 0.01, "overshot to {value}");
        }
    }

    #[test]
    fn an_overdamped_spring_does_not_pass_it_either() {
        let motion = Motion::spring(0.3, 1.6);
        let (seen, _) = run(&motion, 0.0, 100.0, 60.0);
        for value in &seen {
            assert!(*value <= 100.0 + 0.01, "overshot to {value}");
        }
    }

    #[test]
    fn an_underdamped_spring_overshoots_and_comes_back() {
        let motion = Motion::spring(0.3, 0.5);
        let (seen, _) = run(&motion, 0.0, 100.0, 120.0);
        assert!(
            seen.iter().any(|v| *v > 100.5),
            "a loose spring should pass its target"
        );
        assert!((seen.last().copied().unwrap_or_default() - 100.0).abs() < 0.2);
    }

    #[test]
    fn every_spring_settles_and_stops_asking_to_be_drawn() {
        for ratio in [0.4, 0.6, 0.8, 1.0, 1.4] {
            let motion = Motion::spring(0.5, ratio);
            let (_, elapsed) = run(&motion, 0.0, 800.0, 60.0);
            assert!(
                elapsed < Duration::from_secs(10),
                "ratio {ratio} took {elapsed:?}"
            );
        }
    }

    #[test]
    fn a_spring_keeps_its_speed_when_the_target_moves() {
        let motion = Motion::spring(0.4, 0.8);
        let mut value = Animated::pixels(0.0);
        value.retarget(500.0);
        for _ in 0..10 {
            value.step(&motion, Duration::from_millis(16));
        }
        let speed = value.velocity();
        assert!(speed > 1.0, "should be travelling by now, was {speed}");
        value.retarget(-500.0);
        // The whole reason springs are the default: redirecting one does not
        // discard the momentum it already had.
        assert_eq!(value.velocity(), speed);
    }

    #[test]
    fn a_curve_restarts_from_where_it_had_reached() {
        let motion = Motion::bezier(200, [0.33, 1.0, 0.68, 1.0]);
        let mut value = Animated::pixels(0.0);
        value.retarget(100.0);
        for _ in 0..5 {
            value.step(&motion, Duration::from_millis(16));
        }
        let midway = value.value();
        assert!(midway > 0.0 && midway < 100.0);
        value.retarget(0.0);
        // Continuous in value -- no jump on screen -- but starting a fresh
        // flight, which is what a curve can do and a spring need not.
        assert_eq!(value.value(), midway);
        assert!(value.step(&motion, Duration::from_millis(16)));
        assert!(value.value() < midway);
    }

    #[test]
    fn a_curve_arrives_exactly_when_it_said_it_would() {
        let motion = Motion::bezier(100, [0.33, 1.0, 0.68, 1.0]);
        let mut value = Animated::pixels(0.0);
        value.retarget(10.0);
        assert!(value.step(&motion, Duration::from_millis(99)));
        assert!(value.value() < 10.0);
        assert!(!value.step(&motion, Duration::from_millis(1)));
        assert_eq!(value.value(), 10.0);
    }

    #[test]
    fn asking_for_the_target_it_already_has_does_nothing() {
        let motion = Motion::bezier(200, [0.33, 1.0, 0.68, 1.0]);
        let mut value = Animated::pixels(0.0);
        value.retarget(100.0);
        for _ in 0..4 {
            value.step(&motion, Duration::from_millis(16));
        }
        let midway = value.value();
        // The layout is reapplied constantly; this is what stops a curve being
        // pinned to its first frame forever.
        value.retarget(100.0);
        value.step(&motion, Duration::from_millis(16));
        assert!(value.value() > midway);
    }

    #[test]
    fn a_stiff_spring_stays_stable_at_a_terrible_frame_rate() {
        // One step of a tenth of a second against a spring whose whole period
        // is a twentieth: without sub-stepping this diverges instead of
        // arriving.
        let motion = Motion::spring(0.05, 0.9);
        let mut value = Animated::pixels(0.0);
        value.retarget(100.0);
        let mut steps = 0;
        while value.step(&motion, Duration::from_millis(100)) && steps < 200 {
            assert!(
                value.value().abs() < 1000.0,
                "diverged to {}",
                value.value()
            );
            steps += 1;
        }
        assert!((value.value() - 100.0).abs() < 0.2);
    }

    #[test]
    fn the_frame_rate_does_not_change_where_a_spring_gets_to() {
        let motion = Motion::spring(0.3, 0.8);
        let at = |hz: f32| {
            let dt = Duration::from_secs_f32(1.0 / hz);
            let mut value = Animated::pixels(0.0);
            value.retarget(100.0);
            let mut elapsed = Duration::ZERO;
            while elapsed < Duration::from_millis(150) {
                value.step(&motion, dt);
                elapsed += dt;
            }
            value.value()
        };
        let (slow, fast) = (at(60.0), at(144.0));
        assert!(
            (slow - fast).abs() < 1.0,
            "60Hz reached {slow}, 144Hz reached {fast}"
        );
    }

    #[test]
    fn a_nudge_travels_back_without_moving_the_target() {
        let motion = Motion::spring(0.3, 1.0);
        let mut value = Animated::pixels(200.0);
        value.nudge(-1000.0);
        assert_eq!(value.target(), 200.0);
        assert_eq!(value.value(), -800.0);
        let mut steps = 0;
        while value.step(&motion, Duration::from_millis(16)) && steps < 600 {
            steps += 1;
        }
        assert!((value.value() - 200.0).abs() < 0.2);
    }

    #[test]
    fn a_kick_disturbs_something_already_settled() {
        let motion = Motion::spring(0.2, 0.5);
        let mut value = Animated::pixels(50.0);
        assert!(!value.moving());
        value.kick(400.0);
        assert!(value.moving());
        assert!(value.step(&motion, Duration::from_millis(16)));
        assert!(value.value() > 50.0, "a kick should move it");
        let mut steps = 0;
        while value.step(&motion, Duration::from_millis(16)) && steps < 600 {
            steps += 1;
        }
        assert!(
            (value.value() - 50.0).abs() < 0.2,
            "and it should come home"
        );
    }

    #[test]
    fn snapping_cancels_everything_in_flight() {
        let motion = Motion::spring(0.4, 0.8);
        let mut value = Animated::pixels(0.0);
        value.retarget(300.0);
        value.step(&motion, Duration::from_millis(32));
        value.snap(12.0);
        assert!(!value.moving());
        assert_eq!(value.value(), 12.0);
        assert_eq!(value.target(), 12.0);
        assert_eq!(value.velocity(), 0.0);
        assert!(!value.step(&motion, Duration::from_millis(16)));
    }

    #[test]
    fn an_easing_curve_starts_at_nothing_and_ends_at_everything() {
        for points in [
            [0.33, 1.0, 0.68, 1.0],
            [0.2, 0.0, 0.0, 1.0],
            [0.34, 1.56, 0.64, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        ] {
            assert_eq!(ease(points, 0.0), 0.0);
            assert_eq!(ease(points, 1.0), 1.0);
            assert_eq!(ease(points, -1.0), 0.0);
            assert_eq!(ease(points, 2.0), 1.0);
        }
    }

    #[test]
    fn a_linear_curve_is_the_line_it_looks_like() {
        for step in 0..=10 {
            let x = step as f32 / 10.0;
            assert!((ease([0.0, 0.0, 1.0, 1.0], x) - x).abs() < 1e-3);
        }
    }

    #[test]
    fn a_curve_that_overshoots_is_allowed_to() {
        // Back out: the y control point above 1 is the overshoot, and clamping
        // it away would quietly delete the effect someone chose.
        let peak = (1..100)
            .map(|i| ease([0.34, 1.56, 0.64, 1.0], i as f32 / 100.0))
            .fold(0.0f32, f32::max);
        assert!(peak > 1.0, "back-out should pass 1, peaked at {peak}");
    }

    #[test]
    fn every_named_curve_is_a_curve() {
        for curve in [
            Curve::OutCubic,
            Curve::OutQuint,
            Curve::OutExpo,
            Curve::Emphasized,
            Curve::InOutCubic,
            Curve::OutBack,
            Curve::Linear,
        ] {
            let points = curve.points();
            assert_eq!(ease(points, 0.0), 0.0, "{curve:?} does not start at 0");
            assert_eq!(ease(points, 1.0), 1.0, "{curve:?} does not end at 1");
            // The x control values have to stay in range or solving for the
            // parameter is not a well defined problem.
            assert!((0.0..=1.0).contains(&points[0]), "{curve:?}");
            assert!((0.0..=1.0).contains(&points[2]), "{curve:?}");
        }
    }

    #[test]
    fn only_one_named_curve_overshoots() {
        // Back-out is the one that passes its target. If another one started
        // doing that, something would have been mistyped.
        for curve in [
            Curve::OutCubic,
            Curve::OutQuint,
            Curve::OutExpo,
            Curve::Emphasized,
            Curve::InOutCubic,
            Curve::Linear,
        ] {
            let peak = (1..100)
                .map(|i| ease(curve.points(), i as f32 / 100.0))
                .fold(0.0f32, f32::max);
            assert!(peak <= 1.0 + 1e-3, "{curve:?} overshot to {peak}");
        }
        let back = (1..100)
            .map(|i| ease(Curve::OutBack.points(), i as f32 / 100.0))
            .fold(0.0f32, f32::max);
        assert!(back > 1.0, "back-out should overshoot, peaked at {back}");
    }

    #[test]
    fn the_sharper_curves_really_are_sharper() {
        // A tenth of the way through, each ease-out should have covered more
        // ground than the one before it. This ladder is the whole reason they
        // have separate names, and it is easy to break by mistyping a control
        // point into a curve that is merely a slightly different cubic.
        let early = |curve: Curve| ease(curve.points(), 0.1);
        assert!(early(Curve::Linear) < early(Curve::OutCubic));
        assert!(early(Curve::OutCubic) < early(Curve::OutQuint));
        assert!(early(Curve::OutQuint) < early(Curve::OutExpo));

        // Emphasized leaves more gently than any of them and has passed cubic
        // by a quarter of the way through: gentle, then a rush.
        assert!(early(Curve::Emphasized) < early(Curve::OutCubic));
        let quarter = |curve: Curve| ease(curve.points(), 0.25);
        assert!(quarter(Curve::Emphasized) > quarter(Curve::OutCubic));

        // Easing at both ends is the only one that is barely moving early on,
        // and the only one still exactly halfway at halfway.
        assert!(early(Curve::InOutCubic) < early(Curve::Linear));
        assert!((ease(Curve::InOutCubic.points(), 0.5) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn a_curve_may_be_named_instead_of_spelled_out() {
        #[derive(Debug, Deserialize)]
        struct One {
            motion: Motion,
        }
        let named: One = toml_lite(
            r#"
            [motion]
            kind = "bezier"
            duration_ms = 300
            curve = "out_quint"
            "#,
        );
        assert_eq!(named.motion, Motion::curve(300, Curve::OutQuint));
        // The same curve written out, which is what the lab exports.
        let spelled: One = toml_lite(
            r#"
            [motion]
            kind = "bezier"
            duration_ms = 300
            points = [0.22, 1.00, 0.36, 1.00]
            "#,
        );
        assert_eq!(spelled.motion, named.motion);
    }

    #[test]
    fn a_curve_with_no_shape_named_is_still_a_curve() {
        #[derive(Debug, Deserialize)]
        struct One {
            motion: Motion,
        }
        let plain: One = toml_lite("[motion]\nkind = \"bezier\"\n");
        assert_eq!(plain.motion, Motion::curve(240, Curve::OutCubic));
    }

    #[test]
    fn naming_a_curve_and_spelling_one_out_is_refused() {
        #[derive(Debug, Deserialize)]
        struct One {
            #[allow(dead_code)]
            motion: Motion,
        }
        let both: Result<One, _> = ::toml::from_str(
            r#"
            [motion]
            kind = "bezier"
            curve = "out_back"
            points = [0.0, 0.0, 1.0, 1.0]
            "#,
        );
        // Silently preferring one of them would mean a file that says two
        // different things and a desktop that obeys whichever we happened to
        // check first.
        let message = both.unwrap_err().to_string();
        assert!(message.contains("not both"), "unhelpful error: {message}");

        let unknown: Result<One, _> =
            ::toml::from_str("[motion]\nkind = \"bezier\"\ncurve = \"whoosh\"\n");
        assert!(unknown.is_err(), "an invented curve name should be refused");
    }

    #[test]
    fn nonsense_settings_are_clamped_rather_than_obeyed() {
        // A zero response is an infinitely stiff spring; NaN poisons every
        // later step. Neither should cost someone their session.
        let Motion::Spring {
            response,
            damping_ratio,
        } = Motion::spring(0.0, f32::NAN)
        else {
            panic!("should still be a spring");
        };
        assert!(response >= MIN_RESPONSE);
        assert_eq!(damping_ratio, 0.0);

        let Motion::Bezier { duration, points } = Motion::bezier(0, [-3.0, 0.2, 9.0, 1.0]) else {
            panic!("should still be a curve");
        };
        assert!(duration > Duration::ZERO);
        assert_eq!(points[0], 0.0);
        assert_eq!(points[2], 1.0);
    }

    #[test]
    fn the_constants_match_the_friendly_numbers() {
        let (stiffness, damping) = Motion::spring(0.5, 1.0).constants().unwrap();
        let omega = std::f32::consts::TAU / 0.5;
        assert!((stiffness - omega * omega).abs() < 0.01);
        assert!((damping - 2.0 * omega).abs() < 0.01);
        assert!(
            Motion::bezier(200, [0.0, 0.0, 1.0, 1.0])
                .constants()
                .is_none()
        );
    }

    /// The table the motion lab exports, parsed by the types a program would
    /// declare. This is the contract between the tuning tool and the desktop.
    #[test]
    fn the_table_a_person_writes_is_the_table_we_read() {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields, default)]
        struct Animation {
            time_scale: f32,
            layout: Motion,
            open: Motion,
            workspace: Motion,
        }

        impl Default for Animation {
            fn default() -> Self {
                Animation {
                    time_scale: 1.0,
                    layout: Motion::default(),
                    open: Motion::default(),
                    workspace: Motion::default(),
                }
            }
        }

        let table: Animation = toml_lite(
            r#"
            time_scale = 1.00

            [layout]
            kind = "spring"
            response = 0.380
            damping_ratio = 0.800

            [open]
            kind = "bezier"
            duration_ms = 240
            points = [0.34, 1.56, 0.64, 1.00]
            "#,
        );
        assert_eq!(table.time_scale, 1.0);
        assert_eq!(table.layout, Motion::spring(0.38, 0.8));
        assert_eq!(table.open, Motion::bezier(240, [0.34, 1.56, 0.64, 1.0]));
        // Said nothing, so it keeps the built-in feel.
        assert_eq!(table.workspace, Motion::default());
    }

    #[test]
    fn a_misspelled_setting_is_an_error_rather_than_a_shrug() {
        #[derive(Debug, Deserialize)]
        struct One {
            motion: Motion,
        }
        let bad: Result<One, _> = ::toml::from_str(
            r#"
            [motion]
            kind = "spring"
            damping = 0.8
            "#,
        );
        let message = bad.unwrap_err().to_string();
        assert!(message.contains("damping"), "unhelpful error: {message}");

        // The same table spelled correctly, to show the error was about the
        // name and not about the shape of what surrounds it.
        let good: One = toml_lite(
            r#"
            [motion]
            kind = "spring"
            damping_ratio = 0.8
            "#,
        );
        assert_eq!(good.motion, Motion::spring(0.34, 0.8));
    }

    /// Parses with `toml`, which is a dev-dependency here: the crate itself
    /// does not care what format the settings arrived in.
    fn toml_lite<T: serde::de::DeserializeOwned>(text: &str) -> T {
        ::toml::from_str(text).expect("the lab's own output should parse")
    }
}
