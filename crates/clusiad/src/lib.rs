//! clusiad: the only stateful Clúsia process. Clients talk to it over a Unix socket.

mod activity;
mod agent;
mod agent_log;
mod agent_stream;
mod bridge;
mod checks;
mod connection;
mod first_run;
mod giphy;
mod handlers;
mod holds;
mod inbox;
mod lock;
mod login;
mod media;
mod news;
mod notifications;
mod options;
mod permissions;
mod publish;
mod relocate;
mod retention;
mod reviews;
mod server;
mod sessions;
mod spawner;
mod state;
mod sync;
mod tray;
mod turns;
mod worktrees;

pub use bridge::run_permission_bridge;
pub use lock::DaemonLock;
pub use options::DaemonOptions;
pub use server::{Daemon, ShutdownHandle, StartError};
pub use spawner::{ProcessSpawner, RecordingSpawner, Spawner};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
