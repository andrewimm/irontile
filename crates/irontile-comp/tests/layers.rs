//! Layer-shell tests.
//!
//! The behaviour that matters is the connection between a panel's exclusive
//! zone and where windows are allowed to go: a bar at the top of a display must
//! shrink the work area, and give it back when it goes away.

mod harness;

use harness::Compositor;
use irontile_ipc::{Action, LayerKind};

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

#[test]
fn a_bar_drawing_another_frame_is_not_news() {
    // A bar redraws whenever its clock ticks. If each of those commits made the
    // compositor configure the surface again, or announce that the displays
    // had changed, the bar would redraw because it had just drawn -- and the
    // two would chase each other for as long as the session lasted, at whatever
    // rate the machine could manage. It did, at three hundred frames a second.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let bar = client.map_top_bar(BAR, BAR);

    let settled = client.layer_configures(bar);
    compositor
        .client
        .subscribe()
        .expect("could not subscribe to events");
    compositor
        .client
        .set_timeout(Some(std::time::Duration::from_millis(150)))
        .expect("could not set a timeout");

    for _ in 0..20 {
        client.redraw_layer(bar);
    }

    assert_eq!(
        client.layer_configures(bar),
        settled,
        "nothing about the surface changed, so it needed no reconfiguring"
    );

    let mut announced = Vec::new();
    while let Ok(event) = compositor.client.next_event() {
        announced.push(event);
    }
    assert!(
        announced.is_empty(),
        "twenty frames of the same bar said nothing new: {announced:?}"
    );
}

#[test]
fn a_panel_is_told_the_exact_scale_of_the_display_it_is_on() {
    // A bar is mostly text, and text is what suffers most from being drawn at
    // one scale and resampled to another. A panel belongs to a display outright
    // rather than through a placement, so it was the one kind of surface never
    // told anything: it rendered at scale one and was stretched to fit.
    //
    // Two displays at different scales, with the bar on the one that is not
    // focused. With a single display, or with the bar on the focused one, the
    // fallback for "no idea, use the focused display" gives the right answer by
    // accident and the test proves nothing.
    let config = harness::TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        scale = 1.3333
        position = [0, 0]

        [[output]]
        name = "HEADLESS-2"
        scale = 2.0
        position = [1692, 0]
        "#,
    );
    let compositor = Compositor::with_config("2256x1504,2560x1440", Some(config.path()));
    let mut client = compositor.connect_client();
    assert_eq!(client.display_count(), 2, "both displays were advertised");

    let bar = client.map_top_bar_on(1, BAR, BAR);
    client.wait_for(|client| client.layer_fractional_scale(bar).is_some());
    assert_eq!(
        client.layer_fractional_scale(bar),
        Some(240),
        "the second display's scale of two, in the 120ths the protocol counts in"
    );

    // And the other way round, so neither number can be the one it always says.
    let other = client.map_top_bar_on(0, BAR, BAR);
    client.wait_for(|client| client.layer_fractional_scale(other).is_some());
    assert_eq!(
        client.layer_fractional_scale(other),
        Some(160),
        "four thirds on the first display"
    );
}

#[test]
fn a_panel_receives_the_pointer() {
    // The bug this exists for: the lookup for what is under the pointer walked
    // only the tiled windows, so a bar never received an enter, a motion or a
    // click. Everything built on top of that -- clicking a desktop button,
    // resting on a module for a tooltip -- was dead, and nothing said so
    // because nothing in a test could move a pointer.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let bar = client.map_top_bar(BAR, BAR);

    // Onto the bar. The test client's buffer is small, so this stays within it:
    // what is under the pointer is the surface's own content, not the rectangle
    // the layer map reserved for it.
    let (x, y) = (32, BAR / 2);
    compositor
        .client
        .action(Action::WarpPointer(x, y))
        .expect("the compositor refused to move the pointer");
    client.wait_for(|client| client.pointer_on_layer(bar));
    let (_, at) = client.pointer_on().expect("the pointer is on the bar");
    assert!(
        (at.0 - f64::from(x)).abs() < 1.0 && (at.1 - f64::from(y)).abs() < 1.0,
        "and at the point it was sent to, in the panel's own coordinates: {at:?}"
    );

    // A click lands on it rather than on whatever is behind.
    compositor
        .client
        .action(Action::ClickPointer(1))
        .expect("the compositor refused to click");
    client.wait_for(|client| !client.buttons().is_empty());
    assert_eq!(client.buttons(), vec![0x110], "the left button");

    // And below the bar it is somebody else's pointer.
    compositor
        .client
        .action(Action::WarpPointer(960, 540))
        .expect("warp");
    client.wait_for(|client| !client.pointer_on_layer(bar));
}

#[test]
fn the_panels_on_screen_can_be_asked_about() {
    // A layer surface is neither a window nor a display, so nothing else the
    // socket reports describes one. Without this, a notification daemon or a
    // bar that fails to appear leaves nothing to look at but the screen it is
    // not on.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    assert!(
        layers(&mut compositor).is_empty(),
        "nothing has mapped a panel yet"
    );

    let _bar = client.map_top_bar(BAR, BAR);
    let _launcher = client.map_launcher(200);

    let found = layers(&mut compositor);
    assert_eq!(found.len(), 2, "both panels: {found:?}");

    let bar = found
        .iter()
        .find(|l| l.namespace == "irontile-test-bar")
        .expect("the bar");
    assert_eq!(bar.layer, LayerKind::Top);
    // The rectangle is what the surface actually occupies rather than what it
    // asked for, so a client whose buffer does not match its request is
    // reported where it really is.
    assert_eq!(bar.rect.x, 0);
    assert_eq!(bar.rect.y, 0);
    assert_eq!(bar.exclusive, BAR, "it reserves its own height");
    assert!(!bar.keyboard, "a bar is not typed into");

    let launcher = found
        .iter()
        .find(|l| l.namespace == "irontile-test-launcher")
        .expect("the launcher");
    assert_eq!(launcher.exclusive, 0, "a launcher reserves nothing");
    assert!(
        launcher.keyboard,
        "it asked for the keyboard, which is what a launcher is for"
    );
}

fn layers(compositor: &mut Compositor) -> Vec<irontile_ipc::LayerInfo> {
    match compositor.client.query(irontile_ipc::Query::Layers) {
        Ok(irontile_ipc::ResponsePayload::Layers(layers)) => layers,
        other => panic!("expected panels, got {other:?}"),
    }
}

#[test]
fn a_panel_that_hides_itself_can_come_back() {
    // What a notification centre does every time it is toggled: it attaches no
    // buffer to hide, and attaches one again to show. Layer-shell says an
    // unmapped surface returns to its initial state and may not attach another
    // buffer until it has been configured afresh -- so a compositor that only
    // configures when its own idea of the surface has changed sends nothing,
    // and the panel waits for ever. It opens once, closes, and never opens
    // again.
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let bar = client.map_top_bar(BAR, BAR);
    let shown = |c: &mut Compositor| layers(c).iter().any(|l| l.namespace == "irontile-test-bar");
    assert!(shown(&mut compositor), "it is up to begin with");

    for round in 1..=3 {
        client.unmap_layer(bar);
        assert!(!shown(&mut compositor), "hidden on round {round}");
        // This is the part that hangs without the configure.
        client.remap_layer(bar);
        assert!(shown(&mut compositor), "shown again on round {round}");
    }
}

#[test]
fn a_panel_gets_its_frame_callbacks_back() {
    // A toolkit asks for one of these and waits for it before drawing its next
    // frame. A panel that never receives one draws exactly once and then stops,
    // which looks like a client that renders badly rather than a compositor
    // that never answered -- and irontile's own bar draws on a schedule of its
    // own, so it is the one panel that would never have shown this.
    let compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let bar = client.map_top_bar(BAR, BAR);

    let before = client.frames(bar);
    client.request_frame(bar);
    client.wait_for(|client| client.frames(bar) > before);
}
