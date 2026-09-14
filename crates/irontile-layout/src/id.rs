//! Opaque identifiers.
//!
//! [`WindowId`] and [`OutputId`] are minted by the compositor, which knows how
//! they map onto toplevels and `wl_output`s. [`WorkspaceId`] and [`NodeId`] are
//! minted here.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Identifies a managed toplevel. Minted by the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub u64);

/// Identifies a physical display. Minted by the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputId(pub u64);

/// Identifies a desktop. Allocated monotonically and never reused, so a stale
/// reference from the control socket is always a clean error rather than a
/// silent hit on an unrelated desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub u64);

/// A handle into a [`Tree`]'s node arena.
///
/// The generation counter makes reuse of a freed slot detectable: a handle held
/// across the removal of its node resolves to `None` rather than to whatever
/// node landed in the slot afterwards.
///
/// [`Tree`]: crate::Tree
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl NodeId {
    pub(crate) const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    pub const fn index(self) -> u32 {
        self.index
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({}v{})", self.index, self.generation)
    }
}

macro_rules! impl_display {
    ($($ty:ty => $prefix:literal),* $(,)?) => {
        $(
            impl fmt::Display for $ty {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, concat!($prefix, "{}"), self.0)
                }
            }
        )*
    };
}

impl_display! {
    WindowId => "window#",
    OutputId => "output#",
    WorkspaceId => "workspace#",
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node#{}v{}", self.index, self.generation)
    }
}
