//! The window's tabs, kept by the daemon: they come back with the window, and nothing runs
//! again for a review whose head did not move.

mod common;

use std::time::{Duration, Instant};

use clusia_core::{DraftKind, OpenTab, OpenTabs, PrRef, Side};
use clusia_harness::testkit::{FakeClaude, Script, Turn};
use clusia_protocol::{AnchorInput, Client, Command, Reply};
use common::TestDaemon;
use common::agent::{calls, fake_program, ready, until, watching};
use common::review_world::{open, pr7, world};

const DAY: i64 = 86_400;

fn pr(s: &str) -> PrRef {
    s.parse().unwrap()
}

fn now() -> i64 {
    clusia_store::now_unix()
}

async fn restore(c: &mut Client) -> OpenTabs {
    match c.request(Command::RestoreTabs).await.unwrap() {
        Reply::OpenTabs(tabs) => tabs,
        other => panic!("expected OpenTabs, got {other:?}"),
    }
}

fn prs(tabs: &OpenTabs) -> Vec<String> {
    tabs.tabs.iter().map(|t| t.pr.to_string()).collect()
}

#[tokio::test]
async fn the_tabs_come_back_in_order_with_the_one_shown_and_their_sections() {
    let daemon = TestDaemon::start().await;
    let mut c = daemon.client().await;
    assert_eq!(restore(&mut c).await, OpenTabs::default(), "none at first");
    let sent = OpenTabs {
        tabs: vec![
            OpenTabs::tab(pr("acme/widgets#7"), "comments"),
            OpenTabs::tab(pr("rzorzal/clusia#98"), "diff"),
            OpenTabs::tab(pr("acme/widgets#5"), "security"),
        ],
        selected: Some(pr("rzorzal/clusia#98")),
    };
    let before = now();
    c.request(Command::SetOpenTabs { tabs: sent.clone() })
        .await
        .unwrap();
    drop(c);
    let mut c = daemon.client().await;
    let back = restore(&mut c).await;
    assert_eq!(
        prs(&back),
        ["acme/widgets#7", "rzorzal/clusia#98", "acme/widgets#5"]
    );
    assert_eq!(back.selected, sent.selected);
    let sections: Vec<&str> = back.tabs.iter().map(|t| t.section.as_str()).collect();
    assert_eq!(sections, ["comments", "diff", "security"]);
    assert!(back.tabs.iter().all(|t| t.seen_at >= before), "seen now");
    assert!(daemon.paths.open_tabs_file().exists());
}

#[tokio::test]
async fn a_window_that_comes_back_reads_the_cache_and_summarizes_nothing_again() {
    let w = world().await;
    let dir = fake_program(&w, Script::one(Turn::answer("The summary."))).await;
    let mut watcher = watching(&w).await;
    let mut window = w.daemon.client().await;
    open(&mut window).await;
    until(&mut watcher, ready).await;
    calls(&dir, 1).await;
    window
        .request(Command::SetOpenTabs {
            tabs: OpenTabs {
                tabs: vec![OpenTabs::tab(pr7(), "diff")],
                selected: Some(pr7()),
            },
        })
        .await
        .unwrap();
    // The window closes without leaving the review.
    drop(window);

    let mut window = w.daemon.client().await;
    let back = restore(&mut window).await;
    assert_eq!(prs(&back), ["acme/widgets#7"]);
    assert!(
        w.daemon.paths.review_file(&pr7()).exists(),
        "the review is still open"
    );
    match window
        .request(Command::GetCachedReview { pr: pr7() })
        .await
        .unwrap()
    {
        Reply::Cached(cached) => assert_eq!(cached.view.review.pr, pr7()),
        other => panic!("expected the cached copy, got {other:?}"),
    }
    open(&mut window).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        FakeClaude::calls(&dir).len(),
        1,
        "the same head is not summarized again"
    );
}

/// Writes the tabs file as a window would have left it, `days` ago for each tab.
fn left_tabs(daemon: &TestDaemon, tabs: &[(PrRef, i64)]) {
    let tabs = OpenTabs {
        tabs: tabs
            .iter()
            .map(|(pr, days)| OpenTab {
                seen_at: now() - days * DAY,
                ..OpenTabs::tab(pr.clone(), "diff")
            })
            .collect(),
        selected: None,
    };
    clusia_store::save_open_tabs(&daemon.paths, &tabs).unwrap();
}

async fn restore_until(c: &mut Client, what: &str, done: impl Fn(&OpenTabs) -> bool) -> OpenTabs {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let tabs = restore(c).await;
        if done(&tabs) {
            return tabs;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_idle_tab_with_an_empty_draft_is_closed_when_the_window_starts() {
    let w = world().await;
    let mut cli = w.daemon.client().await;
    open(&mut cli).await;
    let gone = pr("acme/widgets#8");
    left_tabs(&w.daemon, &[(pr7(), 15), (gone.clone(), 30)]);
    let mut window = w.daemon.client().await;
    let tabs = restore(&mut window).await;
    assert_eq!(
        prs(&tabs),
        ["acme/widgets#7"],
        "a review another client holds stays; one with no review goes"
    );
    // Once nothing holds it, the idle review goes like an empty tab's ×.
    drop(cli);
    let tabs = restore_until(&mut window, "the idle tab to close", |t| t.tabs.is_empty()).await;
    assert_eq!(tabs, OpenTabs::default());
    assert!(!w.daemon.paths.review_file(&pr7()).exists(), "cleaned up");
    assert!(
        clusia_store::load_open_tabs(&w.daemon.paths)
            .unwrap()
            .tabs
            .is_empty(),
        "and forgotten"
    );
}

#[tokio::test]
async fn an_idle_tab_with_a_draft_or_seen_lately_stays() {
    let w = world().await;
    let mut cli = w.daemon.client().await;
    open(&mut cli).await;
    cli.request(Command::AddDraftItem {
        pr: pr7(),
        thread: None,
        kind: DraftKind::LineComment,
        anchor: Some(AnchorInput {
            path: "feature.txt".into(),
            line: 2,
            start_line: None,
            side: Side::Right,
        }),
        body: "rename".into(),
    })
    .await
    .unwrap();
    drop(cli);
    let recent = pr("acme/widgets#9");
    left_tabs(&w.daemon, &[(pr7(), 40), (recent, 13)]);
    let mut window = w.daemon.client().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tabs = restore(&mut window).await;
    assert_eq!(prs(&tabs), ["acme/widgets#7", "acme/widgets#9"]);
    assert!(
        w.daemon.paths.review_file(&pr7()).exists(),
        "the draft is kept"
    );
}
