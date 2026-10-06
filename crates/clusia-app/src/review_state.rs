//! Each review tab's state (spec #75 §3.2): its phase (loading, failed, ready) and its UI state
//! (section, file, Unified/Split, comment editor, modal). The bridge's tells move the phase;
//! screens read it and change the UI state.

use std::collections::HashMap;

use bevy::prelude::*;
use clusia_core::config::DiffView;
use clusia_core::{PrRef, Side, ThreadRef, Verdict};
use clusia_protocol::{
    ErrorCode, LoadStep, LoadStepKind, NewsItem, PublishResult, ReviewView, StepStatus,
};

use crate::bridge::{Ask, Asks, Model, Tell};
use crate::nav::{Nav, NavSystems};

/// Diff lines shown at first and added by each *Show more*.
pub const SHOW_STEP: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReviewSection {
    Diagrams,
    Security,
    #[default]
    Diff,
    Audits,
    Comments,
    Tests,
}

impl ReviewSection {
    pub const ALL: [ReviewSection; 6] = [
        ReviewSection::Diagrams,
        ReviewSection::Security,
        ReviewSection::Diff,
        ReviewSection::Audits,
        ReviewSection::Comments,
        ReviewSection::Tests,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ReviewSection::Diagrams => "Diagrams",
            ReviewSection::Security => "Security",
            ReviewSection::Diff => "Diff",
            ReviewSection::Audits => "Audits",
            ReviewSection::Comments => "Comments",
            ReviewSection::Tests => "Tests",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DiffMode {
    #[default]
    Unified,
    Split,
}

impl From<DiffView> for DiffMode {
    fn from(view: DiffView) -> Self {
        match view {
            DiffView::Unified => DiffMode::Unified,
            DiffView::Split => DiffMode::Split,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CommentsFilter {
    #[default]
    Open,
    Resolved,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modal {
    WhatsNew,
    Finalize,
    /// `window`: asked while the window closes (Task 13).
    Leave {
        window: bool,
    },
}

/// What the comment editor writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    /// A new line comment; `start` for a range on the same side.
    Line {
        path: String,
        side: Side,
        start: Option<u32>,
        line: u32,
    },
    Reply(ThreadRef),
    General,
    /// An existing draft item, by id.
    Item(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub target: EditTarget,
    pub text: String,
    /// The daemon's refusal, shown under the text (which is kept).
    pub error: Option<String>,
    /// Set while an `AddItem`/`UpdateItem` is in flight; the bridge answers with this ticket.
    pub ticket: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FinalizeForm {
    pub summary: String,
    pub verdict: Option<Verdict>,
    pub error: Option<String>,
    /// A `Publish` is in flight.
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabUi {
    pub section: ReviewSection,
    /// The file shown in Diff; `None`: the first one.
    pub file: Option<String>,
    pub mode: DiffMode,
    pub filter: CommentsFilter,
    pub modal: Option<Modal>,
    pub editor: Option<Editor>,
    /// Diff lines shown (steps of `SHOW_STEP`).
    pub shown: usize,
    pub finalize: FinalizeForm,
}

impl Default for TabUi {
    fn default() -> Self {
        Self {
            section: ReviewSection::Diff,
            file: None,
            mode: DiffMode::Unified,
            filter: CommentsFilter::Open,
            modal: None,
            editor: None,
            shown: SHOW_STEP,
            finalize: FinalizeForm::default(),
        }
    }
}

/// An open review: the daemon's view (with the patches in `view.diff`) and what changed since
/// the user last looked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub view: ReviewView,
    pub news: Vec<NewsItem>,
    /// `Some(fetched_at)` when this is the cached copy (read-only).
    pub cached_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Opening; `steps` as the daemon reports them, `cached` once the cache answered.
    Loading {
        steps: Vec<LoadStep>,
        cached: Option<Box<Ready>>,
    },
    /// `step`: the step that failed, when the daemon said which.
    Failed {
        step: Option<LoadStepKind>,
        message: String,
        cached: Option<Box<Ready>>,
    },
    Ready(Box<Ready>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub phase: Phase,
    pub ui: TabUi,
}

impl Tab {
    pub fn loading(mode: DiffMode) -> Self {
        Self {
            phase: Phase::Loading {
                steps: Vec::new(),
                cached: None,
            },
            ui: TabUi {
                mode,
                ..TabUi::default()
            },
        }
    }

    pub fn ready(&self) -> Option<&Ready> {
        match &self.phase {
            Phase::Ready(r) => Some(r),
            _ => None,
        }
    }

    pub fn ready_mut(&mut self) -> Option<&mut Ready> {
        match &mut self.phase {
            Phase::Ready(r) => Some(r),
            _ => None,
        }
    }
}

#[derive(Resource, Debug, Default)]
pub struct ReviewTabs(pub HashMap<PrRef, Tab>);

impl ReviewTabs {
    /// *Try again*: back to loading (keeping any cached copy to show underneath) and opens
    /// again.
    pub fn retry(&mut self, pr: &PrRef, asks: &mut Asks) {
        let Some(tab) = self.0.get_mut(pr) else {
            return;
        };
        let previous = std::mem::replace(
            &mut tab.phase,
            Phase::Loading {
                steps: Vec::new(),
                cached: None,
            },
        );
        let cached = match previous {
            Phase::Loading { cached, .. } | Phase::Failed { cached, .. } => cached,
            Phase::Ready(r) if r.cached_at.is_some() => Some(r),
            Phase::Ready(_) => None,
        };
        tab.phase = Phase::Loading {
            steps: Vec::new(),
            cached,
        };
        asks.send(Ask::OpenReview(pr.clone()));
    }
}

/// Tickets for draft writes, so a late answer never lands on another editor.
#[derive(Resource, Debug, Default)]
pub struct Tickets(u64);

impl Tickets {
    /// The next ticket; the first is 1.
    pub fn issue(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Review outcomes for the screens (Task 13: toasts, closing tabs, modal errors).
#[derive(Message, Debug, Clone, PartialEq)]
pub enum ReviewEvent {
    Published {
        pr: PrRef,
        result: PublishResult,
    },
    PublishFailed {
        pr: PrRef,
        code: ErrorCode,
        message: String,
    },
    /// Closed or discarded on the daemon.
    Left(PrRef),
}

pub struct ReviewStatePlugin;

impl Plugin for ReviewStatePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReviewTabs>()
            .init_resource::<Tickets>()
            .add_message::<ReviewEvent>()
            .add_systems(Update, open_new_tabs.after(NavSystems));
    }
}

/// Every review tab in `Nav` without a `Tab` starts loading (and asks `OpenReview`); tabs no
/// longer in `Nav` are forgotten.
pub fn open_new_tabs(
    nav: Res<Nav>,
    model: Res<Model>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
) {
    if tabs.0.keys().any(|pr| !nav.reviews.contains(pr)) {
        tabs.0.retain(|pr, _| nav.reviews.contains(pr));
    }
    let new: Vec<PrRef> = nav
        .reviews
        .iter()
        .filter(|pr| !tabs.0.contains_key(*pr))
        .cloned()
        .collect();
    let mode = DiffMode::from(model.snapshot.config.appearance.diff_view);
    for pr in new {
        tabs.0.insert(pr.clone(), Tab::loading(mode));
        asks.send(Ask::OpenReview(pr));
    }
}

/// Applies a review tell to the tabs; outcomes for the screens go to `events`. Gives back a
/// tell it does not handle (not about reviews, or a refusal no editor waits on). A review that
/// opens after its tab closed is closed on the daemon through `asks`.
pub(crate) fn apply(
    tabs: &mut ReviewTabs,
    asks: &mut Asks,
    tell: Tell,
    events: &mut Vec<ReviewEvent>,
) -> Option<Tell> {
    match tell {
        Tell::Step(step) => {
            if let Some(Tab {
                phase: Phase::Loading { steps, .. },
                ..
            }) = tabs.0.get_mut(&step.pr)
            {
                match steps.iter_mut().find(|s| s.step == step.step) {
                    Some(s) => *s = step,
                    None => steps.push(step),
                }
            }
        }
        Tell::CachedAvailable {
            pr,
            view,
            fetched_at,
        } => {
            if let Some(tab) = tabs.0.get_mut(&pr)
                && let Phase::Loading { cached, .. } | Phase::Failed { cached, .. } = &mut tab.phase
            {
                *cached = Some(Box::new(Ready {
                    view: *view,
                    news: Vec::new(),
                    cached_at: Some(fetched_at),
                }));
            }
        }
        Tell::Opened { pr, view, news } => match tabs.0.get_mut(&pr) {
            Some(tab) => {
                tab.phase = Phase::Ready(Box::new(Ready {
                    view: *view,
                    news,
                    cached_at: None,
                }));
            }
            // The tab closed while it loaded: the daemon's review is left too.
            None => asks.send(Ask::CloseReview(pr)),
        },
        Tell::OpenedFromCache {
            pr,
            view,
            fetched_at,
        } => match tabs.0.get_mut(&pr) {
            Some(tab) => {
                tab.phase = Phase::Ready(Box::new(Ready {
                    view: *view,
                    news: Vec::new(),
                    cached_at: Some(fetched_at),
                }));
            }
            None => asks.send(Ask::CloseReview(pr)),
        },
        Tell::OpenFailed { pr, message, .. } => {
            if let Some(tab) = tabs.0.get_mut(&pr) {
                // A copy opened from the cache meanwhile stays; it offers *Try again*.
                let failed = match &mut tab.phase {
                    Phase::Loading { steps, cached } => Some(Phase::Failed {
                        step: steps
                            .iter()
                            .find(|s| s.status == StepStatus::Failed)
                            .map(|s| s.step),
                        message,
                        cached: cached.take(),
                    }),
                    Phase::Failed { cached, step, .. } => Some(Phase::Failed {
                        step: *step,
                        message,
                        cached: cached.take(),
                    }),
                    Phase::Ready(_) => None,
                };
                if let Some(phase) = failed {
                    tab.phase = phase;
                }
            }
        }
        Tell::ReviewFile(review) => {
            if let Some(ready) = tabs.0.get_mut(&review.pr).and_then(Tab::ready_mut) {
                ready.view.review = *review;
            }
        }
        Tell::Conversation { pr, conversation } => {
            if let Some(ready) = tabs.0.get_mut(&pr).and_then(Tab::ready_mut) {
                ready.view.conversation = Some(conversation);
            }
        }
        Tell::Saved { pr, ticket } => {
            if let Some(tab) = tabs.0.get_mut(&pr)
                && tab.ui.editor.as_ref().and_then(|e| e.ticket) == Some(ticket)
            {
                tab.ui.editor = None;
            }
        }
        Tell::Refused {
            pr,
            ticket,
            message,
        } => match tabs.0.get_mut(&pr).and_then(|t| t.ui.editor.as_mut()) {
            Some(editor) if editor.ticket == Some(ticket) => {
                editor.error = Some(message);
                editor.ticket = None;
            }
            // No editor waits on it (a resolve, an item edited in Finalize): a warning toast.
            _ => {
                return Some(Tell::Refused {
                    pr,
                    ticket,
                    message,
                });
            }
        },
        Tell::Published { pr, result } => {
            not_busy(tabs, &pr);
            events.push(ReviewEvent::Published { pr, result });
        }
        Tell::PublishFailed { pr, code, message } => {
            not_busy(tabs, &pr);
            events.push(ReviewEvent::PublishFailed { pr, code, message });
        }
        Tell::Left(pr) => {
            not_busy(tabs, &pr);
            events.push(ReviewEvent::Left(pr));
        }
        other => return Some(other),
    }
    None
}

fn not_busy(tabs: &mut ReviewTabs, pr: &PrRef) {
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.finalize.busy = false;
    }
}

#[cfg(test)]
mod tests {
    use bevy::ecs::message::Messages;
    use clusia_core::config::DiffView;
    use clusia_core::{DraftKind, ReviewState};
    use clusia_protocol::WindowTarget;

    use super::*;
    use crate::bridge::{self, Toasts};
    use crate::fixture;
    use crate::testing::{self, NOW};

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn step(kind: LoadStepKind, status: StepStatus) -> Tell {
        Tell::Step(LoadStep {
            pr: pr(),
            step: kind,
            status,
            message: None,
        })
    }

    fn view() -> Box<ReviewView> {
        Box::new(fixture::demo_review(NOW).0)
    }

    /// An app with the demo tab open and loading.
    fn loading_app() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: pr() });
        app.update();
        app
    }

    fn events(app: &mut App) -> Vec<ReviewEvent> {
        app.world_mut()
            .resource_mut::<Messages<ReviewEvent>>()
            .drain()
            .collect()
    }

    #[test]
    fn tabs_open_with_the_nav_and_go_with_it() {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Model>()
            .snapshot
            .config
            .appearance
            .diff_view = DiffView::Split;
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: pr() });
        app.update();
        let tab = testing::tab(&app, &pr());
        assert_eq!(
            tab.phase,
            Phase::Loading {
                steps: vec![],
                cached: None
            }
        );
        assert_eq!(
            tab.ui.mode,
            DiffMode::Split,
            "seeded from appearance.diff_view"
        );
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert_eq!(tab.ui.shown, SHOW_STEP);
        assert_eq!(testing::recorded(&mut app), [Ask::OpenReview(pr())]);
        app.update();
        assert!(testing::recorded(&mut app).is_empty(), "asked once");
        app.world_mut().resource_mut::<Nav>().close_review(&pr());
        app.update();
        assert!(app.world().resource::<ReviewTabs>().0.is_empty());
    }

    #[test]
    fn steps_then_cache_then_opened() {
        let mut app = loading_app();
        testing::tell(&mut app, step(LoadStepKind::Repo, StepStatus::Running));
        testing::tell(&mut app, step(LoadStepKind::Repo, StepStatus::Done));
        testing::tell(&mut app, step(LoadStepKind::Branch, StepStatus::Running));
        testing::tell(
            &mut app,
            Tell::CachedAvailable {
                pr: pr(),
                view: view(),
                fetched_at: NOW - 3600,
            },
        );
        let Phase::Loading { steps, cached } = testing::tab(&app, &pr()).phase else {
            panic!("still loading");
        };
        let seen: Vec<(LoadStepKind, StepStatus)> =
            steps.iter().map(|s| (s.step, s.status)).collect();
        assert_eq!(
            seen,
            [
                (LoadStepKind::Repo, StepStatus::Done),
                (LoadStepKind::Branch, StepStatus::Running)
            ],
            "a later status replaces the step's earlier one"
        );
        assert_eq!(cached.unwrap().cached_at, Some(NOW - 3600));
        let (fresh, news) = fixture::demo_review(NOW);
        testing::tell(
            &mut app,
            Tell::Opened {
                pr: pr(),
                view: Box::new(fresh.clone()),
                news: news.clone(),
            },
        );
        let tab = testing::tab(&app, &pr());
        assert_eq!(
            tab.phase,
            Phase::Ready(Box::new(Ready {
                view: fresh,
                news,
                cached_at: None
            }))
        );
        assert_eq!(tab.ui.modal, None, "What's new is the screen's call");
    }

    #[test]
    fn failure_names_the_step_and_keeps_the_cache() {
        let mut app = loading_app();
        testing::tell(&mut app, step(LoadStepKind::Repo, StepStatus::Done));
        testing::tell(&mut app, step(LoadStepKind::Branch, StepStatus::Failed));
        testing::tell(
            &mut app,
            Tell::CachedAvailable {
                pr: pr(),
                view: view(),
                fetched_at: NOW - 3600,
            },
        );
        testing::tell(
            &mut app,
            Tell::OpenFailed {
                pr: pr(),
                message: "fatal: couldn't find remote ref".into(),
                cache: true,
            },
        );
        let Phase::Failed {
            step,
            message,
            cached,
        } = testing::tab(&app, &pr()).phase
        else {
            panic!("failed");
        };
        assert_eq!(step, Some(LoadStepKind::Branch));
        assert_eq!(message, "fatal: couldn't find remote ref");
        assert!(cached.is_some());
    }

    #[test]
    fn retry_loads_again_with_the_cached_copy_underneath() {
        let mut app = loading_app();
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::OpenedFromCache {
                pr: pr(),
                view: view(),
                fetched_at: NOW - 7200,
            },
        );
        assert_eq!(
            testing::ready(&app, &pr()).cached_at,
            Some(NOW - 7200),
            "read-only cached copy"
        );
        {
            let world = app.world_mut();
            world.resource_scope(|world, mut tabs: Mut<ReviewTabs>| {
                tabs.retry(&pr(), &mut world.resource_mut::<Asks>());
            });
        }
        let Phase::Loading { steps, cached } = testing::tab(&app, &pr()).phase else {
            panic!("loading again");
        };
        assert!(steps.is_empty());
        assert_eq!(cached.unwrap().cached_at, Some(NOW - 7200));
        assert_eq!(testing::recorded(&mut app), [Ask::OpenReview(pr())]);
        testing::tell(
            &mut app,
            Tell::OpenFailed {
                pr: pr(),
                message: "offline".into(),
                cache: true,
            },
        );
        {
            let world = app.world_mut();
            world.resource_scope(|world, mut tabs: Mut<ReviewTabs>| {
                tabs.retry(&pr(), &mut world.resource_mut::<Asks>());
            });
        }
        assert!(matches!(
            testing::tab(&app, &pr()).phase,
            Phase::Loading {
                cached: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn editor_outcomes_follow_their_ticket() {
        let mut app = testing::demo_review_app();
        let editor = Editor {
            target: EditTarget::General,
            text: "Nice cleanup".into(),
            error: None,
            ticket: Some(7),
        };
        let set = |app: &mut App, editor: &Editor| {
            app.world_mut()
                .resource_mut::<ReviewTabs>()
                .0
                .get_mut(&pr())
                .unwrap()
                .ui
                .editor = Some(editor.clone());
        };
        set(&mut app, &editor);
        testing::tell(
            &mut app,
            Tell::Saved {
                pr: pr(),
                ticket: 6,
            },
        );
        assert_eq!(
            testing::tab(&app, &pr()).ui.editor.as_ref(),
            Some(&editor),
            "another ticket"
        );
        testing::tell(
            &mut app,
            Tell::Refused {
                pr: pr(),
                ticket: 7,
                message: "the comment is empty".into(),
            },
        );
        let after = testing::tab(&app, &pr()).ui.editor.unwrap();
        assert_eq!(after.text, "Nice cleanup", "the text is kept");
        assert_eq!(after.error.as_deref(), Some("the comment is empty"));
        assert_eq!(after.ticket, None);
        assert!(app.world().resource::<Toasts>().0.is_empty());
        set(
            &mut app,
            &Editor {
                ticket: Some(8),
                ..editor
            },
        );
        testing::tell(
            &mut app,
            Tell::Saved {
                pr: pr(),
                ticket: 8,
            },
        );
        assert_eq!(testing::tab(&app, &pr()).ui.editor, None);
        testing::tell(
            &mut app,
            Tell::Refused {
                pr: pr(),
                ticket: 9,
                message: "this thread is already marked to resolve".into(),
            },
        );
        let toasts = &app.world().resource::<Toasts>().0;
        assert_eq!(toasts.len(), 1, "no editor waits on it: a toast");
        assert!(toasts[0].warning);
        assert_eq!(toasts[0].text, "this thread is already marked to resolve");
    }

    #[test]
    fn outcomes_become_review_events() {
        let mut app = testing::demo_review_app();
        let busy = |app: &mut App| {
            app.world_mut()
                .resource_mut::<ReviewTabs>()
                .0
                .get_mut(&pr())
                .unwrap()
                .ui
                .finalize
                .busy = true;
        };
        busy(&mut app);
        let result = PublishResult {
            url: Some("https://github.com/rzorzal/clusia/pull/123#pullrequestreview-1".into()),
            closed: false,
            unresolved: vec!["PRRT_demo_refresh_41".into()],
        };
        testing::tell(
            &mut app,
            Tell::Published {
                pr: pr(),
                result: result.clone(),
            },
        );
        assert_eq!(
            events(&mut app),
            [ReviewEvent::Published { pr: pr(), result }]
        );
        assert!(!testing::tab(&app, &pr()).ui.finalize.busy);
        busy(&mut app);
        testing::tell(
            &mut app,
            Tell::PublishFailed {
                pr: pr(),
                code: ErrorCode::Conflict,
                message: "the pull request moved".into(),
            },
        );
        assert_eq!(
            events(&mut app),
            [ReviewEvent::PublishFailed {
                pr: pr(),
                code: ErrorCode::Conflict,
                message: "the pull request moved".into()
            }]
        );
        assert!(!testing::tab(&app, &pr()).ui.finalize.busy);
        testing::tell(&mut app, Tell::Left(pr()));
        assert_eq!(events(&mut app), [ReviewEvent::Left(pr())]);
    }

    #[test]
    fn review_file_and_conversation_refresh_the_ready_view() {
        let mut app = testing::demo_review_app();
        let mut review = testing::ready(&app, &pr()).view.review;
        review.state = ReviewState::Saved;
        review.draft.items.pop();
        testing::tell(&mut app, Tell::ReviewFile(Box::new(review.clone())));
        assert_eq!(testing::ready(&app, &pr()).view.review, review);
        let conversation = clusia_core::PrConversation::default();
        testing::tell(
            &mut app,
            Tell::Conversation {
                pr: pr(),
                conversation: conversation.clone(),
            },
        );
        assert_eq!(
            testing::ready(&app, &pr()).view.conversation,
            Some(conversation)
        );
    }

    #[test]
    fn a_late_open_for_a_closed_tab_leaves_the_review() {
        let mut app = loading_app();
        app.world_mut().resource_mut::<Nav>().close_review(&pr());
        app.update();
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::Opened {
                pr: pr(),
                view: view(),
                news: Vec::new(),
            },
        );
        assert!(
            app.world().resource::<ReviewTabs>().0.is_empty(),
            "the closed tab does not come back"
        );
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::CloseReview(pr())],
            "the daemon's review is closed too"
        );
        testing::tell(
            &mut app,
            Tell::OpenedFromCache {
                pr: pr(),
                view: view(),
                fetched_at: NOW - 3600,
            },
        );
        assert!(app.world().resource::<ReviewTabs>().0.is_empty());
        assert_eq!(testing::recorded(&mut app), [Ask::CloseReview(pr())]);
    }

    #[test]
    fn tickets_count_from_one() {
        let mut tickets = Tickets::default();
        assert_eq!((tickets.issue(), tickets.issue()), (1, 2));
    }

    #[test]
    fn demo_answers_play_the_daemon() {
        let mut app = testing::app(fixture::demo(NOW));
        app.add_systems(Update, bridge::demo_answers);
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: pr() });
        testing::settle(&mut app);
        let ready = testing::ready(&app, &pr());
        assert_eq!(ready.view, fixture::demo_review(NOW).0);
        assert_eq!(ready.news.len(), 5);
        let send = |app: &mut App, ask: Ask| {
            app.world_mut().resource_mut::<Asks>().send(ask);
            testing::settle(app);
        };
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr())
            .unwrap()
            .ui
            .editor = Some(Editor {
            target: EditTarget::General,
            text: "Ship it after the lock fix.".into(),
            error: None,
            ticket: Some(1),
        });
        send(
            &mut app,
            Ask::AddItem {
                pr: pr(),
                kind: DraftKind::General,
                anchor: None,
                thread: None,
                body: "Ship it after the lock fix.".into(),
                ticket: 1,
            },
        );
        let ready = testing::ready(&app, &pr());
        assert_eq!(ready.view.review.draft.items.len(), 4);
        assert_eq!(testing::tab(&app, &pr()).ui.editor, None, "saved");
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr())
            .unwrap()
            .ui
            .editor = Some(Editor {
            target: EditTarget::General,
            text: "   ".into(),
            error: None,
            ticket: Some(2),
        });
        send(
            &mut app,
            Ask::AddItem {
                pr: pr(),
                kind: DraftKind::General,
                anchor: None,
                thread: None,
                body: "   ".into(),
                ticket: 2,
            },
        );
        let editor = testing::tab(&app, &pr()).ui.editor.unwrap();
        assert_eq!(editor.error.as_deref(), Some("the comment is empty"));
        send(
            &mut app,
            Ask::RemoveItem {
                pr: pr(),
                id: "i1".into(),
            },
        );
        assert_eq!(testing::ready(&app, &pr()).view.review.draft.items.len(), 3);
        send(
            &mut app,
            Ask::Publish {
                pr: pr(),
                verdict: Verdict::Comment,
                summary: String::new(),
            },
        );
        let published: Vec<ReviewEvent> = events(&mut app);
        assert!(matches!(
            published.as_slice(),
            [ReviewEvent::Published { result, .. }] if !result.closed
        ));
        let other: PrRef = "rzorzal/clusia#98".parse().unwrap();
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: other.clone() });
        testing::settle(&mut app);
        assert!(matches!(
            testing::tab(&app, &other).phase,
            Phase::Failed {
                step: Some(LoadStepKind::Repo),
                ..
            }
        ));
    }
}
