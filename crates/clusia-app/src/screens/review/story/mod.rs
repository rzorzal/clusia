//! The Diff's Story view: the change told as ordered steps. `model` holds what the daemon said;
//! the systems here ask for it and start the agent's turn when the view opens.

use bevy::prelude::*;
use clusia_core::draft::{DraftKind, Origin};
use clusia_core::{PrRef, Review};

use crate::bridge::{Ask, Asks, Connection, Model};
use crate::review_state::{DiffMode, ReviewSection, ReviewTabs, TabUi};
use crate::screens::review::ReviewSystems;
use crate::screens::review::agent::harness_ready;

pub mod model;
pub mod ui;
pub mod view;

pub use model::{Stories, StoryModel};

/// The Story frame's root inside the section body.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StoryRoot(pub PrRef);

pub struct StoryPlugin;

impl Plugin for StoryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (load_story, trigger_write).chain().after(ReviewSystems),
        );
        ui::add_systems(app);
    }
}

/// The Story frame: the root that holds the list and the body.
pub(crate) fn frame(p: &mut bevy::ecs::hierarchy::ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            flex_grow: 1.0,
            min_width: px(0),
            min_height: px(0),
            ..default()
        },
        StoryRoot(pr.clone()),
    ))
    .with_children(|root| ui::parts(root, pr));
}

/// The status bar's middle text while the Story view shows, after the draft summary; `None`
/// keeps the draft summary alone.
pub(crate) fn status_text(ui: &TabUi, model: Option<&StoryModel>, draft: &str) -> Option<String> {
    let m = model.filter(|m| m.received)?;
    let tail = if m.busy() {
        "writing the story".to_string()
    } else if let Some((since, now)) = view::compare_active(ui, m) {
        format!("comparing story versions {since} → {now}")
    } else if m.outdated() {
        "story from an older commit".to_string()
    } else {
        let (read, total) = view::step_counts(ui, m);
        if total == 0 {
            return None;
        }
        format!("{read} of {total} steps read")
    };
    Some(format!("{draft} · {tail}"))
}

/// What a scene shows of the Story, one per mockup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryStage {
    /// Version 1 written for the head, step 2 current.
    Written,
    /// No version yet, the agent writing.
    Static,
    /// Version 1 written for an older commit than the head.
    Outdated,
    /// Versions 1 and 2, the comparison on.
    Compare,
}

/// Leaves the tab of `pr` on the Diff section in the Story mode with the model of `stage`.
pub fn stage(world: &mut World, pr: &PrRef, stage: StoryStage, now: i64) {
    let demo = crate::fixture::demo_story(now);
    let mut first = demo.first;
    if stage == StoryStage::Outdated {
        first.commit = crate::fixture::DEMO_OLD_SHA.into();
    }
    let (versions, state, step, compare) = match stage {
        StoryStage::Written | StoryStage::Outdated => (
            vec![first],
            clusia_protocol::StoryState::Idle,
            Some("one-at-a-time"),
            false,
        ),
        StoryStage::Static => (
            Vec::new(),
            clusia_protocol::StoryState::Writing,
            None,
            false,
        ),
        StoryStage::Compare => (
            vec![first, demo.second],
            clusia_protocol::StoryState::Idle,
            Some("one-at-a-time"),
            true,
        ),
    };
    let read = if stage == StoryStage::Static {
        Vec::new()
    } else {
        demo.read
    };
    let tell = crate::bridge::StoryTell::Loaded {
        pr: pr.clone(),
        versions: versions
            .iter()
            .map(|v| clusia_protocol::StoryVersionInfo {
                number: v.number,
                commit: v.commit.clone(),
                state: v.state.clone(),
            })
            .collect(),
        latest: versions.last().cloned().map(Box::new),
        state,
        static_steps: demo.static_steps,
        read,
        head: demo.head,
        new_commits: if stage == StoryStage::Outdated { 2 } else { 0 },
    };
    world.resource_mut::<Stories>().apply(&tell);
    if let Some(tab) = world.resource_mut::<ReviewTabs>().0.get_mut(pr) {
        tab.ui.section = ReviewSection::Diff;
        tab.ui.mode = DiffMode::Story;
        tab.ui.story_step = step.map(str::to_string);
        tab.ui.story_version = None;
        tab.ui.story_compare = compare;
    }
}

/// Whether the tab shows the Story: the Diff section in the Story mode.
fn shows_story(ui: &TabUi) -> bool {
    ui.section == ReviewSection::Diff && ui.mode == DiffMode::Story
}

/// Asks for the story of each ready tab that shows it once the connection is live, forgets the
/// story of closed tabs, and asks for an older version when the tab steps to one that has not
/// arrived. A cached copy asks when the fresh one arrives.
fn load_story(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut stories: ResMut<Stories>,
    mut asks: ResMut<Asks>,
) {
    if stories.0.keys().any(|pr| !tabs.0.contains_key(pr)) {
        stories.0.retain(|pr, _| tabs.0.contains_key(pr));
    }
    if model.connection != Connection::Live {
        return;
    }
    for (pr, tab) in &tabs.0 {
        if !shows_story(&tab.ui) || !tab.ready().is_some_and(|r| r.cached_at.is_none()) {
            continue;
        }
        let head = tab.ready().map_or("", |r| r.view.review.head_sha.as_str());
        if let Some(m) = stories.0.get_mut(pr)
            && m.asked
            && !m.asked_head.is_empty()
            && m.asked_head != head
        {
            m.asked = false;
            m.received = false;
        }
        match stories.0.get_mut(pr) {
            Some(m) if m.asked => {
                let Some(number) = tab.ui.story_version else {
                    continue;
                };
                let wanted = m.received
                    && !m.failed.contains(&number)
                    && !m.loaded.contains_key(&number)
                    && !m.requested.contains(&number)
                    && m.versions.iter().any(|v| v.number == number);
                if wanted {
                    m.requested.insert(number);
                    asks.send(Ask::GetStoryVersion {
                        pr: pr.clone(),
                        number,
                    });
                }
            }
            _ => {
                let m = stories.0.entry(pr.clone()).or_default();
                m.asked = true;
                m.asked_head = head.to_string();
                asks.send(Ask::GetStory(pr.clone()));
            }
        }
    }
}

/// Sends `WriteStory` when the Story view shows a review that has no version at all: the setting
/// is on, the daemon is idle, and no earlier send waits for an answer. A failed or stopped run
/// leaves a version, so it never starts again on its own.
pub(super) fn trigger_write(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut stories: ResMut<Stories>,
    mut asks: ResMut<Asks>,
) {
    if model.connection != Connection::Live || !model.snapshot.config.harness.story {
        return;
    }
    for (pr, tab) in &tabs.0 {
        if !shows_story(&tab.ui)
            || !tab.ready().is_some_and(|r| r.cached_at.is_none())
            || !harness_ready(
                &model.snapshot,
                crate::agent_name::kind_of_tab(&model.snapshot, tab),
            )
        {
            continue;
        }
        let Some(m) = stories.0.get_mut(pr) else {
            continue;
        };
        if m.received
            && !m.write_sent
            && m.versions.is_empty()
            && m.latest().is_none()
            && m.state == clusia_protocol::StoryState::Idle
        {
            m.write_sent = true;
            asks.send(Ask::WriteStory(pr.clone()));
        }
    }
}

/// What the demo daemon does with a comment on a step: a general draft item that names the step.
pub(crate) fn demo_step_comment(
    review: &mut Review,
    title: &str,
    body: &str,
    now: i64,
) -> Result<(), String> {
    review
        .draft
        .add_as(
            Origin::Human,
            DraftKind::General,
            None,
            None,
            &format!("On \"{title}\": {body}"),
            now,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{CallsTell, Connection, StoryTell, Tell};
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use clusia_protocol::StoryState;

    /// The demo has no harness program; a Story is written only once one is set up.
    fn story_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        testing::set_config_locally(&mut app, "harness.program", "/opt/homebrew/bin/claude");
        let pr = testing::open_ready(&mut app, false);
        set_mode(&mut app, &pr, DiffMode::Story);
        (app, pr)
    }

    fn move_head(app: &mut App, pr: &PrRef, sha: &str) {
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(pr)
            .unwrap()
            .ready_mut()
            .unwrap()
            .view
            .review
            .head_sha = sha.into();
        testing::settle(app);
    }

    fn gets(asks: &[Ask], pr: &PrRef) -> usize {
        asks.iter()
            .filter(|a| **a == Ask::GetStory(pr.clone()))
            .count()
    }

    fn set_mode(app: &mut App, pr: &PrRef, mode: DiffMode) {
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(pr)
            .unwrap()
            .ui
            .mode = mode;
        testing::settle(app);
    }

    fn empty(pr: &PrRef, state: StoryState) -> Tell {
        Tell::Story(StoryTell::Loaded {
            pr: pr.clone(),
            versions: Vec::new(),
            latest: None,
            state,
            static_steps: fixture::demo_story(NOW).static_steps,
            read: Vec::new(),
            head: "9be1d07c3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d".into(),
            new_commits: 0,
        })
    }

    fn writes(asks: &[Ask]) -> usize {
        asks.iter()
            .filter(|a| matches!(a, Ask::WriteStory(_)))
            .count()
    }

    #[test]
    fn the_story_is_asked_once_when_the_story_view_shows_and_not_before() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::settle(&mut app);
        assert!(!testing::recorded(&mut app).contains(&Ask::GetStory(pr.clone())));
        set_mode(&mut app, &pr, DiffMode::Story);
        assert_eq!(
            testing::recorded(&mut app)
                .iter()
                .filter(|a| **a == Ask::GetStory(pr.clone()))
                .count(),
            1
        );
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty(), "only once");
        testing::tell(&mut app, Tell::Lost("gone".into()));
        assert!(!app.world().resource::<Stories>().0[&pr].asked);
        app.world_mut().resource_mut::<Model>().connection = Connection::Live;
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).contains(&Ask::GetStory(pr)));
    }

    #[test]
    fn a_review_with_no_version_writes_once_when_the_answer_arrives() {
        let (mut app, pr) = story_app();
        testing::recorded(&mut app);
        testing::tell(&mut app, empty(&pr, StoryState::Idle));
        testing::settle(&mut app);
        let asks = testing::recorded(&mut app);
        assert_eq!(writes(&asks), 1, "{asks:?}");
        assert!(asks.contains(&Ask::WriteStory(pr.clone())));
        testing::settle(&mut app);
        assert_eq!(writes(&testing::recorded(&mut app)), 0, "not again");
        // The run ends: the old answer is stale, so nothing writes until the daemon answers again.
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::State {
                pr: pr.clone(),
                state: StoryState::Writing,
            }),
        );
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::State {
                pr: pr.clone(),
                state: StoryState::Idle,
            }),
        );
        testing::settle(&mut app);
        let asks = testing::recorded(&mut app);
        assert_eq!(
            writes(&asks),
            0,
            "no second write when the run ends: {asks:?}"
        );
        assert!(
            asks.contains(&Ask::GetStory(pr.clone())),
            "the final versions are read"
        );
        testing::tell(&mut app, empty(&pr, StoryState::Idle));
        testing::settle(&mut app);
        assert_eq!(
            writes(&testing::recorded(&mut app)),
            1,
            "an answer with no version may write again"
        );
        testing::tell(&mut app, fixture::demo_story_tell(&pr, NOW));
        testing::settle(&mut app);
        assert_eq!(
            writes(&testing::recorded(&mut app)),
            0,
            "the answer after a run has a version"
        );
    }

    #[test]
    fn nothing_writes_with_a_version_a_run_the_setting_off_the_wrong_view_or_no_link() {
        let (mut app, pr) = story_app();
        testing::recorded(&mut app);

        testing::tell(&mut app, fixture::demo_story_tell(&pr, NOW));
        testing::settle(&mut app);
        assert_eq!(writes(&testing::recorded(&mut app)), 0, "a version exists");

        for state in [
            StoryState::Writing,
            StoryState::Waiting,
            StoryState::Failed {
                message: "x".into(),
            },
        ] {
            testing::tell(&mut app, empty(&pr, state));
            testing::settle(&mut app);
            assert_eq!(writes(&testing::recorded(&mut app)), 0);
        }

        testing::set_config_locally(&mut app, "harness.story", "false");
        testing::tell(&mut app, empty(&pr, StoryState::Idle));
        testing::settle(&mut app);
        assert_eq!(
            writes(&testing::recorded(&mut app)),
            0,
            "harness.story is off"
        );
        testing::set_config_locally(&mut app, "harness.story", "true");
        // Turning the setting back on writes for the answer that waited, which is not what is tested next.
        testing::recorded(&mut app);

        set_mode(&mut app, &pr, DiffMode::Unified);
        testing::tell(&mut app, empty(&pr, StoryState::Idle));
        testing::settle(&mut app);
        assert_eq!(
            writes(&testing::recorded(&mut app)),
            0,
            "Story is not the view"
        );
        set_mode(&mut app, &pr, DiffMode::Story);
        testing::recorded(&mut app);

        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        testing::tell(&mut app, empty(&pr, StoryState::Idle));
        testing::settle(&mut app);
        assert_eq!(
            writes(&testing::recorded(&mut app)),
            0,
            "no live connection"
        );
    }

    #[test]
    fn the_cached_copy_does_not_write() {
        let (mut app, pr) = story_app();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr).unwrap().ready_mut().unwrap().cached_at = Some(NOW - 60);
        }
        testing::recorded(&mut app);
        testing::tell(&mut app, empty(&pr, StoryState::Idle));
        testing::settle(&mut app);
        assert_eq!(writes(&testing::recorded(&mut app)), 0);
    }

    #[test]
    fn an_older_version_is_asked_when_the_tab_steps_to_it() {
        let (mut app, pr) = story_app();
        testing::tell(&mut app, fixture::demo_story_tell(&pr, NOW));
        testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .story_version = Some(1);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetStoryVersion {
                pr: pr.clone(),
                number: 1
            }]
        );
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty(), "once");
    }

    #[test]
    fn the_tab_and_the_story_go_together() {
        let (mut app, pr) = story_app();
        assert!(app.world().resource::<Stories>().0.contains_key(&pr));
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .close_review(&pr);
        testing::settle(&mut app);
        assert!(!app.world().resource::<Stories>().0.contains_key(&pr));
    }

    #[test]
    fn the_status_says_what_the_story_is_doing_after_the_draft_summary() {
        use crate::screens::review::story::model::StoryModel;
        let draft = "Draft saved · 4 items";
        let loaded = |tell: Tell| {
            let mut m = StoryModel::default();
            let Tell::Story(t) = tell else {
                panic!("a story tell")
            };
            m.apply(&t);
            m
        };
        let ui = TabUi::default();
        assert_eq!(status_text(&ui, None, draft), None);
        assert_eq!(
            status_text(&ui, Some(&StoryModel::default()), draft),
            None,
            "nothing known yet"
        );

        let mut m = loaded(fixture::demo_story_tell(&fixture::demo_pr(), NOW));
        assert_eq!(
            status_text(&ui, Some(&m), draft).as_deref(),
            Some("Draft saved · 4 items · 1 of 6 steps read")
        );
        let compare = TabUi {
            story_compare: true,
            ..TabUi::default()
        };
        assert_eq!(
            status_text(&compare, Some(&m), draft).as_deref(),
            Some("Draft saved · 4 items · comparing story versions 1 → 2")
        );
        m.head = "feedfacefeedfacefeedfacefeedfacefeedface".into();
        assert_eq!(
            status_text(&ui, Some(&m), draft).as_deref(),
            Some("Draft saved · 4 items · story from an older commit")
        );
        m.apply(&StoryTell::State {
            pr: fixture::demo_pr(),
            state: StoryState::Writing,
        });
        assert_eq!(
            status_text(&compare, Some(&m), draft).as_deref(),
            Some("Draft saved · 4 items · writing the story"),
            "a run in progress wins over the rest"
        );
    }

    #[test]
    fn each_stage_leaves_the_tab_on_the_story_with_the_model_of_its_mockup() {
        for (stage_kind, versions, step, compare, state) in [
            (
                StoryStage::Written,
                1,
                Some("one-at-a-time"),
                false,
                StoryState::Idle,
            ),
            (StoryStage::Static, 0, None, false, StoryState::Writing),
            (
                StoryStage::Outdated,
                1,
                Some("one-at-a-time"),
                false,
                StoryState::Idle,
            ),
            (
                StoryStage::Compare,
                2,
                Some("one-at-a-time"),
                true,
                StoryState::Idle,
            ),
        ] {
            let (mut app, pr) = {
                let mut app = testing::app(fixture::demo(NOW));
                let pr = testing::open_ready(&mut app, false);
                (app, pr)
            };
            stage(app.world_mut(), &pr, stage_kind, NOW);
            testing::settle(&mut app);
            let tab = testing::tab(&app, &pr);
            assert_eq!(
                (tab.ui.section, tab.ui.mode),
                (ReviewSection::Diff, DiffMode::Story)
            );
            assert_eq!(tab.ui.story_step.as_deref(), step, "{stage_kind:?}");
            assert_eq!(
                (tab.ui.story_compare, tab.ui.story_version),
                (compare, None)
            );
            let m = &app.world().resource::<Stories>().0[&pr];
            assert_eq!(m.versions.len(), versions, "{stage_kind:?}");
            assert_eq!(m.state, state);
            assert_eq!(
                m.new_commits,
                if stage_kind == StoryStage::Outdated {
                    2
                } else {
                    0
                }
            );
            assert!(
                m.asked && m.received,
                "the loader does not ask over a staged story"
            );
            assert_eq!(
                m.outdated(),
                stage_kind == StoryStage::Outdated,
                "{stage_kind:?}"
            );
            let asks = testing::recorded(&mut app);
            assert!(
                !asks.contains(&Ask::GetStory(pr.clone()))
                    && !asks.contains(&Ask::WriteStory(pr.clone()))
            );
        }
    }

    #[test]
    fn a_finished_index_reads_the_story_again_so_the_static_order_appears() {
        let (mut app, pr) = story_app();
        let Tell::Story(StoryTell::Loaded {
            versions,
            latest,
            state,
            read,
            head,
            new_commits,
            ..
        }) = empty(&pr, StoryState::Idle)
        else {
            panic!("a loaded story");
        };
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::Loaded {
                pr: pr.clone(),
                versions,
                latest,
                state,
                static_steps: Vec::new(),
                read,
                head,
                new_commits,
            }),
        );
        testing::settle(&mut app);
        testing::recorded(&mut app);
        let index = |state| {
            Tell::Calls(CallsTell::Index {
                pr: pr.clone(),
                state,
            })
        };
        testing::tell(&mut app, index(clusia_protocol::CallsIndexState::Building));
        testing::settle(&mut app);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 0, "still building");
        testing::tell(
            &mut app,
            index(clusia_protocol::CallsIndexState::Ready {
                commit: "9be1d07c3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d".into(),
                cut: false,
            }),
        );
        testing::settle(&mut app);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 1);
        testing::tell(
            &mut app,
            index(clusia_protocol::CallsIndexState::Failed {
                message: "x".into(),
            }),
        );
        testing::settle(&mut app);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 1, "a failure too");
    }

    #[test]
    fn a_new_head_asks_again_and_the_story_then_says_it_is_outdated() {
        let (mut app, pr) = story_app();
        testing::tell(&mut app, fixture::demo_story_tell(&pr, NOW));
        testing::settle(&mut app);
        testing::recorded(&mut app);
        assert!(!testing::shows(&mut app, "Rewrite"));
        let moved = "aaaaaaa3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d0";
        move_head(&mut app, &pr, moved);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 1);
        let Tell::Story(StoryTell::Loaded {
            versions,
            latest,
            state,
            static_steps,
            read,
            ..
        }) = fixture::demo_story_tell(&pr, NOW)
        else {
            panic!("a loaded story");
        };
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::Loaded {
                pr: pr.clone(),
                versions,
                latest,
                state,
                static_steps,
                read,
                head: moved.into(),
                new_commits: 1,
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Rewrite"));
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 0, "once per head");
    }

    #[test]
    fn with_no_harness_the_static_order_says_so_and_nothing_is_written() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        set_mode(&mut app, &pr, DiffMode::Story);
        testing::recorded(&mut app);
        testing::tell(&mut app, empty(&pr, StoryState::Idle));
        testing::settle(&mut app);
        assert_eq!(writes(&testing::recorded(&mut app)), 0);
        assert!(testing::shows(
            &mut app,
            "No harness is set up. Showing the static order."
        ));
    }

    #[test]
    fn a_step_comment_in_the_demo_is_a_general_item_named_for_the_step() {
        let (view, _) = fixture::demo_review(NOW);
        let mut review = view.review;
        let before = review.draft.items.len();
        demo_step_comment(&mut review, "One refresh at a time", "Why a lock?", NOW).expect("saved");
        let item = review.draft.items.last().expect("an item");
        assert_eq!(review.draft.items.len(), before + 1);
        assert_eq!(item.kind, clusia_core::draft::DraftKind::General);
        assert_eq!(item.origin, clusia_core::draft::Origin::Human);
        assert_eq!(item.body, "On \"One refresh at a time\": Why a lock?");
        assert!(item.anchor.is_none());
    }
}
