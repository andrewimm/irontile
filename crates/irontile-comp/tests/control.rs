//! End-to-end tests driving a real compositor over its control socket.
//!
//! These cover the ground the layout engine's own tests cannot reach: that the
//! compositor wires displays, desktops and configuration to the engine
//! correctly, and that the arrangement survives displays coming and going.

mod harness;

use harness::Compositor;
use irontile_ipc::{Action, Command, Output, OutputId, Query, Rect, ResponsePayload};

fn outputs(compositor: &mut Compositor) -> Vec<Output> {
    match compositor.client.query(Query::Outputs).unwrap() {
        ResponsePayload::Outputs(outputs) => outputs,
        other => panic!("expected outputs, got {other:?}"),
    }
}

fn workspaces(compositor: &mut Compositor) -> Vec<irontile_ipc::WorkspaceSummary> {
    match compositor.client.query(Query::Workspaces).unwrap() {
        ResponsePayload::Workspaces(w) => w,
        other => panic!("expected workspaces, got {other:?}"),
    }
}

fn on_output(compositor: &mut Compositor, output: u64) -> Option<String> {
    workspaces(compositor)
        .into_iter()
        .find(|w| w.output == Some(OutputId(output)))
        .and_then(|w| w.name)
}

#[test]
fn a_compositor_comes_up_and_answers_queries() {
    let mut compositor = Compositor::start("1920x1080");
    let outputs = outputs(&mut compositor);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].logical, Rect::new(0, 0, 1920, 1080));

    let layout = compositor.client.layout().unwrap();
    // The whole engine state crosses the socket, so a client can check it.
    layout.validate().unwrap();
}

#[test]
fn every_display_comes_up_with_its_own_numbered_desktop() {
    let mut compositor = Compositor::start("1920x1080,1280x1024");
    let outputs = outputs(&mut compositor);
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[1].logical, Rect::new(1920, 0, 1280, 1024));

    let workspaces = workspaces(&mut compositor);
    assert_eq!(workspaces.len(), 2);
    // Numbers exist so the `workspace N` bindings have something to address.
    let mut names: Vec<_> = workspaces.iter().filter_map(|w| w.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["1", "2"]);
    assert_eq!(workspaces.iter().filter(|w| w.focused).count(), 1);
}

#[test]
fn a_desktop_sent_to_another_display_steals_it_and_leaves_a_new_one() {
    let mut compositor = Compositor::start("1920x1080,1280x1024");
    assert_eq!(on_output(&mut compositor, 1).as_deref(), Some("1"));
    assert_eq!(on_output(&mut compositor, 2).as_deref(), Some("2"));

    compositor
        .client
        .action(Action::SendToOutput(right()))
        .unwrap();

    // Desktop 1 took over the second display.
    assert_eq!(on_output(&mut compositor, 2).as_deref(), Some("1"));
    // The display it left is not blank, and not showing the desktop it
    // displaced either: it gets a new one.
    let left = on_output(&mut compositor, 1).expect("the first display still shows something");
    assert_ne!(left, "1");

    compositor.client.layout().unwrap().validate().unwrap();
}

#[test]
fn numeric_bindings_create_desktops_on_first_use() {
    let mut compositor = Compositor::start("1920x1080");
    assert_eq!(workspaces(&mut compositor).len(), 1);

    compositor.client.action(Action::Workspace(7)).unwrap();
    assert_eq!(on_output(&mut compositor, 1).as_deref(), Some("7"));

    // Desktop 1 was empty and is now off screen, so it is gone; 7 replaced it.
    let names: Vec<_> = workspaces(&mut compositor)
        .into_iter()
        .filter_map(|w| w.name)
        .collect();
    assert_eq!(names, vec!["7"]);

    // Going back creates it again, which is what "infinite desktops" means.
    compositor.client.action(Action::Workspace(1)).unwrap();
    assert_eq!(on_output(&mut compositor, 1).as_deref(), Some("1"));
}

#[test]
fn unplugging_a_display_parks_its_desktop_and_replugging_restores_it() {
    let mut compositor = Compositor::start("1920x1080,1280x1024");
    // Put a recognisable desktop on the second display.
    compositor
        .client
        .action(Action::FocusOutput(right()))
        .unwrap();
    compositor.client.action(Action::Workspace(9)).unwrap();
    assert_eq!(on_output(&mut compositor, 2).as_deref(), Some("9"));

    let both = outputs(&mut compositor);
    let first = vec![both[0].clone()];

    // Unplug.
    compositor
        .client
        .command(Command::ReconfigureOutputs { outputs: first })
        .unwrap();
    let after = outputs(&mut compositor);
    assert_eq!(after.len(), 1);
    // The desktop survives with no display of its own.
    let parked = workspaces(&mut compositor)
        .into_iter()
        .find(|w| w.name.as_deref() == Some("9"))
        .expect("desktop 9 still exists");
    assert_eq!(parked.output, None);
    compositor.client.layout().unwrap().validate().unwrap();

    // Replug.
    compositor
        .client
        .command(Command::ReconfigureOutputs { outputs: both })
        .unwrap();
    assert_eq!(outputs(&mut compositor).len(), 2);
    assert_eq!(on_output(&mut compositor, 2).as_deref(), Some("9"));
    compositor.client.layout().unwrap().validate().unwrap();
}

#[test]
fn no_display_is_ever_left_showing_nothing() {
    let mut compositor = Compositor::start("1920x1080,1280x1024,800x600");
    // Pull every desktop onto one display in turn, which is the sequence that
    // would leave the others blank if backfilling were wrong.
    for number in [1, 2, 3, 4, 5] {
        compositor.client.action(Action::Workspace(number)).unwrap();
        let displayed: Vec<_> = workspaces(&mut compositor)
            .into_iter()
            .filter(|w| w.output.is_some())
            .collect();
        assert_eq!(displayed.len(), 3, "after showing {number}");
        compositor.client.layout().unwrap().validate().unwrap();
    }
}

#[test]
fn a_rejected_command_is_reported_rather_than_silently_dropped() {
    let mut compositor = Compositor::start("1920x1080");
    let err = compositor
        .client
        .command(Command::DestroyWorkspace {
            workspace: irontile_ipc::WorkspaceId(9999),
        })
        .unwrap_err();
    assert!(format!("{err}").contains("9999"), "{err}");
    // The compositor is still healthy afterwards.
    compositor.client.layout().unwrap().validate().unwrap();
}

#[test]
fn the_event_stream_reports_what_changed() {
    let mut compositor = Compositor::start("1920x1080");
    compositor.client.subscribe().unwrap();
    compositor.client.action(Action::Workspace(4)).unwrap();

    // The subscription and the reply share one connection, and the compositor
    // writes the events before the reply. They must survive that rather than
    // being swallowed while the client waits for its response.
    let seen = compositor.client.drain_events();
    assert!(
        seen.iter()
            .any(|e| matches!(e, irontile_ipc::Event::WorkspaceShown { .. })),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, irontile_ipc::Event::WorkspaceCreated { .. })),
        "{seen:?}"
    );
}

fn right() -> irontile_layout::Direction {
    irontile_layout::Direction::Right
}
