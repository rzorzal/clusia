//! State shared by every connection.

use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use clusia_core::{Config, Paths};
use clusia_protocol::Event;
use tokio::sync::{RwLock, broadcast, watch};

pub(crate) struct Shared {
    pub paths: Paths,
    pub config: RwLock<Config>,
    pub started: Instant,
    pub clients: AtomicUsize,
    pub events: broadcast::Sender<(String, Event)>,
    pub shutdown: watch::Sender<bool>,
}

impl Shared {
    pub fn new(paths: Paths, config: Config) -> Self {
        let (events, _) = broadcast::channel(256);
        let (shutdown, _) = watch::channel(false);
        Self {
            paths,
            config: RwLock::new(config),
            started: Instant::now(),
            clients: AtomicUsize::new(0),
            events,
            shutdown,
        }
    }

    pub fn trigger_shutdown(&self) {
        self.shutdown.send_replace(true);
    }
}
