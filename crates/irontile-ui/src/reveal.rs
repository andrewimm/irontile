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
    /// Whose text this is.
    ///
    /// The difference between moving to another window and a window renaming
    /// itself, which look identical if all you compare is the string. A
    /// terminal running something chatty rewrites its own title several times
    /// a second, and animating each one turns the bar into a slot machine.
    subject: Option<u64>,
    /// Zero while `going` is fully in place, one once `here` is.
    at: Animated,
}

impl Default for Reveal {
    fn default() -> Self {
        Reveal {
            going: String::new(),
            here: String::new(),
            subject: None,
            // Already arrived: the first text to be shown is not a change.
            at: Animated::unit(1.0),
        }
    }
}

impl Reveal {
    /// Replaces the text, starting a change if it is actually a change.
    ///
    /// `subject` is whose text this is -- the focused window, for the bar.
    /// Being handed the same string again does nothing, which matters because
    /// the bar re-reads the whole world on every event: a window title arrives
    /// unchanged many times for every time it changes.
    ///
    /// A new subject animates. The same subject with different text does not:
    /// it is one window saying something new about itself, not the bar coming
    /// to rest on a different window, and only the second is worth a movement.
    /// Without that distinction a terminal whose title tracks what it is doing
    /// -- a build, a long-running tool, anything that reports progress in the
    /// title -- makes the bar slide continuously for as long as it runs.
    ///
    /// Updating the same subject mid-animation leaves the animation alone, so
    /// a window change that is still travelling arrives at whatever the title
    /// has become rather than being cut short by it.
    pub fn to(&mut self, subject: Option<u64>, text: &str) {
        if text == self.here && subject == self.subject {
            return;
        }
        let same_subject = subject == self.subject && !self.here.is_empty();
        self.subject = subject;
        if same_subject {
            self.here = text.to_string();
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
        reveal.to(Some(1), "nvim anim.rs");
        assert!(!reveal.moving());
        assert_eq!(reveal.here(), "nvim anim.rs");
        assert_eq!(reveal.going(), None);
        assert_eq!(reveal.progress(), 1.0);
    }

    #[test]
    fn a_change_shows_the_old_text_leaving() {
        let mut reveal = Reveal::default();
        reveal.to(Some(1), "htop");
        reveal.to(Some(2), "fish ~");
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
        reveal.to(Some(1), "htop");
        reveal.to(Some(2), "fish ~");
        for _ in 0..3 {
            reveal.step(&spring(), Duration::from_millis(16));
        }
        let midway = reveal.progress();
        reveal.to(Some(2), "fish ~");
        assert_eq!(reveal.progress(), midway, "it restarted");
    }

    #[test]
    fn a_second_change_abandons_the_first_rather_than_queueing_it() {
        let mut reveal = Reveal::default();
        reveal.to(Some(1), "one");
        reveal.to(Some(2), "two");
        for _ in 0..3 {
            reveal.step(&spring(), Duration::from_millis(16));
        }
        reveal.to(Some(3), "three");
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
        reveal.to(Some(1), "before");
        reveal.to(Some(2), "after");
        reveal.finish();
        assert!(!reveal.moving());
        assert_eq!(reveal.here(), "after");
        assert_eq!(reveal.going(), None);
    }

    #[test]
    fn a_window_renaming_itself_does_not_animate() {
        // The bug this exists for. A terminal running something that reports
        // progress in its title rewrites it several times a second, and every
        // one of those used to be a full slide: the bar never stood still for
        // as long as the job ran.
        let mut reveal = Reveal::default();
        reveal.to(Some(7), "claude");
        for text in ["claude - thinking", "claude - editing", "claude - done"] {
            reveal.to(Some(7), text);
            assert!(!reveal.moving(), "{text:?} should have arrived quietly");
            assert_eq!(reveal.going(), None, "nothing should be leaving");
            assert_eq!(reveal.here(), text, "but it should still be shown");
        }
    }

    #[test]
    fn moving_to_another_window_still_animates() {
        // The half worth keeping: the whole point of the movement is to say
        // that the bar is talking about something else now.
        let mut reveal = Reveal::default();
        reveal.to(Some(7), "claude");
        reveal.to(Some(8), "htop");
        assert!(reveal.moving());
        assert_eq!(reveal.going().map(|(text, _)| text), Some("claude"));
    }

    #[test]
    fn two_windows_with_the_same_title_still_animate() {
        // Comparing the text alone would miss this: two terminals both called
        // `fish ~` are still two windows, and moving between them is still
        // worth saying.
        let mut reveal = Reveal::default();
        reveal.to(Some(1), "fish ~");
        reveal.to(Some(2), "fish ~");
        assert!(
            reveal.moving(),
            "the bar is talking about another window now"
        );
        assert_eq!(reveal.going().map(|(text, _)| text), Some("fish ~"));
    }

    #[test]
    fn a_rename_mid_slide_lets_the_slide_finish() {
        // A window change that is still travelling when the new window renames
        // itself should arrive at the new name rather than being cut short by
        // it -- the movement has already been started and stopping it halfway
        // looks like a glitch.
        let mut reveal = Reveal::default();
        reveal.to(Some(1), "htop");
        reveal.to(Some(2), "claude");
        for _ in 0..3 {
            reveal.step(&spring(), Duration::from_millis(16));
        }
        let midway = reveal.progress();
        assert!(
            midway > 0.0 && midway < 1.0,
            "it should be part way through"
        );

        reveal.to(Some(2), "claude - thinking");
        assert!(reveal.moving(), "the slide should still be running");
        assert_eq!(reveal.progress(), midway, "and not have restarted");
        assert_eq!(reveal.going().map(|(text, _)| text), Some("htop"));
        assert_eq!(
            reveal.here(),
            "claude - thinking",
            "landing on the new name"
        );
    }

    #[test]
    fn going_back_to_an_empty_title_is_still_a_change() {
        // Closing the last window: the title empties, and that is worth seeing
        // just as much as any other change.
        let mut reveal = Reveal::default();
        reveal.to(Some(1), "the last window");
        // No window focused at all, which is a different subject rather than
        // the same one having fallen silent.
        reveal.to(None, "");
        assert!(reveal.moving());
        assert_eq!(
            reveal.going().map(|(text, _)| text),
            Some("the last window")
        );
        assert_eq!(reveal.here(), "");
    }
}
