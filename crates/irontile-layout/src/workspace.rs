//! Desktops.
//!
//! A workspace is one discrete desktop: a tiling tree plus the windows that sit
//! outside it. There is no limit on how many exist, and a workspace is not tied
//! to a display — it only remembers which display it would rather be on.

use serde::{Deserialize, Serialize};

use crate::geom::Rect;
use crate::id::{OutputId, WindowId, WorkspaceId};
use crate::tree::Tree;

/// A window that sits outside the tiling tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Floating {
    pub window: WindowId,
    /// Position in the global logical coordinate space.
    pub rect: Rect,
}

/// One discrete desktop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: Option<String>,
    pub tree: Tree,
    /// Floating windows in stacking order, bottom first. The vector order *is*
    /// the z-order, so raising a window is a move to the end and there is no
    /// separate counter to keep consistent or to overflow.
    pub floating: Vec<Floating>,
    /// The window currently covering the whole display, if any. It stays in the
    /// tree or the floating list; this only changes how it is placed.
    pub fullscreen: Option<WindowId>,
    /// Set when focus is on a floating window; otherwise focus is whatever the
    /// tree says.
    pub focused_floating: Option<WindowId>,
    /// Where this desktop returns to when that display is reconnected.
    pub preferred_output: Option<OutputId>,
}

impl Workspace {
    pub fn new(id: WorkspaceId) -> Self {
        Self {
            id,
            name: None,
            tree: Tree::new(),
            floating: Vec::new(),
            fullscreen: None,
            focused_floating: None,
            preferred_output: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty() && self.floating.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tree.len() + self.floating.len()
    }

    pub fn contains(&self, window: WindowId) -> bool {
        self.tree.contains(window) || self.is_floating(window)
    }

    pub fn is_floating(&self, window: WindowId) -> bool {
        self.floating.iter().any(|f| f.window == window)
    }

    pub fn float_of(&self, window: WindowId) -> Option<&Floating> {
        self.floating.iter().find(|f| f.window == window)
    }

    /// Every window, tiled first in tree order, then floating bottom-to-top.
    pub fn windows(&self) -> Vec<WindowId> {
        let mut out: Vec<WindowId> = self.tree.leaves().into_iter().map(|(_, w)| w).collect();
        out.extend(self.floating.iter().map(|f| f.window));
        out
    }

    pub fn focused_window(&self) -> Option<WindowId> {
        self.focused_floating
            .filter(|&w| self.is_floating(w))
            .or_else(|| self.tree.focused_window())
    }

    /// Moves a floating window to the top of the stack.
    pub(crate) fn raise(&mut self, window: WindowId) {
        if let Some(i) = self.floating.iter().position(|f| f.window == window) {
            let f = self.floating.remove(i);
            self.floating.push(f);
        }
    }

    /// Drops every trace of a window that is leaving this desktop.
    pub(crate) fn forget(&mut self, window: WindowId) {
        self.floating.retain(|f| f.window != window);
        if self.fullscreen == Some(window) {
            self.fullscreen = None;
        }
        if self.focused_floating == Some(window) {
            self.focused_floating = None;
        }
    }
}
