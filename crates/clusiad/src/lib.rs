//! clusiad: the only stateful Clúsia process. Clients talk to it over a Unix socket.

mod connection;
mod handlers;
mod server;
mod state;

pub use server::{Daemon, ShutdownHandle, StartError};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
