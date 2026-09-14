//! The irontile layout engine.
//!
//! Pure geometry and desktop bookkeeping, with no dependency on Wayland, on a
//! compositor toolkit, or on the machine it runs on. Everything it holds is
//! integer-valued and serializable, and everything it exposes is data in and
//! data out: [`dispatch`] applies a [`Command`] and reports [`Event`]s,
//! [`frame`] says where every window should be. No handles or callbacks cross
//! that line, which is what will let the engine move behind a serialization
//! boundary without the compositor having to change.
//!
//! The model has three layers:
//!
//! - [`Tree`] is one desktop's split-container arrangement. It is where the
//!   tiling guarantee lives: [`apply`] turns a tree and a rectangle into
//!   rectangles that are pairwise non-overlapping and exactly cover the input.
//! - [`Workspace`] adds what sits outside the tree — floating windows and
//!   fullscreen — so that [`Tree`] keeps its invariants unconditional.
//! - [`Layout`] owns the display arrangement and the binding from display to
//!   desktop. Displays are just rectangles in one global coordinate space, so
//!   moving focus or a window across monitors is the same geometry as moving it
//!   within one.

#![forbid(unsafe_code)]

mod command;
mod error;
mod frame;
mod geom;
mod id;
mod layout;
mod output;
mod tiling;
mod tree;
mod workspace;

pub use command::{Command, Event, dispatch};
pub use error::{LayoutError, LayoutInvariant, TreeInvariant};
pub use frame::{Frame, Placement, PlacementKind, frame};
pub use geom::{Axis, Direction, Point, Rect, Size};
pub use id::{NodeId, OutputId, WindowId, WorkspaceId};
pub use layout::{Config, Layout};
pub use output::Output;
pub use tiling::{Params, apply, apply_with, geometry, pick_direction};
pub use tree::{
    DEFAULT_WEIGHT, InsertTarget, Leaf, MIN_WEIGHT, Node, Removed, Split, Subtree, Tree,
};
pub use workspace::{Floating, Workspace};
