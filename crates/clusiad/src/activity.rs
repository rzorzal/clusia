//! The activity heatmap and stats (spec §7, Home and tray).

use clusia_protocol::{ErrorCode, Outcome, ProtocolError, Reply};
use clusia_store::read_activity;

use crate::state::Shared;
use crate::sync::now_unix;

/// Seconds east of UTC for the machine's local time zone at `now`.
pub(crate) fn local_offset_secs(now: i64) -> i64 {
    let t = now as libc::time_t;
    // SAFETY: `tm` is a plain C struct; zeroed is a valid initial value, and `localtime_r`
    // only writes into the buffer we pass (it is the re-entrant variant).
    #[allow(unsafe_code)]
    let (result_is_null, offset) = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let result = libc::localtime_r(&t, &mut tm);
        (result.is_null(), tm.tm_gmtoff as i64)
    };
    if result_is_null { 0 } else { offset }
}

pub(crate) async fn summary(shared: &Shared) -> Outcome {
    match read_activity(&shared.paths) {
        Ok((activities, skipped)) => {
            if skipped > 0 {
                tracing::debug!(skipped, "ignored unreadable activity lines");
            }
            let now = now_unix();
            Outcome::Ok(Reply::Activity(clusia_analytics::summary(
                &activities,
                now,
                local_offset_secs(now),
            )))
        }
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("cannot read the activity log: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_offset_is_a_sane_timezone() {
        let offset = super::local_offset_secs(1_700_000_000);
        assert!((-14 * 3600..=14 * 3600).contains(&offset), "{offset}");
        assert_eq!(offset % 900, 0, "time zones are whole quarter hours");
    }
}
