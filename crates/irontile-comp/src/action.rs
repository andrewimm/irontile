//! Executing an action.
//!
//! One implementation serves both key bindings and the control socket, so a
//! binding and the equivalent `irontilectl` invocation cannot drift apart.

use irontile_ipc::Action;
use irontile_layout::{Command, Event};

use crate::state::Irontile;

/// Carries out an action, reporting whatever the layout engine did.
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
