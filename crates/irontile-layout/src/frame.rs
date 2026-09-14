//! The declarative snapshot the compositor renders.

use serde::{Deserialize, Serialize};

use crate::geom::Rect;
use crate::id::{OutputId, WindowId, WorkspaceId};
use crate::layout::Layout;
use crate::tiling::apply_with;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementKind {
    Tiled,
    Floating,
    Fullscreen,
}

/// Where one window goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub window: WindowId,
    /// The whole cell, in global logical coordinates. The border stroke is the
    /// compositor's business: it insets this rectangle by whatever width it
    /// draws, so the layout engine never has to know a border exists.
    pub rect: Rect,
    pub output: OutputId,
    pub workspace: WorkspaceId,
    pub kind: PlacementKind,
    /// Stacking order within the output, lowest first.
    pub z: u32,
    pub focused: bool,
}

/// Everything that should currently be on screen.
///
/// This is a complete snapshot rather than a diff. The compositor compares it
/// against what it last applied and configures only what changed, which keeps
/// the layout engine free of any notion of surface state and makes the whole
/// engine replaceable behind a serialization boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub placements: Vec<Placement>,
    pub focused: Option<WindowId>,
}

impl Frame {
    pub fn placement(&self, window: WindowId) -> Option<&Placement> {
        self.placements.iter().find(|p| p.window == window)
    }

    pub fn on_output(&self, output: OutputId) -> impl Iterator<Item = &Placement> + '_ {
        self.placements.iter().filter(move |p| p.output == output)
    }
}

/// Computes the current frame.
pub fn frame(layout: &Layout) -> Frame {
    let focused = layout.focused_window();
    let params = layout.config().params;
    let mut placements = Vec::new();

    for output in layout.outputs() {
        let Some(ws_id) = layout.active_workspace(output.id) else {
            continue;
        };
        let Some(workspace) = layout.workspace(ws_id) else {
            continue;
        };
        let full = workspace.fullscreen;

        // Tiled and floating windows are still placed while something is
        // fullscreen. They are occluded rather than unmapped, which spares every
        // window behind the fullscreen one a resize round trip on the way in and
        // on the way back out.
        for (window, rect) in apply_with(&workspace.tree, output.work_area, &params) {
            if full == Some(window) {
                continue;
            }
            placements.push(Placement {
                window,
                rect,
                output: output.id,
                workspace: ws_id,
                kind: PlacementKind::Tiled,
                z: 0,
                focused: focused == Some(window),
            });
        }
        for (i, f) in workspace.floating.iter().enumerate() {
            if full == Some(f.window) {
                continue;
            }
            placements.push(Placement {
                window: f.window,
                rect: f.rect,
                output: output.id,
                workspace: ws_id,
                kind: PlacementKind::Floating,
                z: (i as u32).saturating_add(1),
                focused: focused == Some(f.window),
            });
        }
        if let Some(window) = full {
            placements.push(Placement {
                window,
                // The whole display, not the work area: a fullscreen window
                // covers the bars too.
                rect: output.logical,
                output: output.id,
                workspace: ws_id,
                kind: PlacementKind::Fullscreen,
                z: u32::MAX,
                focused: focused == Some(window),
            });
        }
    }

    Frame {
        placements,
        focused,
    }
}
