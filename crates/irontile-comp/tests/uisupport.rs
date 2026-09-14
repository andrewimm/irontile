//! What a bar and a launcher need from the compositor.
//!
//! Both are ordinary layer-shell clients with no privileged access, so
//! everything they know arrives over the control socket or through the
//! protocol. These cover the two things that were missing: window metadata, and
//! a layer surface being able to hold the keyboard.

mod harness;

use harness::Compositor;
use irontile_ipc::{Query, ResponsePayload, WindowInfo};

fn windows(compositor: &mut Compositor) -> Vec<WindowInfo> {
    match compositor.client.query(Query::Windows).unwrap() {
        ResponsePayload::Windows(windows) => windows,
        other => panic!("expected windows, got {other:?}"),
    }
}

#[test]
fn windows_report_what_they_call_themselves() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    client.map_window("editor");
    client.map_window("browser");
    compositor.wait_for_windows(2);

    let mut listed = windows(&mut compositor);
    listed.sort_by_key(|w| w.id.0);
    assert_eq!(listed.len(), 2);

    // Without these a bar can list windows but not name them, which is most of
    // the point of listing them.
    assert_eq!(listed[0].title.as_deref(), Some("editor"));
    assert_eq!(listed[1].title.as_deref(), Some("browser"));
    assert_eq!(listed[0].app_id.as_deref(), Some("irontile.test.editor"));

    // And where each one is, so a bar can group them by desktop.
    assert_eq!(listed[0].workspace, listed[1].workspace);
    assert!(listed[0].output.is_some());
    assert_eq!(listed.iter().filter(|w| w.focused).count(), 1);
}

#[test]
fn a_window_on_a_hidden_desktop_reports_no_display() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    client.map_window("tucked away");
    compositor.wait_for_windows(1);

    compositor
        .client
        .action(irontile_ipc::Action::MoveToWorkspace(9))
        .unwrap();

    let listed = windows(&mut compositor);
    assert_eq!(listed.len(), 1, "it still exists");
    assert_eq!(listed[0].output, None, "but it is not on any display");
}

#[test]
fn a_renamed_window_is_announced() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();
    let window = client.map_window("before");
    compositor.wait_for_windows(1);

    compositor.client.subscribe().unwrap();
    client.set_title(window, "after");

    // A title changes while nothing else does, so polling for it would mean
    // polling constantly. Each round trip pumps the connection, which is what
    // moves events off the socket and into the client's queue.
    let mut seen = Vec::new();
    compositor.wait_for(|c| {
        let _ = c.client.frame();
        seen.extend(c.client.drain_events());
        seen.iter()
            .any(|e| matches!(e, irontile_ipc::Event::WindowRenamed { .. }))
    });
    let listed = windows(&mut compositor);
    assert_eq!(listed[0].title.as_deref(), Some("after"));
}

#[test]
fn a_launcher_can_hold_the_keyboard() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let window = client.map_window("editor");
    compositor.wait_for_windows(1);
    assert!(
        client.window_has_keyboard(window),
        "the window starts focused"
    );

    // A layer surface asking for the keyboard outranks the tiling tree. Without
    // this a launcher could not read a single keystroke.
    let launcher = client.map_launcher(200);
    client.wait_for(|c| c.layer_has_keyboard(launcher));
    assert!(!client.window_has_keyboard(window));

    // And the window gets it back when the launcher goes away.
    client.close_layer(launcher);
    client.wait_for(|c| c.window_has_keyboard(window));
}

#[test]
fn a_bar_does_not_steal_the_keyboard() {
    let mut compositor = Compositor::start("1920x1080");
    let mut client = compositor.connect_client();

    let window = client.map_window("editor");
    compositor.wait_for_windows(1);

    // A bar asks for no keyboard interactivity, so typing must keep going to
    // the window underneath.
    let bar = client.map_top_bar(30, 30);
    compositor.wait_for(|c| c.client.layout().unwrap().outputs()[0].work_area.h == 1080 - 30);
    assert!(client.window_has_keyboard(window));
    assert!(!client.layer_has_keyboard(bar));
}
