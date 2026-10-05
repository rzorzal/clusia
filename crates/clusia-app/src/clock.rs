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
}
