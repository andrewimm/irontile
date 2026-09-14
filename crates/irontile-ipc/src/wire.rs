//! Message framing.
//!
//! Each message is a little-endian `u32` byte count followed by that many bytes
//! of JSON. JSON rather than a compact encoding because the protocol is meant
//! to be usable from a shell one-liner and readable in a packet dump, and the
//! traffic is a handful of messages per keypress.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Refuse to buffer more than this for a single message. A serialized
/// [`crate::Query::Layout`] response is the largest thing on the wire and is
/// far below this even with hundreds of windows.
pub const MAX_MESSAGE: usize = 8 * 1024 * 1024;

const HEADER: usize = 4;

#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    /// The declared length exceeds [`MAX_MESSAGE`].
    Oversized(usize),
    Malformed(serde_json::Error),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "{e}"),
            WireError::Oversized(n) => write!(f, "message of {n} bytes exceeds the limit"),
            WireError::Malformed(e) => write!(f, "malformed message: {e}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        WireError::Io(e)
    }
}

impl From<serde_json::Error> for WireError {
    fn from(e: serde_json::Error) -> Self {
        WireError::Malformed(e)
    }
}

/// Writes one framed message.
pub fn write_message<W: Write, T: Serialize>(w: &mut W, message: &T) -> Result<(), WireError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_MESSAGE {
        return Err(WireError::Oversized(body.len()));
    }
    let len = body.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Reads one framed message, blocking until it is complete.
pub fn read_message<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, WireError> {
    let mut header = [0u8; HEADER];
    r.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_MESSAGE {
        return Err(WireError::Oversized(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

/// Accumulates bytes and yields whole messages.
///
/// The compositor reads its socket without blocking, so a read can stop in the
/// middle of a message; this holds the remainder until the rest arrives.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Takes the next complete message, if one has arrived.
    ///
    /// Returns `None` while the buffer holds only part of a message. An error
    /// means the stream is no longer trustworthy and the peer should be
    /// dropped: the framing is lost, not just this one message.
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Option<Result<T, WireError>> {
        if self.buf.len() < HEADER {
            return None;
        }
        let len = u32::from_le_bytes(self.buf[..HEADER].try_into().ok()?) as usize;
        if len > MAX_MESSAGE {
            return Some(Err(WireError::Oversized(len)));
        }
        if self.buf.len() < HEADER + len {
            return None;
        }
        let body: Vec<u8> = self.buf.drain(..HEADER + len).skip(HEADER).collect();
        Some(serde_json::from_slice(&body).map_err(WireError::Malformed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_round_trips() {
        let mut buf = Vec::new();
        write_message(&mut buf, &"hello").unwrap();
        let decoded: String = read_message(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded, "hello");
    }

    #[test]
    fn the_decoder_waits_for_the_whole_message() {
        let mut buf = Vec::new();
        write_message(&mut buf, &vec![1u32, 2, 3]).unwrap();

        let mut decoder = Decoder::new();
        // Feed it one byte at a time; nothing should come out until the last.
        for (i, byte) in buf.iter().enumerate() {
            decoder.extend(&[*byte]);
            let got: Option<Result<Vec<u32>, _>> = decoder.next_message();
            if i + 1 < buf.len() {
                assert!(got.is_none(), "yielded a message after {} bytes", i + 1);
            } else {
                assert_eq!(got.unwrap().unwrap(), vec![1, 2, 3]);
            }
        }
        assert!(decoder.is_empty());
    }

    #[test]
    fn the_decoder_handles_several_messages_in_one_read() {
        let mut buf = Vec::new();
        write_message(&mut buf, &"one").unwrap();
        write_message(&mut buf, &"two").unwrap();

        let mut decoder = Decoder::new();
        decoder.extend(&buf);
        let first: String = decoder.next_message().unwrap().unwrap();
        let second: String = decoder.next_message().unwrap().unwrap();
        assert_eq!((first.as_str(), second.as_str()), ("one", "two"));
        assert!(decoder.next_message::<String>().is_none());
    }

    #[test]
    fn an_absurd_length_is_refused_before_allocating() {
        let mut decoder = Decoder::new();
        decoder.extend(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decoder.next_message::<String>(),
            Some(Err(WireError::Oversized(_)))
        ));
    }
}
