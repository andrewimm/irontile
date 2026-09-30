//! One piece of text being replaced by another, visibly.
//!
//! A bar that simply swapped the window title would leave you unsure whether
//! anything happened: the text is different, but nothing said so, and a title
//! that changed because you moved to another window looks exactly like a window
//! that renamed itself. Showing the old line leave upward as the new one arrives
//! says which of those it was.
//!
//! Nothing here draws. It holds the two strings and one number between zero and
//! one, and whatever draws decides what to do with that -- the bar lifts and
//! fades, and something else could slide sideways from the same state.

use std::time::Duration;

use irontile_motion::{Animated, Motion};

/// Text on its way to being replaced.
#[derive(Debug)]
pub struct Reveal {
    /// What is leaving. Empty when there was nothing there before, which is the
    /// case the first time anything is shown.
    going: String,
    /// What has arrived and will stay.
    here: String,
    /// Zero while `going` is fully in place, one once `here` is.
    at: Animated,
}

impl Default for Reveal {
    fn default() -> Self {
        Reveal {
            going: String::new(),
            here: String::new(),
            // Already arrived: the first text to be shown is not a change.
            at: Animated::unit(1.0),
        }
    }
}

impl Reveal {
    /// Replaces the text, starting a change if it is actually different.
    ///
    /// Being handed the same string again does nothing, which matters because
    /// the bar re-reads the whole world on every event: a window title arrives
    /// unchanged many times for every time it changes.
    pub fn to(&mut self, text: &str) {
        if text == self.here {
            return;
        }
        // Whatever was part way through leaving is abandoned rather than queued.
        // Two titles in quick succession should end on the second one, and a
        // queue would show a change nobody is waiting for any more.
        self.going = std::mem::replace(&mut self.here, text.to_string());
        // The first text is not a change, so there is nothing to see it replace.
        if self.going.is_empty() {
            self.at.snap(1.0);
            return;
        }
        self.at.snap(0.0);
        self.at.retarget(1.0);
    }

    /// The text that is staying.
    pub fn here(&self) -> &str {
        &self.here
    }

    /// The text being replaced, and how far along the change is, while there is
    /// still something to see. `None` once it has finished.
    pub fn going(&self) -> Option<(&str, f32)> {
        if !self.at.moving() {
            return None;
        }
        Some((&self.going, self.at.value()))
    }

    /// How far along, from nothing to done.
    pub fn progress(&self) -> f32 {
        self.at.value()
    }

    pub fn moving(&self) -> bool {
        self.at.moving()
    }

    /// Advances by `dt`. Returns whether there is still something to show.
    pub fn step(&mut self, motion: &Motion, dt: Duration) -> bool {
        self.at.step(motion, dt)
    }

    /// Ends the change immediately, leaving the new text in place.
    pub fn finish(&mut self) {
        self.at.snap(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spring() -> Motion {
        Motion::spring(0.3, 0.72)
    }

    fn settle(reveal: &mut Reveal) -> usize {
        let mut frames = 0;
        while reveal.step(&spring(), Duration::from_millis(16)) && frames < 600 {
            frames += 1;
        }
        frames
    }

    #[test]
    fn the_first_text_arrives_without_a_flourish() {
        // Starting the bar should not look like the title just changed.
        let mut reveal = Reveal::default();
        reveal.to("nvim anim.rs");
        assert!(!reveal.moving());
        assert_eq!(reveal.here(), "nvim anim.rs");
        assert_eq!(reveal.going(), None);
        assert_eq!(reveal.progress(), 1.0);
    }

    #[test]
    fn a_change_shows_the_old_text_leaving() {
        let mut reveal = Reveal::default();
        reveal.to("htop");
        reveal.to("fish ~");
        assert!(reveal.moving());
        let (going, at) = reveal.going().expect("something should be leaving");
        assert_eq!(going, "htop");
        assert_eq!(at, 0.0);
        assert_eq!(reveal.here(), "fish ~");

        let frames = settle(&mut reveal);
        assert!(frames > 1, "a change should take more than one frame");
        assert_eq!(reveal.going(), None, "nothing should still be leaving");
        assert_eq!(reveal.progress(), 1.0);
        assert_eq!(reveal.here(), "fish ~");
    }

    #[test]
    fn the_same_text_again_is_not_a_change() {
        // The bar re-reads everything on every compositor event, so this is the
        // common case by a wide margin.
        let mut reveal = Reveal::default();
        reveal.to("htop");
        reveal.to("fish ~");
        for _ in 0..3 {
            reveal.step(&spring(), Duration::from_millis(16));
        }
        let midway = reveal.progress();
        reveal.to("fish ~");
        assert_eq!(reveal.progress(), midway, "it restarted");
    }

    #[test]
    fn a_second_change_abandons_the_first_rather_than_queueing_it() {
        let mut reveal = Reveal::default();
        reveal.to("one");
        reveal.to("two");
        for _ in 0..3 {
            reveal.step(&spring(), Duration::from_millis(16));
        }
        reveal.to("three");
        // "two" never finished arriving, and it is what "three" is replacing:
        // showing "one" leave again would be showing a change that is two
        // titles out of date.
        let (going, at) = reveal.going().expect("something should be leaving");
        assert_eq!(going, "two");
        assert_eq!(at, 0.0);
        assert_eq!(reveal.here(), "three");
    }

    #[test]
    fn finishing_leaves_the_new_text_in_place() {
        let mut reveal = Reveal::default();
        reveal.to("before");
        reveal.to("after");
        reveal.finish();
        assert!(!reveal.moving());
        assert_eq!(reveal.here(), "after");
        assert_eq!(reveal.going(), None);
    }

    #[test]
    fn going_back_to_an_empty_title_is_still_a_change() {
        // Closing the last window: the title empties, and that is worth seeing
        // just as much as any other change.
        let mut reveal = Reveal::default();
        reveal.to("the last window");
        reveal.to("");
        assert!(reveal.moving());
        assert_eq!(
            reveal.going().map(|(text, _)| text),
            Some("the last window")
        );
        assert_eq!(reveal.here(), "");
    }
}
