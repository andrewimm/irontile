//! The session lock.
//!
//! What is being tested is a security guarantee rather than a feature: while
//! the session is locked, nothing that was on screen is reachable. A lock that
//! merely draws over the desktop is worth nothing.

mod harness;

use harness::Compositor;
use irontile_ipc::Action;

#[test]
fn the_lock_is_advertised() {
    let compositor = Compositor::start("1920x1080");
    let client = compositor.connect_client();
    assert!(
        client
            .advertised()
            .iter()
            .any(|i| i == "ext_session_lock_manager_v1"),
        "without it nothing can lock the session, and a laptop that closes is open"
    );
}

#[test]
fn locking_takes_the_keyboard_from_the_window_that_had_it() {
    // The guarantee: what was focused stops being focused. A lock screen that
    // draws over a terminal while the terminal still receives what is typed is
    // worse than no lock, because it looks like one.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let window = client.map_window("a terminal");
    compositor.wait_for_windows(1);
    client.wait_for(|client| client.window_has_keyboard(window));

    client.lock_session();
    assert!(
        !client.window_has_keyboard(window),
        "the window still had the keyboard with the session locked"
    );

    client.unlock_session();
    client.wait_for(|client| client.window_has_keyboard(window));
}

#[test]
fn the_pointer_cannot_reach_what_is_behind_the_lock() {
    // The other half of it. A click landing on the window underneath is a way
    // through the lock even if nothing of that window is visible.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let _window = client.map_window("a terminal");
    compositor.wait_for_windows(1);

    // Onto the window. Near the corner of its cell, because the test client's
    // buffer is small: what is under the pointer is a surface's own content,
    // not the cell the layout gave it.
    let (x, y) = (30, 30);
    compositor
        .client
        .action(Action::WarpPointer(x, y))
        .expect("warp");
    client.wait_for(|client| client.pointer_on().is_some());
    let before = client.pointer_on().map(|(surface, _)| surface);
    assert!(before.is_some(), "the window has the pointer to begin with");

    client.lock_session();
    compositor
        .client
        .action(Action::WarpPointer(x, y))
        .expect("warp");
    client.roundtrip();

    let after = client.pointer_on().map(|(surface, _)| surface);
    assert_ne!(
        after, before,
        "the pointer was still on the window with the session locked"
    );
}

#[test]
fn a_second_lock_is_refused() {
    // Telling a newcomer it succeeded would hand it a session somebody else is
    // holding, and both would believe they had it.
    let compositor = Compositor::start("1920x1080");
    let mut first = compositor.connect_client();
    first.lock_session();

    let mut second = compositor.connect_client();
    assert!(
        !second.try_lock_session(),
        "the session was locked twice over"
    );
}

#[test]
fn a_lock_screen_gets_its_frame_callbacks_back() {
    // The bug this exists for: a locker that animates -- a fade, a clock, a
    // caps-lock indicator -- asks for a frame callback and waits for it before
    // drawing again. Never answering does not merely stop it drawing: one that
    // expects a callback can spin waiting, which costs a core and starves the
    // compositor's own event loop, so input then arrives too late for anything
    // to notice somebody came back. swaylock draws once and waits, so it never
    // showed any of this; hyprlock did, immediately.
    let compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    client.lock_session();

    let before = client.lock_frames(0);
    client.request_lock_frame(0);
    client.wait_for(|client| client.lock_frames(0) > before);
}
