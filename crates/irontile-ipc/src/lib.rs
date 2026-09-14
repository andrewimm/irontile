//! Typed control-socket protocol for irontile.
//!
//! The wire protocol is deliberately a thin envelope around
//! [`irontile_layout::Command`] and [`irontile_layout::Event`]: the control
//! socket is a remote control for the layout engine, so anything expressible at
//! a keybinding is expressible here without a parallel vocabulary.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

pub use irontile_layout::{Command, Event, Frame, LayoutError};

/// A single client-to-compositor message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Echoed back on the matching [`Response`] so clients can pipeline.
    pub id: u64,
    pub payload: RequestPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestPayload {
    /// Apply a layout command.
    Command(Command),
    /// Ask for the current placement of every window.
    Frame,
    /// Subscribe to the event stream; further [`Response::Event`] messages
    /// arrive unsolicited with `id` set to the subscribing request's id.
    Subscribe,
}

/// A single compositor-to-client message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub payload: ResponsePayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResponsePayload {
    Ok { events: Vec<Event> },
    Frame(Frame),
    Event(Event),
    Error { message: String },
}

// TODO: socket transport (length-prefixed framing over a unix stream), the
// client handle, and the `irontilectl` binary.
