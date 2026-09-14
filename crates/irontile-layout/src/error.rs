//! Error and invariant-violation types.

use crate::geom::Direction;
use crate::id::{NodeId, OutputId, WindowId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Every way an operation can be rejected.
///
/// These are all "the caller asked for something that does not make sense
/// against the current state" conditions. They carry enough detail to be
/// reported verbatim over the control socket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LayoutError {
    UnknownWindow(WindowId),
    UnknownWorkspace(WorkspaceId),
    UnknownOutput(OutputId),
    /// The handle refers to a node that has since been removed.
    StaleNode(NodeId),
    NotAContainer(NodeId),
    NotALeaf(NodeId),
    /// The requested directional operation would leave the tree entirely.
    /// Callers that own an output arrangement catch this and retry against the
    /// neighbouring display.
    AtEdge(Direction),
    /// An operation needed a display and none are connected.
    NoOutputs,
    WindowAlreadyManaged(WindowId),
    /// A node index was outside its container's child list.
    IndexOutOfRange,
    /// `swap` was given two nodes where one contains the other.
    OverlappingSubtrees(NodeId, NodeId),
    /// A desktop that still holds windows cannot be destroyed.
    WorkspaceNotEmpty(WorkspaceId),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::UnknownWindow(w) => write!(f, "unknown {w}"),
            LayoutError::UnknownWorkspace(w) => write!(f, "unknown {w}"),
            LayoutError::UnknownOutput(o) => write!(f, "unknown {o}"),
            LayoutError::StaleNode(n) => write!(f, "stale handle {n}"),
            LayoutError::NotAContainer(n) => write!(f, "{n} is not a container"),
            LayoutError::NotALeaf(n) => write!(f, "{n} is not a leaf"),
            LayoutError::AtEdge(d) => write!(f, "already at the {d:?} edge of the tree"),
            LayoutError::NoOutputs => write!(f, "no outputs are connected"),
            LayoutError::WindowAlreadyManaged(w) => write!(f, "{w} is already managed"),
            LayoutError::IndexOutOfRange => write!(f, "child index out of range"),
            LayoutError::OverlappingSubtrees(a, b) => {
                write!(f, "cannot swap {a} with {b}: one contains the other")
            }
            LayoutError::WorkspaceNotEmpty(w) => write!(f, "{w} still holds windows"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// A structural defect in a [`Tree`].
///
/// Produced only by [`Tree::validate`], which exists so that property tests can
/// assert the tree is well formed after an arbitrary sequence of operations,
/// and so that a deserialized tree from an untrusted source can be checked
/// before use.
///
/// [`Tree`]: crate::Tree
/// [`Tree::validate`]: crate::Tree::validate
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TreeInvariant {
    /// A child handle does not resolve to a live node.
    DanglingChild { parent: NodeId, child: NodeId },
    /// `children` and `weights` have different lengths.
    WeightArity {
        container: NodeId,
        children: usize,
        weights: usize,
    },
    /// A container holds fewer than two children. Single-child containers are
    /// collapsed eagerly so that the tree shape stays canonical.
    UndersizedContainer { container: NodeId, children: usize },
    /// A weight of zero would let a child vanish irrecoverably.
    ZeroWeight { container: NodeId, index: usize },
    /// A node's recorded parent disagrees with the parent that lists it.
    ParentMismatch {
        node: NodeId,
        recorded: Option<NodeId>,
        actual: Option<NodeId>,
    },
    /// A live node is not reachable from the root.
    Orphan(NodeId),
    /// A node is listed as a child by more than one container.
    MultipleParents(NodeId),
    /// The same window appears in two leaves.
    DuplicateWindow(WindowId),
    /// The window index disagrees with the leaves actually present.
    WindowIndexMismatch(WindowId),
    /// The root handle does not resolve, or a non-empty arena has no root.
    BadRoot(Option<NodeId>),
    /// Focus points at a node that is not live.
    BadFocus(NodeId),
}

impl fmt::Display for TreeInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for TreeInvariant {}

/// A structural defect in a [`Layout`].
///
/// [`Layout`]: crate::Layout
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LayoutInvariant {
    Tree(WorkspaceId, TreeInvariant),
    /// A workspace is displayed on more than one output at once.
    WorkspaceShownTwice(WorkspaceId),
    /// An output is connected but is displaying nothing.
    OutputWithoutWorkspace(OutputId),
    /// A displayed workspace does not exist.
    UnknownShownWorkspace(OutputId, WorkspaceId),
    /// Two connected outputs share an id.
    DuplicateOutput(OutputId),
    /// The window index disagrees with the workspace contents.
    WindowIndexMismatch(WindowId),
    /// A window is both tiled and floating, or is in two workspaces.
    DuplicateWindow(WindowId),
    /// A workspace is fullscreening a window it does not contain.
    BadFullscreen(WorkspaceId, WindowId),
    /// A workspace has floating focus on a window it does not float.
    BadFloatingFocus(WorkspaceId, WindowId),
    /// Focus points at a display that is not connected.
    BadFocusedOutput(OutputId),
    /// No outputs are connected but some workspace is still displayed.
    ShownWithoutOutputs,
}

impl fmt::Display for LayoutInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for LayoutInvariant {}
