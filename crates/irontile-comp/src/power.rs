//! When to turn the displays off, and when to bring them back.
//!
//! Only the policy lives here: a clock, a timeout, and the question of whether
//! the moment has arrived. Actually powering a connector down belongs to the
//! session backend, which is the only one with a connector to power down, and
//! keeping the decision apart from the deed is what makes the decision
//! testable without a graphics card.
//!
//! This is not sleeping. The machine stays up and everything on it keeps
//! running; it is the panel that stops being driven. Somebody who has
//! deliberately inhibited suspend so a long job can finish still wants the
//! screen off, and those two things being the same setting is the thing this
//! avoids.

use std::time::{Duration, Instant};

/// What the clock says to do about the displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Turn them off now.
    Off,
    /// Leave them as they are, and ask again after this long.
    Wait(Duration),
    /// Nothing to time. No timeout is set, or they are already off and the
    /// only thing that can change that is somebody arriving.
    Nothing,
}

/// The countdown.
#[derive(Debug)]
pub struct Blanking {
    /// How long the seat may be idle before the displays go off. `None` is
    /// never, which is what a configuration that says nothing about it means.
    after: Option<Duration>,
    /// When something last happened.
    last: Instant,
    /// Whether the displays are off now.
    off: bool,
}

impl Blanking {
    pub fn new(after: Option<Duration>, now: Instant) -> Self {
        Blanking {
            after,
            last: now,
            off: false,
        }
    }

    /// Takes a reloaded configuration.
    ///
    /// The countdown restarts rather than carrying over: a timeout that has
    /// just been shortened should not turn the screen off in the middle of the
    /// edit that shortened it.
    pub fn set_timeout(&mut self, after: Option<Duration>, now: Instant) {
        self.after = after;
        self.last = now;
    }

    pub fn is_off(&self) -> bool {
        self.off
    }

    /// Somebody is there.
    ///
    /// Returns whether the displays need bringing back, so the caller can do
    /// the expensive part only when there is something to undo.
    pub fn stir(&mut self, now: Instant) -> bool {
        self.last = now;
        let waking = self.off;
        self.off = false;
        waking
    }

    /// What to do now.
    ///
    /// `inhibited` is a window asking the session to stay awake, which counts
    /// as somebody being there: a film has nobody touching the keyboard for
    /// two hours and is the clearest case of the screen being wanted. The
    /// countdown restarts for as long as one is held, so putting the inhibitor
    /// down starts the whole timeout rather than resuming a part-spent one.
    pub fn poll(&mut self, now: Instant, inhibited: bool) -> Next {
        let Some(after) = self.after else {
            return Next::Nothing;
        };
        if inhibited {
            self.last = now;
            return Next::Wait(after);
        }
        if self.off {
            return Next::Nothing;
        }
        match after.checked_sub(now.saturating_duration_since(self.last)) {
            // `checked_sub` gives None once the time has passed, and zero is
            // the tick that lands exactly on it.
            None => Next::Off,
            Some(left) if left.is_zero() => Next::Off,
            Some(left) => Next::Wait(left),
        }
    }

    /// Records that the displays are now off. Separate from `poll` because
    /// turning them off can fail, and a countdown that believed it had
    /// happened would never try again.
    pub fn went_off(&mut self) {
        self.off = true;
    }
}

/// Reads a timeout as somebody would write one: `10m`, `30s`, `1h`.
///
/// A bare number is refused rather than guessed at. Minutes are the obvious
/// reading and seconds are the obvious implementation, and a setting where
/// those two disagree by a factor of sixty is not one to be clever about.
pub fn parse_timeout(text: &str) -> Result<Option<Duration>, String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("never") || text == "0" {
        return Ok(None);
    }
    let (digits, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let amount: u64 = digits
        .parse()
        .map_err(|_| format!("`{text}` is not a length of time, such as `10m`"))?;
    let seconds = match unit.trim() {
        "s" => amount,
        "m" => amount * 60,
        "h" => amount * 3600,
        "" => {
            return Err(format!(
                "`{text}` needs a unit: `{text}s`, `{text}m` or `{text}h`"
            ));
        }
        other => return Err(format!("`{other}` is not a unit of time; use s, m or h")),
    };
    if seconds == 0 {
        return Ok(None);
    }
    Ok(Some(Duration::from_secs(seconds)))
}

#[cfg(test)]
mod timeouts {
    use super::*;

    #[test]
    fn a_length_of_time_is_written_the_way_people_write_one() {
        assert_eq!(parse_timeout("30s"), Ok(Some(Duration::from_secs(30))));
        assert_eq!(parse_timeout("10m"), Ok(Some(Duration::from_secs(600))));
        assert_eq!(parse_timeout("1h"), Ok(Some(Duration::from_secs(3600))));
        assert_eq!(parse_timeout(" 5m "), Ok(Some(Duration::from_secs(300))));
    }

    #[test]
    fn never_and_nothing_and_zero_all_mean_leave_it_on() {
        assert_eq!(parse_timeout("never"), Ok(None));
        assert_eq!(parse_timeout("Never"), Ok(None));
        assert_eq!(parse_timeout(""), Ok(None));
        assert_eq!(parse_timeout("0"), Ok(None));
        assert_eq!(parse_timeout("0m"), Ok(None));
    }

    #[test]
    fn a_bare_number_is_refused_rather_than_guessed_at() {
        // Ten minutes or ten seconds is the difference between a setting that
        // works and one that blanks the screen while you are reading.
        let err = parse_timeout("10").unwrap_err();
        assert!(err.contains("10s"), "{err}");
        assert!(err.contains("10m"), "{err}");
    }

    #[test]
    fn nonsense_says_what_was_wrong_with_it() {
        assert!(parse_timeout("soon").unwrap_err().contains("not a length"));
        assert!(parse_timeout("10x").unwrap_err().contains("not a unit"));
    }
}

#[cfg(test)]
mod countdown {
    use super::*;

    fn at(start: Instant, seconds: u64) -> Instant {
        start + Duration::from_secs(seconds)
    }

    #[test]
    fn without_a_timeout_nothing_ever_happens() {
        let start = Instant::now();
        let mut blanking = Blanking::new(None, start);
        assert_eq!(blanking.poll(at(start, 100_000), false), Next::Nothing);
        assert!(!blanking.is_off());
    }

    #[test]
    fn it_waits_exactly_as_long_as_is_left() {
        let start = Instant::now();
        let mut blanking = Blanking::new(Some(Duration::from_secs(600)), start);
        assert_eq!(
            blanking.poll(at(start, 60), false),
            Next::Wait(Duration::from_secs(540)),
            "a tick a minute in should ask for the other nine"
        );
        assert_eq!(
            blanking.poll(at(start, 599), false),
            Next::Wait(Duration::from_secs(1))
        );
    }

    #[test]
    fn the_moment_it_arrives_counts() {
        let start = Instant::now();
        let mut blanking = Blanking::new(Some(Duration::from_secs(600)), start);
        // Landing exactly on the timeout is late enough. A tick that arrives
        // on the second should not ask to be woken again for no time at all.
        assert_eq!(blanking.poll(at(start, 600), false), Next::Off);
        assert_eq!(blanking.poll(at(start, 601), false), Next::Off);
    }

    #[test]
    fn a_hand_on_the_keyboard_starts_the_whole_timeout_again() {
        let start = Instant::now();
        let mut blanking = Blanking::new(Some(Duration::from_secs(600)), start);
        assert_eq!(
            blanking.poll(at(start, 599), false),
            Next::Wait(Duration::from_secs(1))
        );
        assert!(
            !blanking.stir(at(start, 599)),
            "it was not off, so nothing to wake"
        );
        assert_eq!(
            blanking.poll(at(start, 600), false),
            Next::Wait(Duration::from_secs(599)),
            "the countdown should run from the keypress, not from before it"
        );
    }

    #[test]
    fn an_inhibitor_holds_it_open_for_as_long_as_it_is_held() {
        let start = Instant::now();
        let mut blanking = Blanking::new(Some(Duration::from_secs(600)), start);
        // A film: nobody touches anything for two hours.
        for minute in 0..=120 {
            assert_eq!(
                blanking.poll(at(start, minute * 60), true),
                Next::Wait(Duration::from_secs(600))
            );
        }
        assert!(!blanking.is_off());
        // Putting it down starts the whole ten minutes, not the remains of an
        // earlier one. Asked at the same moment as the last held poll, because
        // the event loop asks sixty times a second: the gap between the last
        // poll holding it and the first poll not is a frame, not a minute.
        assert_eq!(
            blanking.poll(at(start, 120 * 60), false),
            Next::Wait(Duration::from_secs(600))
        );
    }

    #[test]
    fn once_off_there_is_nothing_left_to_time() {
        let start = Instant::now();
        let mut blanking = Blanking::new(Some(Duration::from_secs(600)), start);
        assert_eq!(blanking.poll(at(start, 600), false), Next::Off);
        blanking.went_off();
        // Asking again must not turn them off a second time: the only thing
        // that changes this state is somebody arriving.
        assert_eq!(blanking.poll(at(start, 1200), false), Next::Nothing);
        assert!(blanking.is_off());
    }

    #[test]
    fn waking_arms_it_again() {
        let start = Instant::now();
        let mut blanking = Blanking::new(Some(Duration::from_secs(600)), start);
        assert_eq!(blanking.poll(at(start, 600), false), Next::Off);
        blanking.went_off();

        assert!(
            blanking.stir(at(start, 700)),
            "it was off, so this is a wake"
        );
        assert!(!blanking.is_off());
        assert_eq!(
            blanking.poll(at(start, 700), false),
            Next::Wait(Duration::from_secs(600))
        );
        // And it goes off again if nothing else happens.
        assert_eq!(blanking.poll(at(start, 1300), false), Next::Off);
    }

    #[test]
    fn a_reloaded_timeout_does_not_blank_the_screen_you_are_editing_on() {
        let start = Instant::now();
        let mut blanking = Blanking::new(Some(Duration::from_secs(3600)), start);
        // Fifty minutes in, the timeout is cut to ten. The old countdown would
        // already be past it.
        let edited = at(start, 50 * 60);
        blanking.set_timeout(Some(Duration::from_secs(600)), edited);
        assert_eq!(
            blanking.poll(edited, false),
            Next::Wait(Duration::from_secs(600))
        );
    }
}
