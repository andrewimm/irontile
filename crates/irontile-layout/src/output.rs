//! Displays and their arrangement.

use serde::{Deserialize, Serialize};

use crate::geom::Rect;
use crate::id::OutputId;

/// A connected display.
///
/// The arrangement of displays is nothing more than their rectangles in a
/// shared coordinate space. That is what lets directional focus and directional
/// window movement cross between displays using the same geometry as they use
/// inside a single tree, with no separate notion of "which monitor is to the
/// left of which".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub id: OutputId,
    /// Stable identifier for the physical display, used for logging and for
    /// addressing an output from the control socket.
    pub name: String,
    /// Position and size in the global logical coordinate space.
    pub logical: Rect,
    /// `logical` minus exclusive zones such as bars. Supplied by the
    /// compositor, which is the only thing that knows about layer-shell.
    pub work_area: Rect,
}

impl Output {
    /// A display whose whole area is usable.
    pub fn new(id: OutputId, name: impl Into<String>, logical: Rect) -> Self {
        Self {
            id,
            name: name.into(),
            logical,
            work_area: logical,
        }
    }

    pub fn with_work_area(mut self, work_area: Rect) -> Self {
        self.work_area = work_area;
        self
    }
}
