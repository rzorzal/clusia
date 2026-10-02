//! The protocol between clusiad and its clients: JSON objects, one per line, over a Unix socket.

pub mod client;
pub mod codec;
pub mod message;

pub use client::{Client, ClientError};
pub use codec::{CodecError, MAX_LINE_BYTES, MessageReader, write_message};
pub use message::*;

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 1;
