//! The activity heatmap and stats (spec §7, Home and tray).

use std::io;

use clusia_core::{Activity, Paths};
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

/// The activity log, read on a blocking thread: it is a whole-file scan that grows with use,
/// and the async threads also serve every other request.
pub(crate) async fn read_off_thread(paths: &Paths) -> io::Result<(Vec<Activity>, usize)> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || read_activity(&paths))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
}

pub(crate) async fn summary(shared: &Shared) -> Outcome {
    match read_off_thread(&shared.paths).await {
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
    use clusia_core::{Activity, ActivityKind, Paths};

    #[tokio::test(flavor = "current_thread")]
    async fn the_log_reads_the_same_off_the_async_threads() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.root()).unwrap();
        for ts in [10, 20, 30] {
            clusia_store::append_activity(
                &paths,
                &Activity {
                    ts,
                    kind: ActivityKind::ReviewOpened,
                    pr: "acme/widgets#7".parse().unwrap(),
                    client: "test".into(),
                    url: None,
                    note: None,
                },
            )
            .unwrap();
        }
        let (items, skipped) = super::read_off_thread(&paths).await.unwrap();
        assert_eq!((items.len(), skipped), (3, 0));
        assert_eq!(items, clusia_store::read_activity(&paths).unwrap().0);
    }

    #[test]
    fn local_offset_is_a_sane_timezone() {
        let offset = super::local_offset_secs(1_700_000_000);
        assert!((-14 * 3600..=14 * 3600).contains(&offset), "{offset}");
        assert_eq!(offset % 900, 0, "time zones are whole quarter hours");
    }
}
