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

#[test]
fn a_subscriber_that_falls_behind_is_waited_for_rather_than_cut_off() {
    // A bar repainting two displays takes longer than the compositor takes to
    // report the next desktop switch, so a burst fills its socket. Writing
    // anyway would put half a frame into the stream and desynchronize it for
    // good; giving up on it means a bar disappears for being one repaint
    // behind. It has to be held instead.
    let mut compositor = Compositor::start("1920x1080");
    let mut idle = compositor.connect_control();
    idle.subscribe().expect("could not subscribe");
    idle.set_timeout(Some(std::time::Duration::from_millis(250)))
        .expect("could not set a timeout");

    // Far more than a socket buffer will hold, with nothing reading them.
    for n in 0..400 {
        compositor
            .client
            .action(Action::Workspace(n % 9 + 1))
            .expect("the compositor stopped answering");
    }

    // It should still be there, and still have the backlog to say.
    let mut received = 0;
    while idle.next_event().is_ok() {
        received += 1;
    }
    assert!(
        received > 0,
        "the subscriber was dropped for being slow instead of waited for"
    );

    // And once it has caught up the connection is an ordinary one again, which
    // is what says the stream never lost its framing.
    idle.set_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("could not set a timeout");
    assert!(
        matches!(idle.query(Query::Outputs), Ok(ResponsePayload::Outputs(_))),
        "the connection came out of the burst still speaking the protocol"
    );
}

#[test]
fn spawned_programs_do_not_pile_up_as_zombies() {
    // Nothing waits on a program a binding starts, so every one of them stays
    // in the process table until something does. A binding that repeats -- a
    // volume key held down -- starts one every forty milliseconds, and when the
    // table fills nothing can be started at all: the binding that worked an
    // hour ago silently does nothing.
    let mut compositor = Compositor::start("1920x1080");
    for _ in 0..40 {
        compositor
            .client
            .action(Action::Spawn(vec!["true".into()]))
            .expect("the compositor refused to spawn");
    }

    // They have to be given a moment to exit before they can be counted.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // One more spawn is what clears the finished ones out.
        compositor
            .client
            .action(Action::Spawn(vec!["true".into()]))
            .expect("spawn");
        let zombies = zombies_of(compositor.pid());
        if zombies <= 2 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{zombies} of them are still sitting in the process table"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// How many of a process's children have exited without being waited for.
fn zombies_of(parent: u32) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
                return false;
            };
            let field = |key: &str| {
                status
                    .lines()
                    .find(|line| line.starts_with(key))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .map(str::to_owned)
            };
            field("PPid:").as_deref() == Some(&parent.to_string())
                && field("State:").as_deref() == Some("Z")
        })
        .count()
}
