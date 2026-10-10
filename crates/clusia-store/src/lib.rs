//! Clúsia's state on disk. Every write is atomic; unreadable files are quarantined, never deleted.

pub mod activity;
pub mod agent;
pub mod atomic;
pub mod cache;
pub mod checks;
pub mod config;
pub mod reviews;

pub use activity::{append_activity, read_activity};
pub use agent::{load_agent_state, save_agent_state};
pub use cache::{delete_review_cache, load_review_cache, save_review_cache};
pub use checks::{delete_checks, load_checks, save_checks};
pub use config::{ConfigKeyError, Loaded, get_value, load_config, save_config, set_value};
pub use reviews::{ReviewLoad, delete_review, list_reviews, load_review, save_review};

/// Seconds since the Unix epoch, for quarantine suffixes.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
