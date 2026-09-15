//! Copying a display's pixels to a client.
//!
//! Filling the buffer needs a renderer, which a headless compositor has none
//! of, so what is covered here is the conversation: that the protocol is
//! offered at all, and that a client is told to bring a buffer the shape of the
//! display rather than some other shape.

mod harness;

use harness::Compositor;

#[test]
fn the_screen_can_be_copied() {
    let compositor = Compositor::start("1920x1080");
    let client = compositor.connect_client();
    assert!(
        client
            .advertised()
            .iter()
            .any(|i| i == "zwlr_screencopy_manager_v1"),
        "without it there are no screenshots and no screen sharing"
    );
}

#[test]
fn a_copy_is_offered_a_buffer_the_shape_of_the_display() {
    let compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let offer = client.ask_for_a_copy(0).expect("an offer");
    assert_eq!(
        offer,
        (1920, 1080, 1920 * 4),
        "the display's own pixels, four bytes each, packed"
    );
}

#[test]
fn each_display_is_offered_its_own_shape() {
    // The second display is a different size, and a client that copied it into
    // a buffer cut for the first would get a picture of neither.
    let compositor = Compositor::start("1920x1080,1280x1024");
    let mut client = compositor.connect_client();
    assert_eq!(client.display_count(), 2);

    let first = client.ask_for_a_copy(0).expect("an offer");
    assert_eq!(first, (1920, 1080, 1920 * 4));
}
