//! Backends.
//!
//! Each backend owns a renderer, a set of displays, and an input source, and
//! drives the same [`Irontile`] state. The nested backend runs irontile as a
//! window inside another compositor, which is the development loop; a
//! session backend on DRM will slot in alongside it.
//!
//! [`Irontile`]: crate::state::Irontile

pub mod nested;
