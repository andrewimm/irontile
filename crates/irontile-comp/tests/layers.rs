//! Layer-shell tests.
//!
//! The behaviour that matters is the connection between a panel's exclusive
//! zone and where windows are allowed to go: a bar at the top of a display must
//! shrink the work area, and give it back when it goes away.

mod harness;

use harness::Compositor;

const BAR: i32 = 30;

#[test]
fn the_layer_shell_is_advertised() {
    let compositor = Compositor::start("1920x1080");
    let client = compositor.connect_client();
    assert!(
        client.has_layer_shell(),
        "without layer shell there can be no bar and no lock screen"
    );
}

#[test]
fn a_bar_shrinks_the_work_area() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let before = compositor.client.layout().unwrap();
    let output = before.outputs()[0].clone();
    assert_eq!(output.work_area, output.logical, "nothing has reserved yet");

    client.map_top_bar(BAR, BAR);
    compositor.wait_for(|c| c.client.layout().unwrap().outputs()[0].work_area.h == 1080 - BAR);

    let after = compositor.client.layout().unwrap();
    let output = &after.outputs()[0];
    // The display itself is unchanged; only the usable part shrank.
    assert_eq!(output.logical, irontile_ipc::Rect::new(0, 0, 1920, 1080));
    assert_eq!(
        output.work_area,
        irontile_ipc::Rect::new(0, BAR, 1920, 1080 - BAR)
    );
    after.validate().unwrap();
}

#[test]
fn windows_tile_below_a_bar() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_top_bar(BAR, BAR);
    compositor.wait_for(|c| c.client.layout().unwrap().outputs()[0].work_area.h == 1080 - BAR);

    client.map_window("one");
    let frame = compositor.wait_for_windows(1);
    let rect = frame.placements[0].rect;

    // The window starts below the bar, not under it, and the outer gap is
    // measured from the work area rather than from the display edge.
    assert_eq!(rect.y, BAR + 4);
    assert_eq!(rect.h, 1080 - BAR - 8);
    assert!(rect.y >= BAR, "a window must not go under the bar");
}

#[test]
fn removing_a_bar_gives_the_space_back() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let bar = client.map_top_bar(BAR, BAR);
    compositor.wait_for(|c| c.client.layout().unwrap().outputs()[0].work_area.h == 1080 - BAR);
    client.map_window("one");
    compositor.wait_for_windows(1);

    client.close_layer(bar);
    compositor.wait_for(|c| c.client.layout().unwrap().outputs()[0].work_area.h == 1080);

    let frame = compositor.client.frame().unwrap();
    assert_eq!(
        frame.placements[0].rect.y, 4,
        "the window should move back up"
    );
    assert_eq!(frame.placements[0].rect.h, 1080 - 8);
}

#[test]
fn a_panel_that_reserves_nothing_leaves_the_work_area_alone() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    // An overlay that does not reserve space - a notification, say - must not
    // push windows around.
    client.map_top_bar(BAR, 0);
    client.map_window("one");
    let frame = compositor.wait_for_windows(1);

    assert_eq!(frame.placements[0].rect.y, 4);
    let layout = compositor.client.layout().unwrap();
    assert_eq!(layout.outputs()[0].work_area, layout.outputs()[0].logical);
}

#[test]
fn resizing_a_display_updates_its_work_area() {
    let mut compositor = Compositor::start("1920x1080");
    // No panel has reserved anything, so the work area is the whole display.
    let before = compositor.client.layout().unwrap();
    assert_eq!(before.outputs()[0].work_area, before.outputs()[0].logical);

    let resized = vec![irontile_ipc::Output::new(
        before.outputs()[0].id,
        before.outputs()[0].name.clone(),
        irontile_ipc::Rect::new(0, 0, 1280, 720),
    )];
    compositor
        .client
        .command(irontile_ipc::Command::ReconfigureOutputs { outputs: resized })
        .unwrap();

    let after = compositor.client.layout().unwrap();
    let output = &after.outputs()[0];
    // The work area is derived from the display, so it has to follow it. It
    // would otherwise keep the size the display had when it first appeared.
    assert_eq!(output.logical, irontile_ipc::Rect::new(0, 0, 1280, 720));
    assert_eq!(output.work_area, output.logical);
}

#[test]
fn a_bar_survives_its_display_being_resized() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    client.map_top_bar(BAR, BAR);
    compositor.wait_for(|c| c.client.layout().unwrap().outputs()[0].work_area.h == 1080 - BAR);

    let id = compositor.client.layout().unwrap().outputs()[0].id;
    compositor
        .client
        .command(irontile_ipc::Command::ReconfigureOutputs {
            outputs: vec![irontile_ipc::Output::new(
                id,
                "HEADLESS-1",
                irontile_ipc::Rect::new(0, 0, 1280, 720),
            )],
        })
        .unwrap();

    compositor.wait_for(|c| c.client.layout().unwrap().outputs()[0].work_area.w == 1280);
    let after = compositor.client.layout().unwrap();
    // Still reserving its strip, now across the narrower display.
    assert_eq!(
        after.outputs()[0].work_area,
        irontile_ipc::Rect::new(0, BAR, 1280, 720 - BAR)
    );
}
