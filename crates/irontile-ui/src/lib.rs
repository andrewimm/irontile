//! Bar and launcher surfaces for irontile.
//!
//! These are ordinary layer-shell clients. They hold no privileged access to
//! the compositor: everything they know arrives over the control socket, the
//! same one the tests drive. That is deliberate — it means anyone can replace
//! them with their own, and it means the bar exercises the public interface
//! rather than a private side channel.

#![forbid(unsafe_code)]

pub mod audio;
pub mod bar;
pub mod config;
pub mod draw;
pub mod icon;
pub mod module;
pub mod theme;
pub mod tray;
pub mod wayland;
pub mod world;

pub use config::Config;
