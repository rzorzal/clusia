mod common;

use clusia_core::{DraftKind, Origin};
use clusia_protocol::{CallsIndexState, Client, Command, Event, Reply};
use common::agent::{until, watching};
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{World, open, pr7, world};

const LIB_RS: &str =
    "pub fn refresh() -> u32 {\n    helper()\n}\n\nfn helper() -> u32 {\n    1\n}\n";

/// The world's pull request with `lib.rs` added on top, opened.
async fn code_world() -> (World, Client, Client) {
    let w = world().await;
    let head = advance_pr(w.tmp.path(), 7, "lib.rs", LIB_RS);
    w.server.reset().await;
    mount_pr(&w.server, &PrMock::new(&head, &w.base, &w.origin)).await;
    let watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    (w, c, watcher)
}

fn ready(e: &Event) -> bool {
    matches!(
        e,
        Event::CallsIndexChanged {
            state: CallsIndexState::Ready { .. },
            ..
        }
    )
}

async fn calls(c: &mut Client) -> Reply {
    c.request(Command::GetCalls {
        pr: pr7(),
        center: None,
        depth: Some(3),
        expand: Vec::new(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn the_graph_of_a_review_is_built_on_the_first_ask_and_drawn_from_memory() {
    let (w, mut c, mut watcher) = code_world().await;
    let Reply::Calls { graph, index, .. } = calls(&mut c).await else {
        panic!("calls");
    };
    if !matches!(index, CallsIndexState::Ready { .. }) {
        assert!(graph.is_none());
        until(&mut watcher, ready).await;
    }
    let Reply::Calls { graph, index, .. } = calls(&mut c).await else {
        panic!("calls");
    };
    assert!(matches!(index, CallsIndexState::Ready { cut: false, .. }));
    let graph = graph.expect("a graph");
    assert_eq!(graph.center, "lib.rs#refresh#1");
    let mut levels: Vec<(String, i8)> = graph
        .nodes
        .iter()
        .map(|n| (n.key.clone(), n.level))
        .collect();
    levels.sort();
    assert_eq!(
        levels,
        [
            ("lib.rs#helper#5".to_string(), 1),
            ("lib.rs#refresh#1".to_string(), 0)
        ]
    );
    let Reply::DraftItem(item) = c
        .request(Command::AddCallsComment {
            pr: pr7(),
            function: "lib.rs#helper#5".into(),
            body: "Why one?".into(),
        })
        .await
        .unwrap()
    else {
        panic!("a draft item");
    };
    // GitHub's files do not list `lib.rs`, so even a new function gets a general comment.
    assert_eq!(
        (item.kind, item.origin, item.body.as_str()),
        (DraftKind::General, Origin::Human, "lib.rs:5-7: Why one?")
    );
    let cache = w.daemon.paths.calls_for(&pr7());
    assert!(cache.is_dir());
    c.request(Command::DiscardReview { pr: pr7() })
        .await
        .unwrap();
    assert!(!cache.exists(), "a discard deletes the index");
    w.daemon.stop().await;
}
