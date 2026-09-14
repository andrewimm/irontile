//! The serializable boundary.
//!
//! The compositor drives the layout engine entirely through [`Command`],
//! [`Event`], and [`crate::frame`]. Nothing crosses that boundary but plain
//! data: no handles, no callbacks, no borrowed state. That is what makes it
//! possible to move the engine behind a serialization boundary later without
//! the compositor noticing.
//!
//! Commands that act on a window take `Option<WindowId>`, where `None` means
//! the focused window. A command aimed at nothing in particular when nothing is
//! focused is a no-op rather than an error, because that is what a keybinding
//! pressed on an empty desktop should do.

use serde::{Deserialize, Serialize};

use crate::error::LayoutError;
use crate::geom::{Axis, Direction, Rect};
use crate::id::{OutputId, WindowId, WorkspaceId};
use crate::layout::{Config, Layout};
use crate::output::Output;
use crate::tree::InsertTarget;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Command {
    AddWindow {
        window: WindowId,
        workspace: Option<WorkspaceId>,
        target: InsertTarget,
    },
    RemoveWindow {
        window: WindowId,
    },
    FocusWindow {
        window: WindowId,
    },
    FocusDirection {
        dir: Direction,
    },
    FocusOutput {
        output: OutputId,
    },
    FocusOutputDirection {
        dir: Direction,
    },
    MoveWindow {
        window: Option<WindowId>,
        dir: Direction,
    },
    MoveWindowToWorkspace {
        window: Option<WindowId>,
        workspace: WorkspaceId,
        follow: bool,
    },
    SwapWindows {
        a: WindowId,
        b: WindowId,
    },
    Resize {
        window: Option<WindowId>,
        dir: Direction,
        delta_px: i32,
    },
    Equalize {
        window: Option<WindowId>,
    },
    /// `axis: None` toggles.
    SetAxis {
        window: Option<WindowId>,
        axis: Option<Axis>,
    },
    /// `floating: None` toggles.
    SetFloating {
        window: Option<WindowId>,
        floating: Option<bool>,
    },
    MoveFloating {
        window: Option<WindowId>,
        rect: Rect,
    },
    /// `fullscreen: None` toggles.
    SetFullscreen {
        window: Option<WindowId>,
        fullscreen: Option<bool>,
    },
    CreateWorkspace {
        name: Option<String>,
    },
    DestroyWorkspace {
        workspace: WorkspaceId,
    },
    RenameWorkspace {
        workspace: WorkspaceId,
        name: Option<String>,
    },
    /// Display a desktop, taking over the output. `output: None` means the
    /// focused output.
    ShowWorkspace {
        workspace: WorkspaceId,
        output: Option<OutputId>,
    },
    SwapOutputWorkspaces {
        a: OutputId,
        b: OutputId,
    },
    ReconfigureOutputs {
        outputs: Vec<Output>,
    },
    SetConfig {
        config: Config,
    },
}

/// What changed. The compositor uses these to decide what to tell clients and
/// what to redraw; recomputing the frame is cheap enough that `LayoutChanged`
/// carries no detail of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    WindowAdded {
        window: WindowId,
        workspace: WorkspaceId,
    },
    WindowRemoved {
        window: WindowId,
    },
    /// A window changed what it calls itself. An event rather than something to
    /// poll for, because a title changes while nothing else does.
    WindowRenamed {
        window: WindowId,
    },
    WindowMoved {
        window: WindowId,
        from: WorkspaceId,
        to: WorkspaceId,
    },
    FocusChanged {
        window: Option<WindowId>,
        output: Option<OutputId>,
    },
    WorkspaceCreated {
        workspace: WorkspaceId,
    },
    WorkspaceDestroyed {
        workspace: WorkspaceId,
    },
    WorkspaceRenamed {
        workspace: WorkspaceId,
    },
    WorkspaceShown {
        workspace: WorkspaceId,
        output: OutputId,
    },
    WorkspaceHidden {
        workspace: WorkspaceId,
    },
    OutputsChanged,
    /// Some geometry changed; recompute the frame.
    LayoutChanged,
}

/// Applies one command.
pub fn dispatch(layout: &mut Layout, command: Command) -> Result<Vec<Event>, LayoutError> {
    // Resolves `None` to the focused window, short-circuiting to a no-op when
    // there is nothing focused.
    macro_rules! target {
        ($window:expr) => {
            match $window.or_else(|| layout.focused_window()) {
                Some(w) => w,
                None => return Ok(Vec::new()),
            }
        };
    }

    match command {
        Command::AddWindow {
            window,
            workspace,
            target,
        } => layout.add_window(window, workspace, target),
        Command::RemoveWindow { window } => layout.remove_window(window),
        Command::FocusWindow { window } => layout.focus_window(window),
        Command::FocusDirection { dir } => layout.focus_direction(dir),
        Command::FocusOutput { output } => layout.focus_output(output),
        Command::FocusOutputDirection { dir } => layout.focus_output_direction(dir),
        Command::MoveWindow { window, dir } => layout.move_window_direction(target!(window), dir),
        Command::MoveWindowToWorkspace {
            window,
            workspace,
            follow,
        } => layout.move_window_to_workspace(target!(window), workspace, follow),
        Command::SwapWindows { a, b } => layout.swap_windows(a, b),
        Command::Resize {
            window,
            dir,
            delta_px,
        } => layout.resize_window(target!(window), dir, delta_px),
        Command::Equalize { window } => layout.equalize(target!(window)),
        Command::SetAxis { window, axis } => layout.set_axis(target!(window), axis),
        Command::SetFloating { window, floating } => {
            let window = target!(window);
            let next = match floating {
                Some(f) => f,
                None => !layout
                    .workspace_of(window)
                    .and_then(|ws| layout.workspace(ws))
                    .is_some_and(|ws| ws.is_floating(window)),
            };
            layout.set_floating(window, next)
        }
        Command::MoveFloating { window, rect } => layout.move_floating(target!(window), rect),
        Command::SetFullscreen { window, fullscreen } => {
            let window = target!(window);
            let next = match fullscreen {
                Some(f) => f,
                None => layout
                    .workspace_of(window)
                    .and_then(|ws| layout.workspace(ws))
                    .is_none_or(|ws| ws.fullscreen != Some(window)),
            };
            layout.set_fullscreen(window, next)
        }
        Command::CreateWorkspace { name } => {
            let workspace = layout.create_workspace(name);
            Ok(vec![Event::WorkspaceCreated { workspace }])
        }
        Command::DestroyWorkspace { workspace } => layout.destroy_workspace(workspace),
        Command::RenameWorkspace { workspace, name } => layout.rename_workspace(workspace, name),
        Command::ShowWorkspace { workspace, output } => {
            let output = match output.or_else(|| layout.focused_output()) {
                Some(o) => o,
                None => return Err(LayoutError::NoOutputs),
            };
            layout.show_workspace(workspace, output)
        }
        Command::SwapOutputWorkspaces { a, b } => layout.swap_output_workspaces(a, b),
        Command::ReconfigureOutputs { outputs } => Ok(layout.reconfigure_outputs(outputs)),
        Command::SetConfig { config } => {
            layout.set_config(config);
            Ok(vec![Event::LayoutChanged])
        }
    }
}
