//! Typed control-socket protocol for irontile.
//!
//! The socket is a remote control for the layout engine. It carries three kinds
//! of request:
//!
//! - [`Action`], the coarse vocabulary a key binding names, which always acts
//!   on whatever is focused. The config file and `irontilectl` use the same
//!   text form, so there is one spelling of "focus left" to learn.
//! - [`Command`], the layout engine's own precise verbs, which address windows
//!   and desktops by id. This is what scripts and tests use when they need to
//!   say exactly which window.
//! - [`Query`], which reads state back. [`Query::Layout`] returns the whole
//!   engine state; it deserializes into a real [`Layout`], so a caller can
//!   inspect the tree or run [`Layout::validate`] on it without the compositor
//!   growing a reporting API of its own.

#![forbid(unsafe_code)]

mod action;
mod client;
mod wire;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use action::{Action, ParseError, Reason, VERBS, parse_action, split_argv};
pub use client::{Client, ClientError};
pub use irontile_layout::{
    Axis, Command, Direction, Event, Frame, Layout, LayoutError, Output, OutputId, Placement,
    PlacementKind, Rect, WindowId, Workspace, WorkspaceId,
};
pub use wire::{Decoder, MAX_MESSAGE, WireError, read_message, write_message};

/// A single client-to-compositor message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Echoed on the matching [`Response`], so a caller may pipeline.
    pub id: u64,
    pub payload: RequestPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestPayload {
    /// Do what a key binding would do.
    Action(Action),
    /// Apply a layout command verbatim.
    Command(Command),
    /// Read state back.
    Query(Query),
    /// Start receiving [`ResponsePayload::Event`] messages, tagged with this
    /// request's id, until the connection closes.
    Subscribe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Query {
    /// Where every window is right now.
    Frame,
    /// Connected displays and their arrangement.
    Outputs,
    /// Every desktop, whether it is on screen and what it holds.
    Workspaces,
    /// Every managed window, with what it calls itself.
    Windows,
    /// The entire layout engine state.
    Layout,
    /// Every panel and overlay on screen.
    Layers,
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
    /// The request was applied; these are the resulting events.
    Ok {
        events: Vec<Event>,
    },
    Frame(Frame),
    Outputs(Vec<Output>),
    Workspaces(Vec<WorkspaceSummary>),
    Windows(Vec<WindowInfo>),
    /// Boxed because it is much larger than every other variant.
    Layout(Box<Layout>),
    Layers(Vec<LayerInfo>),
    /// Pushed to subscribers, carrying the id of their `Subscribe` request.
    Event(Event),
    Error {
        message: String,
    },
}

/// A panel or overlay the compositor is showing.
///
/// Layer surfaces are the one thing on screen that is neither a window nor a
/// display, so nothing else here describes them -- and a bar, a notification or
/// a lock screen that fails to appear otherwise leaves nothing to look at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerInfo {
    /// What the client called it, such as `"irontile-bar"` or `"notifications"`.
    pub namespace: String,
    pub layer: LayerKind,
    pub output: OutputId,
    /// Where it sits, in the space the displays share.
    pub rect: Rect,
    /// How much of its display it reserves for itself.
    pub exclusive: i32,
    /// Whether it has asked for the keyboard.
    pub keyboard: bool,
}

/// Which stratum a panel sits in, from the bottom up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerKind {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// What a window calls itself, and where it is.
///
/// Titles and application ids exist so that something outside the compositor
/// can say what a window is. Without them a bar can list windows but not name
/// them, which is most of the point of listing them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    /// What the window calls itself, if it has said.
    pub title: Option<String>,
    /// The class of application, such as `org.mozilla.firefox`.
    pub app_id: Option<String>,
    pub workspace: WorkspaceId,
    /// The display it is on, or `None` when its desktop is off screen.
    pub output: Option<OutputId>,
    pub focused: bool,
}

/// What a desktop holds, without its whole tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub id: WorkspaceId,
    pub name: Option<String>,
    /// The display showing it, or `None` when it is off screen.
    pub output: Option<OutputId>,
    pub focused: bool,
    pub windows: Vec<WindowId>,
}

/// Where the compositor listens.
///
/// Keyed by the Wayland display name so that two compositors on one machine do
/// not collide, which is exactly what happens when a nested instance is run for
/// development from inside a session.
pub fn socket_path(wayland_display: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("irontile").join(format!("{wayland_display}.sock"))
}

/// The socket a client should connect to by default.
///
/// `IRONTILE_SOCKET` wins, so a script can target a specific instance;
/// otherwise the one belonging to `WAYLAND_DISPLAY`.
pub fn default_socket_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("IRONTILE_SOCKET") {
        return Some(PathBuf::from(explicit));
    }
    let display = std::env::var("WAYLAND_DISPLAY").ok()?;
    Some(socket_path(&display))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_the_wire_format() {
        let requests = [
            Request {
                id: 1,
                payload: RequestPayload::Action(Action::Close),
            },
            Request {
                id: 2,
                payload: RequestPayload::Command(Command::FocusWindow {
                    window: WindowId(3),
                }),
            },
            Request {
                id: 3,
                payload: RequestPayload::Query(Query::Layout),
            },
            Request {
                id: 4,
                payload: RequestPayload::Subscribe,
            },
        ];
        for request in requests {
            let mut buf = Vec::new();
            write_message(&mut buf, &request).unwrap();
            let decoded: Request = read_message(&mut buf.as_slice()).unwrap();
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn a_whole_layout_survives_the_round_trip() {
        let mut layout = Layout::new(Default::default());
        layout.reconfigure_outputs(vec![Output::new(
            OutputId(1),
            "DP-1",
            Rect::new(0, 0, 1920, 1080),
        )]);
        let response = Response {
            id: 7,
            payload: ResponsePayload::Layout(Box::new(layout.clone())),
        };

        let mut buf = Vec::new();
        write_message(&mut buf, &response).unwrap();
        let decoded: Response = read_message(&mut buf.as_slice()).unwrap();

        match decoded.payload {
            ResponsePayload::Layout(restored) => {
                // The point of shipping the whole engine state: the receiver can
                // check it for itself.
                restored.validate().unwrap();
                assert_eq!(*restored, layout);
            }
            other => panic!("expected a layout, got {other:?}"),
        }
    }

    #[test]
    fn the_socket_path_is_namespaced_by_display() {
        // Two compositors on one machine must not collide, which is exactly
        // what a nested development instance is.
        assert_ne!(socket_path("wayland-1"), socket_path("wayland-2"));
        assert!(socket_path("wayland-1").ends_with("irontile/wayland-1.sock"));
    }
}
