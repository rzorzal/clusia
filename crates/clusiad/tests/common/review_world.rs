#![allow(dead_code)]

use clusia_protocol::{Client, Command, Reply, ReviewView};
use wiremock::MockServer;

use super::git_fixture::{advance_pr, origin_with_pr};
use super::github_mock::{PrMock, mount_pr};
use super::{TestDaemon, test_options};
pub struct World {
    pub tmp: tempfile::TempDir,
    pub server: MockServer,
    pub daemon: TestDaemon,
    pub head: String,
    pub base: String,
    pub origin: String,
}

/// acme/widgets#7 with `feature.txt` = "one\ntwo\nthree\n", served by a mock GitHub and a local origin.
pub async fn world() -> World {
    world_with(false).await
}

/// Like `world`, with the daemon's background sync loop running (it also checks saved reviews).
pub async fn world_syncing() -> World {
    world_with(true).await
}

async fn world_with(background_sync: bool) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let origin = origin_with_pr(tmp.path(), 7);
    let head = advance_pr(tmp.path(), 7, "feature.txt", "one\ntwo\nthree\n");
    let base = super::git_fixture::sh(
        tmp.path(),
        &[
            "--git-dir",
            origin.path.to_str().unwrap(),
            "rev-parse",
            "main",
        ],
    );
    let server = MockServer::start().await;
    mount_pr(
        &server,
        &PrMock::new(&head, &base, origin.path.to_str().unwrap()),
    )
    .await;
    let mut options = test_options();
    options.github_api = Some(server.uri());
    options.github_token = Some("tok".into());
    options.background_sync = background_sync;
    let daemon = TestDaemon::start_with(tempfile::tempdir().unwrap(), options).await;
    let empty_roots = tmp.path().join("roots");
    std::fs::create_dir_all(&empty_roots).unwrap();
    daemon
        .client()
        .await
        .request(Command::SetConfigValue {
            key: "repositories.roots".into(),
            value: format!("[\"{}\"]", empty_roots.display()),
        })
        .await
        .unwrap();
    World {
        tmp,
        server,
        daemon,
        head,
        base,
        origin: origin.path.display().to_string(),
    }
}

pub fn pr7() -> clusia_core::PrRef {
    "acme/widgets#7".parse().unwrap()
}

pub fn view_of(reply: Reply) -> ReviewView {
    match reply {
        Reply::Review(v) => *v,
        other => panic!("expected Review, got {other:?}"),
    }
}

pub async fn open(c: &mut Client) -> ReviewView {
    view_of(c.request(Command::OpenReview { pr: pr7() }).await.unwrap())
}
