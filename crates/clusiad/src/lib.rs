//! clusiad: the only stateful Clúsia process. Clients talk to it over a Unix socket.

mod connection;
mod handlers;
mod news;
mod options;
mod publish;
mod relocate;
mod retention;
mod reviews;
mod server;
mod state;
mod sync;
mod worktrees;

pub use options::DaemonOptions;
pub use server::{Daemon, ShutdownHandle, StartError};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
