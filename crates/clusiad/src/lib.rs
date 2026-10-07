//! clusiad: the only stateful Clúsia process. Clients talk to it over a Unix socket.

mod activity;
mod connection;
mod first_run;
mod giphy;
mod handlers;
mod lock;
mod media;
mod news;
mod options;
mod publish;
mod relocate;
mod retention;
mod reviews;
mod server;
mod spawner;
mod state;
mod sync;
mod tray;
mod worktrees;

pub use lock::DaemonLock;
pub use options::DaemonOptions;
pub use server::{Daemon, ShutdownHandle, StartError};
pub use spawner::{ProcessSpawner, RecordingSpawner, Spawner};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
