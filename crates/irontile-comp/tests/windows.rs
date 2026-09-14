//! Window-level tests, driven by a real Wayland client.
//!
//! These cover what neither the layout engine's own tests nor the control
//! socket can reach on their own: that a real toplevel is mapped, configured,
//! tiled and unmapped correctly.

mod harness;

use harness::Compositor;
use irontile_ipc::PlacementKind;

#[test]
fn a_mapped_window_fills_the_work_area() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let window = client.map_window("solo");
    let frame = compositor.wait_for_windows(1);

    let placement = &frame.placements[0];
    assert_eq!(placement.kind, PlacementKind::Tiled);
    // The whole display less the outer gap on each side.
    assert_eq!(placement.rect.w, 1920 - 8);
    assert_eq!(placement.rect.h, 1080 - 8);
    assert!(placement.focused);

    // What the client was told must match, less the border it is inset by.
    let configured = client.configured(window);
    assert_eq!(configured.width, placement.rect.w - 4);
    assert_eq!(configured.height, placement.rect.h - 4);
    assert!(configured.activated, "the only window should be activated");
    assert!(
        configured.tiled,
        "a tiled window should be told every edge is tiled"
    );
}

#[test]
fn two_windows_split_the_display_exactly() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_window("one");
    client.map_window("two");
    let frame = compositor.wait_for_windows(2);

    let mut rects: Vec<_> = frame.placements.iter().map(|p| p.rect).collect();
    rects.sort_by_key(|r| r.x);
    // Side by side: the display is wider than it is tall.
    assert_eq!(rects[0].y, rects[1].y);
    assert!(rects[0].right() < rects[1].x, "{rects:?}");
    // Every pixel between the outer gaps is used: two cells and one inner gap.
    assert_eq!(rects[0].w + rects[1].w + 4, 1920 - 8);
    assert!(!rects[0].intersects(rects[1]));
}

#[test]
fn a_third_window_splits_the_other_way() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_window("one");
    client.map_window("two");
    client.map_window("three");
    let frame = compositor.wait_for_windows(3);

    // The second cell is taller than it is wide by now, so the third window
    // goes below rather than alongside: that is the dwindling split.
    let mut right: Vec<_> = frame
        .placements
        .iter()
        .map(|p| p.rect)
        .filter(|r| r.x > 900)
        .collect();
    assert_eq!(right.len(), 2, "{:?}", frame.placements);
    right.sort_by_key(|r| r.y);
    assert_eq!(right[0].x, right[1].x);
    assert!(right[0].bottom() < right[1].y);
}

#[test]
fn focus_follows_the_newest_window() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let first = client.map_window("one");
    let second = client.map_window("two");
    compositor.wait_for_windows(2);

    // Activation is how a client knows to draw itself as focused.
    assert!(!client.configured(first).activated);
    assert!(client.configured(second).activated);
}

#[test]
fn closing_a_window_reflows_the_rest() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_window("one");
    let second = client.map_window("two");
    compositor.wait_for_windows(2);

    client.close_window(second);
    let frame = compositor.wait_for_windows(1);
    // The survivor takes back the whole work area.
    assert_eq!(frame.placements[0].rect.w, 1920 - 8);
}

#[test]
fn a_window_can_be_sent_to_another_desktop() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_window("one");
    client.map_window("two");
    compositor.wait_for_windows(2);

    compositor
        .client
        .action(irontile_ipc::Action::MoveToWorkspace(5))
        .unwrap();

    // The desktop it went to is not on screen, so only one window is placed.
    let frame = compositor.wait_for_windows(1);
    assert_eq!(frame.placements.len(), 1);

    // It is still managed, just elsewhere.
    let layout = compositor.client.layout().unwrap();
    layout.validate().unwrap();
    assert_eq!(layout.workspaces().filter(|w| !w.is_empty()).count(), 2);
}

#[test]
fn a_window_follows_its_desktop_to_another_display() {
    let mut compositor = Compositor::start("1920x1080,1280x1024");
    let mut client = compositor.connect_client();

    let window = client.map_window("one");
    let frame = compositor.wait_for_windows(1);
    let first_output = frame.placements[0].output;
    assert!(frame.placements[0].rect.x < 1920);

    compositor
        .client
        .action(irontile_ipc::Action::SendToOutput(
            irontile_layout::Direction::Right,
        ))
        .unwrap();

    let frame = compositor.wait_for_windows(1);
    assert_ne!(frame.placements[0].output, first_output);
    // Placed on the second display, which starts at x = 1920.
    assert!(
        frame.placements[0].rect.x >= 1920,
        "{:?}",
        frame.placements[0].rect
    );

    // The second display is narrower, so the client is reconfigured smaller.
    let configured = client.configured(window);
    assert!(configured.width < 1920, "still sized for the first display");
    compositor.client.layout().unwrap().validate().unwrap();
}

#[test]
fn fullscreen_covers_the_whole_display() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let window = client.map_window("solo");
    compositor.wait_for_windows(1);

    compositor
        .client
        .action(irontile_ipc::Action::ToggleFullscreen)
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let configured = client.configured(window);
        if configured.fullscreen {
            // The whole display, gaps and border included.
            assert_eq!((configured.width, configured.height), (1920, 1080));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never went fullscreen"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn closing_the_focused_window_asks_it_to_close() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let window = client.map_window("solo");
    compositor.wait_for_windows(1);

    compositor
        .client
        .action(irontile_ipc::Action::Close)
        .unwrap();

    // A tiling compositor asks; it does not kill the client.
    client.wait_for(|c| c.was_asked_to_close(window));
}

#[test]
fn clients_are_told_to_draw_no_decoration() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    assert!(
        client.has_decoration_manager(),
        "the decoration protocol must be advertised, or clients fall back to \
         drawing their own titlebars"
    );

    let window = client.map_window("solo");
    compositor.wait_for_windows(1);

    use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::Mode;
    assert_eq!(
        client.configured(window).decoration,
        Some(Mode::ServerSide),
        "the compositor draws the only decoration there is"
    );
}

#[test]
fn the_advertised_protocol_surface_is_what_clients_expect() {
    let compositor = Compositor::start("1920x1080");
    let client = compositor.connect_client();
    let advertised = client.advertised();

    // Dropping any of these silently degrades real applications, in ways that
    // are easy to miss by looking at a screen: no server decoration means
    // titlebars come back, no primary selection means middle-click paste stops
    // working, no layer shell means no bar can exist.
    for interface in [
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "wl_output",
        "xdg_wm_base",
        "zxdg_decoration_manager_v1",
        "zwlr_layer_shell_v1",
        "zwp_primary_selection_device_manager_v1",
        "wp_cursor_shape_manager_v1",
        "wl_data_device_manager",
        "zxdg_output_manager_v1",
    ] {
        assert!(
            advertised.iter().any(|a| a == interface),
            "{interface} is not advertised; have {advertised:?}"
        );
    }

    // dmabuf is the exception: it depends on there being a renderer, and the
    // headless backend has none. Advertising it here would promise clients a
    // buffer import that could only ever fail.
    assert!(
        !advertised.iter().any(|a| a == "zwp_linux_dmabuf_v1"),
        "headless must not advertise dmabuf"
    );
}

#[test]
fn the_frame_reports_which_window_is_focused() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_window("one");
    client.map_window("two");
    let frame = compositor.wait_for_windows(2);

    // The newest window has focus, and exactly one does.
    let focused: Vec<_> = frame.placements.iter().filter(|p| p.focused).collect();
    assert_eq!(focused.len(), 1);
    let first_focused = focused[0].window;

    compositor
        .client
        .action(irontile_ipc::Action::Focus(
            irontile_layout::Direction::Left,
        ))
        .unwrap();

    // This is what the border colour is derived from, so if it stops moving the
    // highlight stops moving with it.
    let frame = compositor.client.frame().unwrap();
    let focused: Vec<_> = frame.placements.iter().filter(|p| p.focused).collect();
    assert_eq!(focused.len(), 1, "exactly one window is focused");
    assert_ne!(focused[0].window, first_focused, "focus should have moved");
}

#[test]
fn a_new_window_is_configured_for_its_cell_before_it_draws() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    // `map_window` waits for the first configure and only then attaches a
    // buffer, so this is what the client is told before it has drawn anything.
    let solo = client.map_window("solo");
    compositor.wait_for_windows(1);
    let configured = client.configured(solo);
    // The whole work area, less gaps and border: its real cell, not a size the
    // client picked for itself. Painting at the wrong size and then snapping is
    // what a flash on open looks like.
    assert_eq!(
        (configured.width, configured.height),
        (1920 - 12, 1080 - 12)
    );

    // A second window is told its half straight away, rather than opening full
    // width and shrinking.
    let second = client.map_window("second");
    compositor.wait_for_windows(2);
    let configured = client.configured(second);
    assert!(
        configured.width < 1000,
        "opened at {}px, so it was sized before the split rather than after",
        configured.width
    );
    assert!(configured.tiled);
}

#[test]
fn a_window_that_never_draws_is_not_rendered() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_window("drawn");
    compositor.wait_for_windows(1);

    // A toplevel that exists but has committed no buffer takes part in the
    // layout, so the drawn windows resize around it, but must not appear.
    let pending = client.create_toplevel_without_buffer("pending");
    let frame = compositor.wait_for_windows(1);
    assert_eq!(
        frame.placements.len(),
        1,
        "an undrawn window must not be shown"
    );

    // Once it draws, it appears.
    client.attach_buffer(pending);
    let frame = compositor.wait_for_windows(2);
    assert_eq!(frame.placements.len(), 2);
}
