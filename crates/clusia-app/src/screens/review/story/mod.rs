//! The Diff's Story view: the change told as ordered steps. `model` holds what the daemon said;
//! the systems here ask for it and start the agent's turn when the view opens.

use bevy::prelude::*;
use clusia_core::draft::{DraftKind, Origin};
use clusia_core::{PrRef, Review};

use crate::bridge::{Ask, Asks, Connection, Model};
use crate::review_state::{DiffMode, ReviewSection, ReviewTabs, TabUi};
use crate::screens::review::ReviewSystems;

pub mod model;

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
    }
}

/// The Story frame: an empty root that the view fills.
pub(crate) fn frame(p: &mut bevy::ecs::hierarchy::ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            flex_grow: 1.0,
            min_width: px(0),
            min_height: px(0),
            ..default()
        },
        StoryRoot(pr.clone()),
    ));
}

/// The status bar's middle text while the Story view shows; `None` keeps the draft summary.
pub(crate) fn status_text(
    _ui: &TabUi,
    _model: Option<&StoryModel>,
    _draft: &str,
) -> Option<String> {
    None
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
                stories.0.entry(pr.clone()).or_default().asked = true;
                asks.send(Ask::GetStory(pr.clone()));
            }
        }
    }
}

/// Sends `WriteStory` when the Story view shows a review that has no version at all: the setting
/// is on, the daemon is idle, and no earlier send waits for an answer. A failed or stopped run
/// leaves a version, so it never starts again on its own.
fn trigger_write(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut stories: ResMut<Stories>,
    mut asks: ResMut<Asks>,
) {
    if model.connection != Connection::Live || !model.snapshot.config.harness.story {
        return;
    }
    for (pr, tab) in &tabs.0 {
        if !shows_story(&tab.ui) || !tab.ready().is_some_and(|r| r.cached_at.is_none()) {
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
    use crate::bridge::{Connection, StoryTell, Tell};
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use clusia_protocol::StoryState;

    fn story_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        set_mode(&mut app, &pr, DiffMode::Story);
        (app, pr)
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
    fn the_story_status_text_is_left_to_the_draft_summary_until_the_view_writes_one() {
        let ui = TabUi::default();
        assert_eq!(status_text(&ui, None, "Draft saved · 4 items"), None);
        let _ = ReviewSection::Diff;
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
