//! The control socket.
//!
//! A listening unix socket wired into the event loop. Each connection is read
//! without blocking, so a client that sends half a message, or stops reading
//! its replies, cannot stall the compositor.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use irontile_ipc::{
    Decoder, Event, Query, Request, RequestPayload, Response, ResponsePayload, socket_path,
    write_message,
};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::action;
use crate::state::Irontile;

/// One connected control client.
#[derive(Debug)]
struct Peer {
    stream: UnixStream,
    decoder: Decoder,
    /// Set to the id of a `Subscribe` request once one arrives, so pushed
    /// events carry the id the subscriber is expecting.
    subscription: Option<u64>,
    /// Bytes written for this peer that its socket would not take yet.
    ///
    /// The socket is not blocking, so a burst of events can fill it while a
    /// subscriber is busy drawing the last one. Writing anyway gets a partial
    /// frame into the stream and desynchronizes the protocol for good, and
    /// giving up on the peer means a bar that fell one repaint behind
    /// disappears. Holding the remainder until the socket drains is the only
    /// answer that is both correct and survivable.
    outbox: Vec<u8>,
}

/// How much undelivered event traffic a subscriber may accumulate before it is
/// treated as gone rather than as busy.
///
/// A client that has stopped reading entirely would otherwise grow this without
/// limit, and the compositor is the wrong place to store an unbounded amount of
/// anything on a client's behalf.
const MAX_OUTBOX: usize = 1 << 20;

/// Connections, shared between the listener source and the per-peer sources.
#[derive(Debug, Default)]
pub struct Peers {
    peers: HashMap<u64, Peer>,
    next: u64,
}

impl Peers {
    /// Pushes an event to every subscriber, dropping any that has gone away.
    pub fn broadcast(&mut self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        self.peers.retain(|_, peer| {
            let Some(id) = peer.subscription else {
                return true;
            };
            for event in events {
                let response = Response {
                    id,
                    payload: ResponsePayload::Event(*event),
                };
                // Into the outbox first, so a frame is either queued whole or
                // not at all; the socket never sees half of one.
                if write_message(&mut peer.outbox, &response).is_err() {
                    return false;
                }
            }
            peer.flush()
        });
    }
}

impl Peer {
    /// Pushes as much of the outbox as the socket will take.
    ///
    /// Returns whether the peer is still worth keeping.
    fn flush(&mut self) -> bool {
        while !self.outbox.is_empty() {
            match self.stream.write(&self.outbox) {
                Ok(0) => return false,
                Ok(n) => {
                    self.outbox.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                // Busy, not broken. The rest goes out on the next attempt.
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        self.outbox.len() <= MAX_OUTBOX
    }
}

/// The listening socket, removed from the filesystem when dropped.
#[derive(Debug)]
pub struct Listener {
    path: PathBuf,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Listener {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Binds the control socket and wires it into the event loop.
pub fn listen(
    handle: &LoopHandle<'static, Irontile>,
    wayland_display: &str,
) -> std::io::Result<Listener> {
    let path = socket_path(wayland_display);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict(parent)?;
    }
    // A socket left behind by a compositor that did not exit cleanly would
    // otherwise make every later start fail with "address in use".
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::warn!(path = %path.display(), "removed a stale control socket"),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    restrict(&path)?;

    let loop_handle = handle.clone();
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state: &mut Irontile| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => accept(&loop_handle, state, stream),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(err) => {
                            tracing::warn!(%err, "control socket accept failed");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| std::io::Error::other(format!("{e}")))?;

    Ok(Listener { path })
}

/// Owner-only, because anything that can talk to this socket can drive the
/// session and spawn processes as the user.
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn accept(handle: &LoopHandle<'static, Irontile>, state: &mut Irontile, stream: UnixStream) {
    if let Err(err) = stream.set_nonblocking(true) {
        tracing::warn!(%err, "could not configure a control client");
        return;
    }
    let Ok(readable) = stream.try_clone() else {
        tracing::warn!("could not clone a control client socket");
        return;
    };

    let id = state.peers.next;
    state.peers.next += 1;
    state.peers.peers.insert(
        id,
        Peer {
            stream,
            decoder: Decoder::new(),
            subscription: None,
            outbox: Vec::new(),
        },
    );

    let inserted = handle.insert_source(
        Generic::new(readable, Interest::READ, Mode::Level),
        move |_, source, state: &mut Irontile| {
            match pump(state, id, source) {
                Peered::Continue => Ok(PostAction::Continue),
                // Removing the source is what closes our end; the peer entry
                // goes with it so no later broadcast tries to write to it.
                Peered::Disconnected => {
                    state.peers.peers.remove(&id);
                    Ok(PostAction::Remove)
                }
            }
        },
    );
    if let Err(err) = inserted {
        tracing::warn!(%err, "could not register a control client");
        state.peers.peers.remove(&id);
    } else {
        tracing::debug!(peer = id, "control client connected");
    }
}

enum Peered {
    Continue,
    Disconnected,
}

fn pump(state: &mut Irontile, id: u64, source: &UnixStream) -> Peered {
    // A peer that is readable has been running, so it is worth another try at
    // whatever would not fit last time.
    if let Some(peer) = state.peers.peers.get_mut(&id)
        && !peer.outbox.is_empty()
        && !peer.flush()
    {
        return Peered::Disconnected;
    }

    let mut chunk = [0u8; 8192];
    let mut reader = source;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Peered::Disconnected,
            Ok(n) => {
                let Some(peer) = state.peers.peers.get_mut(&id) else {
                    return Peered::Disconnected;
                };
                peer.decoder.extend(&chunk[..n]);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::debug!(peer = id, %err, "control client read failed");
                return Peered::Disconnected;
            }
        }
    }

    // Requests are taken one at a time because handling one needs `&mut state`,
    // which the decoder borrow would otherwise hold across.
    loop {
        let request = {
            let Some(peer) = state.peers.peers.get_mut(&id) else {
                return Peered::Disconnected;
            };
            match peer.decoder.next_message::<Request>() {
                Some(Ok(request)) => request,
                // Framing is lost, not just this message; the peer cannot be
                // resynchronised, so drop it.
                Some(Err(err)) => {
                    tracing::warn!(peer = id, %err, "malformed control message");
                    return Peered::Disconnected;
                }
                None => return Peered::Continue,
            }
        };

        let response = Response {
            id: request.id,
            payload: handle_request(state, id, &request),
        };
        let Some(peer) = state.peers.peers.get_mut(&id) else {
            return Peered::Disconnected;
        };
        if write_message(&mut peer.stream, &response).is_err() {
            return Peered::Disconnected;
        }
        let _ = peer.stream.flush();
    }
}

fn handle_request(state: &mut Irontile, peer: u64, request: &Request) -> ResponsePayload {
    match &request.payload {
        RequestPayload::Action(action) => {
            // Broadcasting happens inside the state, so that a key binding and
            // a socket request reach subscribers by exactly one path.
            let events = action::perform(state, action);
            ResponsePayload::Ok { events }
        }
        RequestPayload::Command(command) => match state.try_apply(command.clone()) {
            Ok(events) => ResponsePayload::Ok { events },
            Err(err) => ResponsePayload::Error {
                message: err.to_string(),
            },
        },
        RequestPayload::Query(query) => query_reply(state, *query),
        RequestPayload::Subscribe => {
            if let Some(peer) = state.peers.peers.get_mut(&peer) {
                peer.subscription = Some(request.id);
            }
            ResponsePayload::Ok { events: Vec::new() }
        }
        // The protocol is `#[non_exhaustive]`; a request this compositor does
        // not understand is reported rather than silently dropped.
        other => ResponsePayload::Error {
            message: format!("unsupported request: {other:?}"),
        },
    }
}

fn query_reply(state: &mut Irontile, query: Query) -> ResponsePayload {
    // A query must not report a layout the clients have not been configured
    // for, so any pending change is flushed first.
    if state.dirty {
        state.reflow();
    }
    match query {
        Query::Frame => ResponsePayload::Frame(state.placements.clone()),
        Query::Outputs => ResponsePayload::Outputs(state.layout.outputs().to_vec()),
        Query::Workspaces => ResponsePayload::Workspaces(state.workspace_summaries()),
        Query::Windows => ResponsePayload::Windows(state.window_infos()),
        Query::Layout => ResponsePayload::Layout(Box::new(state.layout.clone())),
        Query::Layers => ResponsePayload::Layers(state.layer_infos()),
        // The protocol is `#[non_exhaustive]`; an unknown query is an error
        // rather than a panic.
        other => ResponsePayload::Error {
            message: format!("unsupported query: {other:?}"),
        },
    }
}
