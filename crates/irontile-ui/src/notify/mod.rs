//! Notifications: the popups, and the panel they collect in.
//!
//! Two surfaces from one program. A popup is a card that appears at the corner
//! of a display, says one thing, and leaves; the centre is a panel that holds
//! everything said while you were away, with the session's buttons underneath.
//!
//! Both are drawn the way the lock screen and the polkit dialog are drawn --
//! the same palette, the same hairline rules, the same warm gradient the
//! compositor puts around a focused window -- because a desktop whose parts
//! were each designed separately looks like a desktop assembled from parts.

pub mod model;
pub mod paint;
pub mod service;
pub mod ui;
