//! The wall clock, replaceable in tests (ages like "5m" depend on it).

use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;

/// `Clock(Some(t))` freezes time at `t` (Unix seconds); `Clock(None)` is the system clock.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Clock(pub Option<i64>);

impl Clock {
    pub fn now(&self) -> i64 {
        self.0.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
    }

    /// `now` in Unix milliseconds (a frozen clock reads whole seconds).
    pub fn now_ms(&self) -> i64 {
        self.0.map_or_else(
            || {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            },
            |t| t * 1000,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_clock_reads_milliseconds_too() {
        let clock = Clock(Some(1_790_000_000));
        assert_eq!(clock.now(), 1_790_000_000);
        assert_eq!(clock.now_ms(), 1_790_000_000_000);
    }

    #[test]
    fn the_system_clock_reads_milliseconds() {
        let ms = Clock(None).now_ms();
        let s = Clock(None).now();
        assert!((ms / 1000 - s).abs() <= 1);
    }
}
