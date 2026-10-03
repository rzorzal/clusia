mod common;

use clusia_core::{Activity, ActivityKind};
use clusia_protocol::{Command, Reply};
use common::TestDaemon;

#[tokio::test]
async fn activity_summary_from_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let paths = clusia_core::Paths::new(dir.path());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    for (ts, kind) in [
        (now - 3600, ActivityKind::ReviewOpened),
        (now - 600, ActivityKind::ReviewPublished),
    ] {
        let a = Activity {
            ts,
            kind,
            pr: "acme/widgets#7".parse().unwrap(),
            client: "t".into(),
            url: None,
            note: None,
        };
        clusia_store::append_activity(&paths, &a).unwrap();
    }
    let d = TestDaemon::start_in(dir).await;
    match d
        .client()
        .await
        .request(Command::GetActivity)
        .await
        .unwrap()
    {
        Reply::Activity(s) => {
            assert_eq!((s.published_total, s.published_this_week), (1, 1));
            assert_eq!(s.avg_review_secs, Some(3000));
            assert_eq!(s.heatmap.len(), 26 * 7);
            assert_eq!(s.heatmap.iter().map(|d| d.count).sum::<u32>(), 1);
        }
        other => panic!("{other:?}"),
    }
    d.stop().await;
}
