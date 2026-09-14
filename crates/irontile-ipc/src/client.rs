//! A blocking client.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::wire::{Decoder, WireError, write_message};
use crate::{Action, Command, Event, Query, Request, RequestPayload, Response, ResponsePayload};

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Wire(WireError),
    /// The compositor rejected the request.
    Refused(String),
    /// The compositor closed the connection.
    Closed,
    /// A well-formed reply of the wrong shape, which means the two ends
    /// disagree about the protocol.
    Unexpected(Box<ResponsePayload>),
    /// No socket could be determined from the environment.
    NoSocket,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "{e}"),
            ClientError::Wire(e) => write!(f, "{e}"),
            ClientError::Refused(m) => write!(f, "{m}"),
            ClientError::Closed => write!(f, "the compositor closed the connection"),
            ClientError::Unexpected(p) => write!(f, "unexpected reply: {p:?}"),
            ClientError::NoSocket => write!(
                f,
                "no control socket; set IRONTILE_SOCKET or WAYLAND_DISPLAY"
            ),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        ClientError::Wire(e)
    }
}

#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    decoder: Decoder,
    chunk: Vec<u8>,
    next_id: u64,
    /// Events that arrived while a reply was being waited for.
    ///
    /// A subscriber that also sends requests would otherwise lose every event
    /// that happened to be in flight, including the ones its own request
    /// caused: the compositor writes those before the reply.
    events: VecDeque<Event>,
}

impl Client {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path.as_ref())?;
        Ok(Self {
            stream,
            decoder: Decoder::new(),
            chunk: vec![0u8; 16 * 1024],
            next_id: 1,
            events: VecDeque::new(),
        })
    }

    /// Connects to the socket named by the environment.
    pub fn connect_default() -> Result<Self, ClientError> {
        let path = crate::default_socket_path().ok_or(ClientError::NoSocket)?;
        Self::connect(path)
    }

    /// Bounds how long a call waits on a compositor that has stopped
    /// responding. Off by default, which is what a subscriber wants.
    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ClientError> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    pub fn action(&mut self, action: Action) -> Result<Vec<Event>, ClientError> {
        match self.request(RequestPayload::Action(action))? {
            ResponsePayload::Ok { events } => Ok(events),
            other => Err(ClientError::Unexpected(Box::new(other))),
        }
    }

    pub fn command(&mut self, command: Command) -> Result<Vec<Event>, ClientError> {
        match self.request(RequestPayload::Command(command))? {
            ResponsePayload::Ok { events } => Ok(events),
            other => Err(ClientError::Unexpected(Box::new(other))),
        }
    }

    pub fn query(&mut self, query: Query) -> Result<ResponsePayload, ClientError> {
        self.request(RequestPayload::Query(query))
    }

    /// Asks for the whole engine state.
    pub fn layout(&mut self) -> Result<crate::Layout, ClientError> {
        match self.query(Query::Layout)? {
            ResponsePayload::Layout(layout) => Ok(*layout),
            other => Err(ClientError::Unexpected(Box::new(other))),
        }
    }

    pub fn frame(&mut self) -> Result<crate::Frame, ClientError> {
        match self.query(Query::Frame)? {
            ResponsePayload::Frame(frame) => Ok(frame),
            other => Err(ClientError::Unexpected(Box::new(other))),
        }
    }

    /// Starts the event stream. Every later [`Client::next_event`] reads from it.
    pub fn subscribe(&mut self) -> Result<(), ClientError> {
        match self.request(RequestPayload::Subscribe)? {
            ResponsePayload::Ok { .. } => Ok(()),
            other => Err(ClientError::Unexpected(Box::new(other))),
        }
    }

    /// Blocks until the next event arrives.
    pub fn next_event(&mut self) -> Result<Event, ClientError> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(event);
            }
            if let ResponsePayload::Event(event) = self.read_response()?.payload {
                return Ok(event);
            }
        }
    }

    /// Takes the events that have already arrived, without blocking.
    pub fn drain_events(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }

    /// Sends a request and waits for the reply that matches it.
    ///
    /// Events pushed by a subscription can arrive in the middle of this, so
    /// anything that is not this request's reply is skipped rather than
    /// mistaken for one.
    pub fn request(&mut self, payload: RequestPayload) -> Result<ResponsePayload, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(&mut self.stream, &Request { id, payload })?;

        loop {
            let response = self.read_response()?;
            // Events are set aside rather than dropped, so a subscriber sees
            // the ones its own request caused.
            if let ResponsePayload::Event(event) = response.payload {
                self.events.push_back(event);
                continue;
            }
            if response.id != id {
                continue;
            }
            return match response.payload {
                ResponsePayload::Error { message } => Err(ClientError::Refused(message)),
                payload => Ok(payload),
            };
        }
    }

    fn read_response(&mut self) -> Result<Response, ClientError> {
        loop {
            if let Some(message) = self.decoder.next_message::<Response>() {
                return Ok(message?);
            }
            let read = self.stream.read(&mut self.chunk)?;
            if read == 0 {
                return Err(ClientError::Closed);
            }
            self.decoder.extend(&self.chunk[..read]);
        }
    }
}
