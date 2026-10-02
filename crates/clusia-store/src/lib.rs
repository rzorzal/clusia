//! Clúsia's state on disk. Every write is atomic; unreadable files are quarantined, never deleted.

pub mod atomic;

/// Seconds since the Unix epoch, for quarantine suffixes.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
