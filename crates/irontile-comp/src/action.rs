//! Executing an action.
//!
//! One implementation serves both key bindings and the control socket, so a
//! binding and the equivalent `irontilectl` invocation cannot drift apart.

use irontile_ipc::Action;
use irontile_layout::{Command, Event};

use crate::state::Irontile;

/// Carries out an action, reporting whatever the layout engine did.
/// Whether a binding may fire while the session is locked.
///
/// A locked session means the keyboard belongs to the lock screen and to
/// nothing else. Leaving the bindings live undoes that completely: `spawn`
/// starts a program behind the lock screen, `close` destroys windows nobody
/// can see, and quitting the compositor hands back the terminal the session
/// was started from -- which on a machine launched from a virtual terminal is
/// an authenticated shell, and the whole lock defeated by one keypress.
///
/// Two exceptions in opposite directions. Switching virtual terminal always
/// works: a compositor holding the VT is the only thing that can perform the
/// switch, the terminal switched to asks for a login of its own, and taking it
/// away would remove the last way out of a session that has gone wrong.
/// Quitting never works, whatever a configuration file says, because there is
/// no arrangement in which ending the session from its lock screen is what
/// somebody meant.
pub fn fires_while_locked(action: &Action, opted_in: bool) -> bool {
    match action {
        Action::SwitchVt(_) => true,
        Action::Quit => false,
        _ => opted_in,
    }
}

pub fn perform(state: &mut Irontile, action: &Action) -> Vec<Event> {
    match action {
        Action::Focus(dir) => state.apply(Command::FocusDirection { dir: *dir }),
        Action::MoveWindow(dir) => state.apply(Command::MoveWindow {
            window: None,
            dir: *dir,
        }),
        Action::Resize(dir) => {
            let delta_px = state.config.theme.resize_step;
            state.apply(Command::Resize {
                window: None,
                dir: *dir,
                delta_px,
            })
        }
        Action::FocusOutput(dir) => state.apply(Command::FocusOutputDirection { dir: *dir }),
        Action::SendToOutput(dir) => state.send_workspace_to_output(*dir),
        Action::Split(axis) => state.apply(Command::SetAxis {
            window: None,
            axis: *axis,
        }),
        Action::Equalize => state.apply(Command::Equalize { window: None }),
        Action::ToggleFloating => state.apply(Command::SetFloating {
            window: None,
            floating: None,
        }),
        Action::ToggleFullscreen => state.apply(Command::SetFullscreen {
            window: None,
            fullscreen: None,
        }),
        Action::Close => {
            state.close_focused();
            Vec::new()
        }
        Action::Workspace(number) => {
            let workspace = state.workspace_by_number(*number);
            state.apply(Command::ShowWorkspace {
                workspace,
                output: None,
            })
        }
        Action::WorkspaceStep(step) => {
            let workspace = state.workspace_step(*step);
            state.apply(Command::ShowWorkspace {
                workspace,
                output: None,
            })
        }
        Action::MoveToWorkspace(number) => {
            let workspace = state.workspace_by_number(*number);
            state.apply(Command::MoveWindowToWorkspace {
                window: None,
                workspace,
                follow: false,
            })
        }
        Action::Spawn(argv) => {
            state.spawn(argv);
            Vec::new()
        }
        Action::SwitchVt(vt) => {
            state.backend.change_vt(*vt);
            Vec::new()
        }
        Action::Reload => {
            state.reload_config();
            Vec::new()
        }
        Action::Quit => {
            state.running = false;
            Vec::new()
        }
        Action::WarpPointer(x, y) => {
            crate::input::warp(state, f64::from(*x), f64::from(*y));
            Vec::new()
        }
        Action::ClickPointer(button) => {
            crate::input::click(state, *button);
            Vec::new()
        }
        Action::PressPointer(button) => {
            crate::input::press(state, *button, true);
            Vec::new()
        }
        Action::ReleasePointer(button) => {
            crate::input::press(state, *button, false);
            Vec::new()
        }
        // The vocabulary is `#[non_exhaustive]`; an action added to the
        // protocol that this compositor does not know is a no-op rather than a
        // crash.
        other => {
            tracing::warn!(action = %other, "unsupported action");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod locked_tests {
    use super::fires_while_locked;
    use irontile_ipc::{Action, Direction};

    #[test]
    fn quitting_the_compositor_is_never_a_thing_a_lock_screen_does() {
        // The hole this was written for: a session started from a virtual
        // terminal leaves an authenticated shell behind it, so quitting at the
        // lock screen hands the machine to whoever pressed the key. Opting in
        // must not buy it back.
        assert!(!fires_while_locked(&Action::Quit, false));
        assert!(!fires_while_locked(&Action::Quit, true));
    }

    #[test]
    fn switching_terminal_always_works_because_it_is_the_way_out() {
        // The terminal switched to asks for a login of its own, so this gives
        // nothing away -- and it is the last way into a session whose lock
        // screen has stopped answering.
        assert!(fires_while_locked(&Action::SwitchVt(2), false));
    }

    #[test]
    fn nothing_else_fires_unless_it_was_asked_to() {
        for action in [
            Action::Spawn(vec!["kitty".to_string()]),
            Action::Close,
            Action::Workspace(3),
            Action::Focus(Direction::Left),
            Action::Reload,
        ] {
            assert!(
                !fires_while_locked(&action, false),
                "{action:?} fired at a locked screen without being asked to"
            );
        }
    }

    #[test]
    fn a_volume_key_can_be_asked_to_keep_working() {
        // What the opt-in is for: the keys somebody expects to work while the
        // screen is locked are the ones that change nothing about the session.
        let volume = Action::Spawn(vec!["wpctl".to_string(), "set-volume".to_string()]);
        assert!(fires_while_locked(&volume, true));
    }
}
