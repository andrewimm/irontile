//! Window-level tests, driven by a real Wayland client.
//!
//! These cover what neither the layout engine's own tests nor the control
//! socket can reach on their own: that a real toplevel is mapped, configured,
//! tiled and unmapped correctly.

mod harness;

use harness::Compositor;
use irontile_ipc::{Action, PlacementKind};

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
        // Fractional scale is useless without viewporter: a client rendering
        // at 1.5x has no other way to say how large the result should be.
        "wp_fractional_scale_manager_v1",
        "wp_viewporter",
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

#[test]
fn a_client_is_told_the_exact_scale_of_its_display() {
    let config = harness::TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        scale = 1.5
        "#,
    );
    let mut compositor = Compositor::with_config("2256x1504", Some(config.path()));
    let mut client = compositor.connect_client();

    let window = client.map_window("solo");
    compositor.wait_for_windows(1);

    // The protocol carries 120ths, so 1.5 arrives as 180. A whole number is all
    // wl_output can express, and a client given only that renders at 2x and is
    // resampled down.
    client.wait_for(|c| c.configured(window).fractional_scale.is_some());
    assert_eq!(client.configured(window).fractional_scale, Some(180));
}

#[test]
fn fractional_scale_divides_the_logical_size() {
    let config = harness::TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        scale = 1.5
        "#,
    );
    let mut compositor = Compositor::with_config("2256x1504", Some(config.path()));
    let layout = compositor.client.layout().unwrap();
    // 2256/1.5 = 1504, 1504/1.5 = 1002.67 which rounds to 1003.
    assert_eq!(
        layout.outputs()[0].logical,
        irontile_ipc::Rect::new(0, 0, 1504, 1003)
    );

    let mut client = compositor.connect_client();
    client.map_window("solo");
    let frame = compositor.wait_for_windows(1);
    assert_eq!(frame.placements[0].rect.w, 1504 - 8);
}

#[test]
fn a_window_with_shadows_is_placed_by_its_geometry_and_not_its_buffer() {
    // The bug this exists for: KiCad, and anything else that draws its own
    // shadows, appeared ten or twenty pixels down and right of where it
    // belonged. A client puts its shadows outside the rectangle it names with
    // set_window_geometry, so its buffer begins above and to the left of the
    // window itself. Placing the buffer at the cell's corner therefore puts the
    // window at the corner plus the shadow. Applications drawing no shadows
    // looked right, which is what made it read as those programs being at
    // fault rather than the compositor.
    const SHADOW: i32 = 16;
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let window = client.map_window("shadowed");
    // Not merely configured: placed. A window that has been told its size but
    // has not drawn yet is deliberately absent from the frame, and therefore
    // absent from what the pointer can be over.
    compositor.wait_for_windows(1);

    // Only the pointer can say where a surface really landed: warping to a
    // known point on screen and asking the client where it thinks the pointer
    // is measures the placement from the far end. The test client's buffer is
    // 64x64, so this stays close to the corner to land on it at all.
    let warp = |compositor: &mut Compositor, x, y| {
        compositor
            .client
            .action(Action::WarpPointer(x, y))
            .expect("the compositor refused to move the pointer");
    };

    warp(&mut compositor, 10, 10);
    client.wait_for(|client| client.pointer_on().is_some());
    let (_, before) = client.pointer_on().expect("the pointer is on the window");

    client.set_window_geometry(window, SHADOW, SHADOW, 100, 100);
    // A pixel further along, because a pointer that has not moved generates no
    // motion and the client would keep reporting where it last was.
    warp(&mut compositor, 11, 11);
    client.wait_for(|client| {
        client
            .pointer_on()
            .is_some_and(|(_, at)| at.0 > before.0 + 1.0)
    });
    let (_, after) = client.pointer_on().expect("the pointer is on the window");

    // One pixel of the move is the warp; the rest is the shadow the compositor
    // now knows to hang outside the cell rather than inside it.
    assert_eq!(
        (after.0 - before.0, after.1 - before.1),
        (f64::from(SHADOW) + 1.0, f64::from(SHADOW) + 1.0),
        "surface-local coordinates should shift by the geometry offset: \
         before {before:?}, after {after:?}"
    );
}

#[test]
fn dragging_the_seam_between_two_windows_resizes_both() {
    // Resizing by grabbing an edge, the way a floating compositor's window
    // frame does. The grab lands in the border and gap between the two cells,
    // which is the one part of the screen no client is drawing on, so it takes
    // no click away from either window.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    client.map_window("left");
    client.map_window("right");
    let before = compositor.wait_for_windows(2);

    let mut rects: Vec<_> = before.placements.iter().map(|p| p.rect).collect();
    rects.sort_by_key(|r| r.x);
    let (left, right) = (rects[0], rects[1]);
    assert_eq!(left.w, right.w, "they start even");

    // On the seam, vertically centred so this grabs the edge and not a corner.
    let seam = left.x + left.w;
    let middle = left.y + left.h / 2;
    const PULL: i32 = 60;
    for action in [
        Action::WarpPointer(seam, middle),
        Action::PressPointer(1),
        Action::WarpPointer(seam + PULL, middle),
        Action::ReleasePointer(1),
    ] {
        compositor
            .client
            .action(action)
            .expect("the compositor refused a pointer action");
    }

    let after = compositor.wait_for_windows(2);
    let mut rects: Vec<_> = after.placements.iter().map(|p| p.rect).collect();
    rects.sort_by_key(|r| r.x);
    assert_eq!(
        (rects[0].w, rects[1].w),
        (left.w + PULL, right.w - PULL),
        "the left window should have taken exactly what the right one gave up"
    );
    assert_eq!(
        (rects[0].h, rects[1].h),
        (left.h, right.h),
        "grabbing along a vertical edge should not resize vertically"
    );
}

#[test]
fn a_window_that_unmaps_itself_gives_up_its_cell() {
    // The bug this exists for: a window that unmapped without destroying its
    // toplevel kept its place in the tree forever. Nothing drew there, because
    // there was no buffer to draw, so it read as an invisible window squeezing
    // the real ones -- which is what a browser does with a window it keeps
    // around after you close it.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let keeper = client.map_window("stays");
    let ghost = client.map_window("goes away");
    let before = compositor.wait_for_windows(2);
    assert_eq!(before.placements.len(), 2, "both are placed to begin with");

    client.unmap_window(ghost);

    let after = compositor.wait_for_windows(1);
    let left: Vec<_> = after.placements.iter().map(|p| p.window).collect();
    assert_eq!(
        left.len(),
        1,
        "the unmapped window should be gone: {left:?}"
    );

    // And the one that stayed gets the whole display back, rather than being
    // left squeezed beside a cell nothing is drawing in.
    let placement = &after.placements[0];
    assert_eq!(
        placement.rect.w,
        1920 - 8,
        "the survivor fills the work area"
    );
    // Unmapping is not destroying: the toplevel is still there, and attaching
    // a buffer again puts it back on screen with a cell of its own.
    client.attach_buffer(ghost);
    let back = compositor.wait_for_windows(2);
    assert_eq!(back.placements.len(), 2, "it maps again");
    let _ = keeper;
}

#[test]
fn a_window_destroyed_before_it_ever_drew_leaves_no_cell_behind() {
    // The other half of the same confusion: a window is given its cell when it
    // appears, so that its first paint is already the right size. It was only
    // taken back out of the tree if it had drawn -- so a toplevel created and
    // destroyed without ever painting left a cell with nothing to remove it.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    client.map_window("real");
    compositor.wait_for_windows(1);

    let never = client.create_toplevel_without_buffer("never draws");
    client.close_window(never);

    let after = compositor.wait_for_windows(1);
    assert_eq!(
        after.placements[0].rect.w,
        1920 - 8,
        "the window that drew should have the whole work area"
    );
}
