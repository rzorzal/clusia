//! The protocol between clusiad and its clients: JSON objects, one per line, over a Unix socket.

pub mod message;

pub use message::*;

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 1;
