//! Idle notification, which is what an idle daemon waits on.
//!
//! Without this a screen never dims, never locks itself and never sleeps: the
//! daemon is running and has nothing to be told.

mod harness;

use std::time::Duration;

use harness::Compositor;

/// Long enough that the compositor's own startup does not race it, short enough
/// that a test waiting through it is not a nuisance.
const IDLE_AFTER: Duration = Duration::from_millis(300);

#[test]
fn the_seat_is_reported_idle_once_nothing_has_happened() {
    let compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let _watch = client.watch_idle(IDLE_AFTER);

    assert!(!client.idled(), "not idle the moment it was asked");
    client.wait_for(|client| client.idled());
}

#[test]
fn an_inhibitor_on_a_window_that_is_showing_keeps_the_seat_awake() {
    // What a video player asks for. The protocol leaves it to the compositor to
    // ignore inhibitors whose surface nobody can see, so this checks the case
    // where somebody can.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let window = client.map_window("playing something");
    compositor.wait_for_windows(1);

    let inhibitor = client.inhibit_idle(window);
    let _watch = client.watch_idle(IDLE_AFTER);

    // Well past the point it would have idled without the inhibitor.
    std::thread::sleep(IDLE_AFTER * 3);
    assert!(
        !client.idled(),
        "an inhibited seat should not be reported idle"
    );

    // And letting go lets it idle, which says the inhibitor was what held it
    // rather than the notification never having worked.
    client.release_idle_inhibitor(inhibitor);
    client.wait_for(|client| client.idled());
}
