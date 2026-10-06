//! Opening a review (mockups `Loading.png`, `LoadFailed.png`): four leaves grow as the daemon
//! reports its load steps (repository → branch → pull request → agent). When a cached copy
//! exists it shows dimmed underneath and **Open from cache** reads it at once. A failed step
//! turns its leaf orange and shows what git or GitHub said, with **Try again**.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::PrRef;
use clusia_protocol::{LoadStep, LoadStepKind, StepStatus};

use crate::bridge::{Ask, Asks, Model};
use crate::fonts::UiFonts;
use crate::nav::pr_title;
use crate::review_state::{Phase, ReviewTabs};
use crate::screens::review::ReviewSystems;
use crate::screens::review::shell::{
    HeaderView, OpeningRegion, TryAgain, header_block, header_view, on_try_again, section_tabs,
};
use crate::theme::Swatch;
use crate::ui::kit::{Fill, Stroke, Type, Variant, button, disabled_button, panel, text};
use crate::ui::leaf::{LeafState, leaf, pulsing_dot};
use crate::ui::modal::modal_card;

const KINDS: [LoadStepKind; 4] = [
    LoadStepKind::Repo,
    LoadStepKind::Branch,
    LoadStepKind::Pr,
    LoadStepKind::Agent,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRow {
    pub name: &'static str,
    pub state: LeafState,
    pub detail: String,
    /// Paths show in the code font.
    pub mono: bool,
    /// The step running now (highlighted).
    pub current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheButton {
    Hidden,
    Available,
    /// Shown disabled with "No cached copy yet" (failures only).
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureView {
    /// `Couldn't get the branch for #98`
    pub heading: String,
    pub explanation: &'static str,
    /// `What git said` / `What GitHub said`
    pub said: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadingView {
    /// Repo, Branch, PR, Agent.
    pub leaves: Vec<LeafState>,
    pub title: String,
    pub subtitle: String,
    /// Empty when failed.
    pub rows: Vec<StepRow>,
    pub failure: Option<FailureView>,
    pub cache: CacheButton,
    /// The cached header, drawn dimmed underneath.
    pub backdrop: Option<HeaderView>,
    /// (repository, title) above the skeleton when there is no cached copy.
    pub skeleton: (String, String),
}

/// `/Users/octo/Repos/clusia` → `~/Repos/clusia`.
fn tilde(path: &str) -> String {
    match path
        .strip_prefix("/Users/")
        .and_then(|rest| rest.split_once('/'))
    {
        Some((_, rest)) => format!("~/{rest}"),
        None => path.to_string(),
    }
}

/// The worktree from `worktrees/` on: `worktrees/rzorzal~clusia~123`.
fn short_worktree(path: &str) -> String {
    match path.find("worktrees/") {
        Some(i) => path[i..].to_string(),
        None => tilde(path),
    }
}

fn name(kind: LoadStepKind) -> &'static str {
    match kind {
        LoadStepKind::Repo => "Repo",
        LoadStepKind::Branch => "Branch",
        LoadStepKind::Pr => "PR",
        LoadStepKind::Agent => "Agent",
    }
}

fn leaf_state(status: Option<StepStatus>) -> LeafState {
    match status {
        None => LeafState::Waiting,
        Some(StepStatus::Running) => LeafState::Growing,
        Some(StepStatus::Done) => LeafState::Done,
        Some(StepStatus::Failed) => LeafState::Failed,
        Some(StepStatus::Skipped) => LeafState::Skipped,
    }
}

fn row(pr: &PrRef, kind: LoadStepKind, last: Option<&LoadStep>) -> StepRow {
    let status = last.map(|s| s.status);
    let message = last.and_then(|s| s.message.clone());
    let (detail, mono) = match (kind, status) {
        (_, Some(StepStatus::Failed)) => (message.unwrap_or_else(|| "Failed".into()), true),
        (LoadStepKind::Agent, Some(StepStatus::Skipped)) => {
            ("Skipped: no harness set up".into(), false)
        }
        (_, Some(StepStatus::Skipped)) => ("Skipped".into(), false),
        (LoadStepKind::Repo, None) => (format!("Your clone of {}", pr.slug()), false),
        (LoadStepKind::Repo, Some(StepStatus::Running)) => {
            ("Cloning or updating your clone…".into(), false)
        }
        (LoadStepKind::Repo, Some(StepStatus::Done)) => match message {
            Some(path) => (format!("Your clone at {}", tilde(&path)), true),
            None => ("Your clone is ready".into(), false),
        },
        (LoadStepKind::Branch, None) => ("The pull request's head".into(), false),
        (LoadStepKind::Branch, Some(StepStatus::Running)) => ("Fetching the branch…".into(), false),
        (LoadStepKind::Branch, Some(StepStatus::Done)) => match message {
            Some(path) => (format!("Worktree ready at {}", short_worktree(&path)), true),
            None => ("Worktree ready".into(), false),
        },
        (LoadStepKind::Pr, None | Some(StepStatus::Running)) => {
            ("Files, comments and checks…".into(), false)
        }
        (LoadStepKind::Pr, Some(StepStatus::Done)) => {
            (message.unwrap_or_else(|| "Loaded".into()), false)
        }
        (LoadStepKind::Agent, None | Some(StepStatus::Running)) => {
            ("Your agent joins when a harness is set up".into(), false)
        }
        (LoadStepKind::Agent, Some(StepStatus::Done)) => ("Connected".into(), false),
    };
    StepRow {
        name: name(kind),
        state: leaf_state(status),
        detail,
        mono,
        current: status == Some(StepStatus::Running),
    }
}

fn failure(pr: &PrRef, step: Option<LoadStepKind>, message: &str) -> FailureView {
    let n = pr.number;
    let (heading, explanation, said) = match step {
        Some(LoadStepKind::Repo) => (
            format!("Couldn't reach the repository for #{n}"),
            "Clúsia could not clone or update your copy of the repository.",
            "What git said",
        ),
        Some(LoadStepKind::Branch) => (
            format!("Couldn't get the branch for #{n}"),
            "The pull request's head could not be fetched. It may have been force-pushed away or the fork deleted.",
            "What git said",
        ),
        Some(LoadStepKind::Pr) => (
            format!("Couldn't load #{n} from GitHub"),
            "GitHub did not return the pull request's files, comments and checks.",
            "What GitHub said",
        ),
        Some(LoadStepKind::Agent) | None => (
            format!("Couldn't open #{n}"),
            "clusiad could not open this review.",
            "What clusiad said",
        ),
    };
    FailureView {
        heading,
        explanation,
        said,
        message: message.to_string(),
    }
}

/// The opening screen for a tab that is loading or failed; `None` once it is ready.
/// `title` is the pull request's title when known (from the lists), for the skeleton.
pub fn loading_view(phase: &Phase, pr: &PrRef, title: &str) -> Option<LoadingView> {
    let opening = format!("Opening {} #{}", pr.slug(), pr.number);
    let (view, cached) = match phase {
        Phase::Ready(_) => return None,
        Phase::Loading { steps, cached } => {
            let last = |k: LoadStepKind| steps.iter().rev().find(|s| s.step == k);
            let rows: Vec<StepRow> = KINDS.iter().map(|&k| row(pr, k, last(k))).collect();
            (
                LoadingView {
                    leaves: rows.iter().map(|r| r.state).collect(),
                    title: opening,
                    subtitle: if cached.is_some() {
                        "Fresh data from GitHub. You can keep reading the cached version below."
                            .into()
                    } else {
                        "Getting fresh data from GitHub…".into()
                    },
                    rows,
                    failure: None,
                    cache: if cached.is_some() {
                        CacheButton::Available
                    } else {
                        CacheButton::Hidden
                    },
                    backdrop: None,
                    skeleton: (pr.slug(), title.to_string()),
                },
                cached,
            )
        }
        Phase::Failed {
            step,
            message,
            cached,
        } => {
            let failed = step.and_then(|s| KINDS.iter().position(|&k| k == s));
            let leaves = (0..KINDS.len())
                .map(|i| match failed {
                    Some(f) if i < f => LeafState::Done,
                    Some(f) if i == f => LeafState::Failed,
                    _ => LeafState::Waiting,
                })
                .collect();
            let f = failure(pr, *step, message);
            (
                LoadingView {
                    leaves,
                    title: f.heading.clone(),
                    subtitle: f.explanation.to_string(),
                    rows: Vec::new(),
                    failure: Some(f),
                    cache: if cached.is_some() {
                        CacheButton::Available
                    } else {
                        CacheButton::Missing
                    },
                    backdrop: None,
                    skeleton: (pr.slug(), title.to_string()),
                },
                cached,
            )
        }
    };
    Some(LoadingView {
        backdrop: cached.as_ref().map(|c| header_view(&c.view)),
        ..view
    })
}

/// **Open from cache**.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct OpenFromCache(pub PrRef);

/// The row of leaves (children in step order).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leaves;

/// What an `OpeningRegion` shows now.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
struct OpeningBuilt(LoadingView);

pub struct LoadingPlugin;

impl Plugin for LoadingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, rebuild_opening.in_set(ReviewSystems));
    }
}

fn rebuild_opening(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &OpeningRegion, Option<&OpeningBuilt>)>,
) {
    for (entity, region, built) in &regions {
        let Some(tab) = tabs.0.get(&region.pr) else {
            continue;
        };
        let title = pr_title(&model.snapshot, &region.pr).unwrap_or("");
        let Some(view) = loading_view(&tab.phase, &region.pr, title) else {
            continue;
        };
        if built.map(|b| &b.0) == Some(&view) {
            continue;
        }
        let pr = region.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| opening(p, &fonts, &pr, &view));
        commands.entity(entity).insert(OpeningBuilt(view));
    }
}

fn opening(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &LoadingView) {
    // Underneath: the cached header, or the title over a skeleton.
    p.spawn(Node {
        width: percent(100),
        flex_grow: 1.0,
        flex_direction: FlexDirection::Column,
        ..default()
    })
    .with_children(|under| {
        if v.failure.is_some() && v.backdrop.is_none() {
            return; // The failure card stands alone (LoadFailed.png).
        }
        match &v.backdrop {
            Some(header) => {
                header_block(under, fonts, pr, header);
                let sections: Vec<_> = crate::review_state::ReviewSection::ALL
                    .iter()
                    .map(|&s| (s, s.label().to_string(), s == Default::default()))
                    .collect();
                section_tabs(under, fonts, pr, &sections, false);
            }
            None => {
                under
                    .spawn(Node {
                        padding: UiRect::axes(px(20), px(16)),
                        column_gap: px(10),
                        align_items: AlignItems::Baseline,
                        ..default()
                    })
                    .with_children(|t| {
                        t.spawn(text(fonts, v.skeleton.0.clone(), Type::MUTED));
                        t.spawn(text(fonts, v.skeleton.1.clone(), Type::TITLE.size(17.0)));
                    });
            }
        }
        skeleton(under);
    });
    // On top: the scrim and the card.
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            width: percent(100),
            height: percent(100),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor::default(),
        Fill(if v.failure.is_some() && v.backdrop.is_none() {
            Swatch::Clear
        } else {
            Swatch::Scrim
        }),
    ))
    .with_children(|layer| {
        layer
            .spawn(modal_card(540.0))
            .insert(Node {
                width: px(540),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(16),
                padding: UiRect {
                    left: px(36),
                    right: px(36),
                    top: px(28),
                    bottom: px(26),
                },
                border: px(1).all(),
                border_radius: BorderRadius::all(px(10)),
                ..default()
            })
            .with_children(|c| card(c, fonts, pr, v));
    });
}

fn skeleton(p: &mut ChildSpawnerCommands) {
    p.spawn(Node {
        flex_grow: 1.0,
        column_gap: px(16),
        padding: UiRect::axes(px(20), px(16)),
        ..default()
    })
    .with_children(|row| {
        row.spawn(Node {
            width: px(220),
            flex_direction: FlexDirection::Column,
            row_gap: px(10),
            ..default()
        })
        .with_children(|files| {
            for w in [80, 64, 72, 58, 76, 52, 68] {
                files.spawn(panel(
                    Node {
                        width: percent(w),
                        height: px(12),
                        border_radius: BorderRadius::all(px(4)),
                        ..default()
                    },
                    Swatch::Chrome,
                ));
            }
        });
        row.spawn(panel(
            Node {
                flex_grow: 1.0,
                flex_direction: FlexDirection::Column,
                row_gap: px(10),
                padding: px(16).all(),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            Swatch::Surface,
        ))
        .with_children(|code| {
            for w in [70, 84, 46, 62, 90, 38, 74, 56, 80, 44, 66, 52] {
                code.spawn(panel(
                    Node {
                        width: percent(w),
                        height: px(10),
                        border_radius: BorderRadius::all(px(3)),
                        ..default()
                    },
                    Swatch::Chrome,
                ));
            }
        });
    });
}

fn card(c: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &LoadingView) {
    // The leaves on a ground line.
    c.spawn(Node {
        width: px(260),
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|art| {
        art.spawn((
            Node {
                column_gap: px(17),
                align_items: AlignItems::End,
                ..default()
            },
            Leaves,
        ))
        .with_children(|row| {
            for &state in &v.leaves {
                row.spawn(leaf(state));
            }
        });
        art.spawn(panel(
            Node {
                width: px(224),
                height: px(2),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            Swatch::Line,
        ));
    });
    c.spawn(Node {
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Center,
        row_gap: px(4),
        ..default()
    })
    .with_children(|t| {
        t.spawn(text(fonts, v.title.clone(), Type::HEADING.size(16.0)));
        t.spawn((
            text(fonts, v.subtitle.clone(), Type::MUTED),
            TextLayout::justify(Justify::Center),
        ));
    });
    if !v.rows.is_empty() {
        c.spawn(Node {
            align_self: AlignSelf::Stretch,
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|rows| {
            for r in &v.rows {
                step_row(rows, fonts, r);
            }
        });
    }
    if let Some(f) = &v.failure {
        c.spawn(Node {
            align_self: AlignSelf::Stretch,
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            ..default()
        })
        .with_children(|said| {
            said.spawn(text(fonts, f.said, Type::META));
            said.spawn((
                panel(
                    Node {
                        padding: UiRect::axes(px(12), px(10)),
                        border: UiRect::left(px(2)),
                        border_radius: BorderRadius::all(px(6)),
                        ..default()
                    },
                    Swatch::Bg,
                ),
                BorderColor::default(),
                Stroke(Swatch::Orange),
            ))
            .with_children(|b| {
                b.spawn(text(fonts, f.message.clone(), Type::MONO.ink(Swatch::Fg)));
            });
        });
    }
    c.spawn(Node {
        column_gap: px(8),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|b| {
        match v.cache {
            CacheButton::Hidden => {}
            CacheButton::Available => {
                b.spawn((
                    button(fonts, "Open from cache", Variant::Secondary),
                    OpenFromCache(pr.clone()),
                    observe(on_open_from_cache),
                ));
            }
            CacheButton::Missing => {
                b.spawn(disabled_button(fonts, "Open from cache"));
            }
        }
        if v.failure.is_some() {
            b.spawn((
                button(fonts, "Try again", Variant::Primary),
                TryAgain(pr.clone()),
                observe(on_try_again),
            ));
        }
    });
    if v.cache == CacheButton::Missing {
        c.spawn(text(fonts, "No cached copy yet", Type::META));
    }
}

fn step_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, r: &StepRow) {
    let (dot_fill, dot_ink, icon) = match r.state {
        LeafState::Done => (Swatch::Green, Swatch::OnGreen, "✓"),
        LeafState::Growing => (Swatch::GreenSoft, Swatch::Green, "•"),
        LeafState::Failed => (Swatch::OrangeSoft, Swatch::Orange, "×"),
        LeafState::Skipped => (Swatch::Chrome, Swatch::Faint, "–"),
        LeafState::Waiting => (Swatch::Chrome, Swatch::Faint, ""),
    };
    p.spawn(panel(
        Node {
            column_gap: px(12),
            align_items: AlignItems::Center,
            padding: UiRect::axes(px(10), px(8)),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        if r.current {
            Swatch::GreenSoft
        } else {
            Swatch::Clear
        },
    ))
    .with_children(|row| {
        let mut dot = row.spawn(panel(
            Node {
                width: px(18),
                height: px(18),
                flex_shrink: 0.0,
                border_radius: BorderRadius::MAX,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            dot_fill,
        ));
        if r.state == LeafState::Growing {
            dot.insert(pulsing_dot());
        }
        dot.with_children(|d| {
            d.spawn(text(
                fonts,
                icon,
                Type {
                    size: 11.0,
                    weight: 700,
                    ink: dot_ink,
                    mono: false,
                },
            ));
        });
        row.spawn(Node {
            width: px(64),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|n| {
            n.spawn(text(fonts, r.name, Type::STRONG));
        });
        row.spawn(text(
            fonts,
            r.detail.clone(),
            if r.mono {
                Type::MONO
            } else {
                Type::MUTED.size(12.0)
            },
        ));
    });
}

fn on_open_from_cache(
    activate: On<Activate>,
    buttons: Query<&OpenFromCache>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(OpenFromCache(pr)) = buttons.get(activate.entity) {
        asks.send(Ask::OpenCached(pr.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{ShowRequested, Tell};
    use crate::fixture;
    use crate::review_state::Ready;
    use crate::testing::{self, NOW};
    use crate::ui::leaf::Sprout;
    use clusia_protocol::WindowTarget;

    fn pr() -> PrRef {
        "rzorzal/clusia#123".parse().unwrap()
    }

    fn step(kind: LoadStepKind, status: StepStatus, message: Option<&str>) -> LoadStep {
        LoadStep {
            pr: pr(),
            step: kind,
            status,
            message: message.map(String::from),
        }
    }

    fn cached() -> Option<Box<Ready>> {
        let (view, _) = fixture::demo_review(NOW);
        Some(Box::new(Ready {
            view,
            news: vec![],
            cached_at: Some(NOW - 3600),
        }))
    }

    #[test]
    fn rows_follow_the_mockup() {
        let phase = Phase::Loading {
            steps: vec![
                step(LoadStepKind::Repo, StepStatus::Running, None),
                step(
                    LoadStepKind::Repo,
                    StepStatus::Done,
                    Some("/Users/octo/Repos/clusia"),
                ),
                step(
                    LoadStepKind::Branch,
                    StepStatus::Done,
                    Some(
                        "/Users/octo/Library/Application Support/Clusia/worktrees/rzorzal~clusia~123",
                    ),
                ),
                step(LoadStepKind::Pr, StepStatus::Running, None),
                step(
                    LoadStepKind::Agent,
                    StepStatus::Skipped,
                    Some("arrives with the harness (SP2)"),
                ),
            ],
            cached: cached(),
        };
        let v = loading_view(&phase, &pr(), "feat: auth refresh").unwrap();
        assert_eq!(v.title, "Opening rzorzal/clusia #123");
        assert_eq!(
            v.subtitle,
            "Fresh data from GitHub. You can keep reading the cached version below."
        );
        let rows: Vec<(&str, LeafState, &str, bool, bool)> = v
            .rows
            .iter()
            .map(|r| (r.name, r.state, r.detail.as_str(), r.mono, r.current))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "Repo",
                    LeafState::Done,
                    "Your clone at ~/Repos/clusia",
                    true,
                    false
                ),
                (
                    "Branch",
                    LeafState::Done,
                    "Worktree ready at worktrees/rzorzal~clusia~123",
                    true,
                    false
                ),
                (
                    "PR",
                    LeafState::Growing,
                    "Files, comments and checks…",
                    false,
                    true
                ),
                (
                    "Agent",
                    LeafState::Skipped,
                    "Skipped: no harness set up",
                    false,
                    false
                ),
            ]
        );
        assert_eq!(
            v.leaves,
            [
                LeafState::Done,
                LeafState::Done,
                LeafState::Growing,
                LeafState::Skipped
            ]
        );
        assert_eq!(v.cache, CacheButton::Available);
        assert_eq!(v.backdrop.as_ref().unwrap().title, "feat: auth refresh");
        assert_eq!(v.failure, None);
    }

    #[test]
    fn a_fresh_open_has_no_cache_button() {
        let phase = Phase::Loading {
            steps: vec![],
            cached: None,
        };
        let v = loading_view(&phase, &pr(), "feat: auth refresh").unwrap();
        assert_eq!(v.subtitle, "Getting fresh data from GitHub…");
        assert_eq!(v.cache, CacheButton::Hidden);
        assert_eq!(v.leaves, [LeafState::Waiting; 4]);
        assert_eq!(v.rows[0].detail, "Your clone of rzorzal/clusia");
        assert_eq!(
            v.skeleton,
            ("rzorzal/clusia".into(), "feat: auth refresh".into())
        );
        assert!(v.backdrop.is_none());
    }

    #[test]
    fn failures_are_titled_by_step() {
        let pr98: PrRef = "rzorzal/clusia#98".parse().unwrap();
        let failed = |step, cached| Phase::Failed {
            step,
            message: "fatal: couldn't find remote ref refs/pull/98/head".into(),
            cached,
        };
        let v = loading_view(&failed(Some(LoadStepKind::Branch), None), &pr98, "").unwrap();
        let f = v.failure.as_ref().unwrap();
        assert_eq!(f.heading, "Couldn't get the branch for #98");
        assert_eq!(v.title, f.heading);
        assert_eq!(f.said, "What git said");
        assert_eq!(
            f.message,
            "fatal: couldn't find remote ref refs/pull/98/head"
        );
        assert_eq!(
            v.leaves,
            [
                LeafState::Done,
                LeafState::Failed,
                LeafState::Waiting,
                LeafState::Waiting
            ]
        );
        assert_eq!(v.cache, CacheButton::Missing);
        assert!(v.rows.is_empty());
        let cases = [
            (
                Some(LoadStepKind::Repo),
                "Couldn't reach the repository for #98",
                "What git said",
            ),
            (
                Some(LoadStepKind::Pr),
                "Couldn't load #98 from GitHub",
                "What GitHub said",
            ),
            (None, "Couldn't open #98", "What clusiad said"),
        ];
        for (step, heading, said) in cases {
            let v = loading_view(&failed(step, cached()), &pr98, "").unwrap();
            let f = v.failure.unwrap();
            assert_eq!((f.heading.as_str(), f.said), (heading, said));
            assert_eq!(v.cache, CacheButton::Available);
        }
        let ready = Phase::Ready(cached().unwrap());
        assert_eq!(loading_view(&ready, &pr98, ""), None);
        assert_eq!(tilde("/opt/clusia"), "/opt/clusia");
        assert_eq!(short_worktree("/Users/octo/x"), "~/x");
    }

    fn leaves(app: &mut App) -> Vec<LeafState> {
        let row = testing::find::<Leaves>(app, |_| true);
        app.world()
            .get::<Children>(row)
            .unwrap()
            .iter()
            .map(|c| app.world().get::<Sprout>(c).unwrap().0)
            .collect()
    }

    fn show(app: &mut App) {
        app.world_mut()
            .write_message(ShowRequested(WindowTarget::Review { pr: pr() }));
        testing::settle(app);
        assert_eq!(testing::recorded(app), [Ask::OpenReview(pr())]);
    }

    #[test]
    fn leaves_follow_steps() {
        let mut app = testing::app(fixture::demo(NOW));
        show(&mut app);
        use LeafState::*;
        assert_eq!(leaves(&mut app), [Waiting, Waiting, Waiting, Waiting]);
        let feed = |app: &mut App, s: LoadStep| {
            testing::tell(app, Tell::Step(s));
            testing::settle(app);
        };
        feed(
            &mut app,
            step(LoadStepKind::Repo, StepStatus::Running, None),
        );
        assert_eq!(leaves(&mut app), [Growing, Waiting, Waiting, Waiting]);
        feed(
            &mut app,
            step(
                LoadStepKind::Repo,
                StepStatus::Done,
                Some("/Users/octo/Repos/clusia"),
            ),
        );
        feed(
            &mut app,
            step(
                LoadStepKind::Branch,
                StepStatus::Done,
                Some("/w/worktrees/x"),
            ),
        );
        assert_eq!(leaves(&mut app), [Done, Done, Waiting, Waiting]);
        feed(&mut app, step(LoadStepKind::Pr, StepStatus::Running, None));
        assert_eq!(leaves(&mut app), [Done, Done, Growing, Waiting]);
        feed(
            &mut app,
            step(LoadStepKind::Pr, StepStatus::Failed, Some("HTTP 502")),
        );
        testing::tell(
            &mut app,
            Tell::OpenFailed {
                pr: pr(),
                message: "GitHub refused the request: HTTP 502".into(),
                cache: false,
            },
        );
        testing::settle(&mut app);
        assert_eq!(leaves(&mut app), [Done, Done, Failed, Waiting]);
        let has = |app: &mut App, needle: &str| {
            let mut q = app.world_mut().query::<&Text>();
            q.iter(app.world()).any(|t| t.0 == needle)
        };
        assert!(has(&mut app, "Couldn't load #123 from GitHub"));
        assert!(has(&mut app, "What GitHub said"));
        assert!(has(&mut app, "No cached copy yet"));
        assert_eq!(
            testing::count::<OpenFromCache>(&mut app),
            0,
            "disabled without a cache"
        );
    }

    /// The status dot of the row named `name`.
    fn dot(app: &mut App, name: &str) -> Entity {
        let label = {
            let mut q = app.world_mut().query::<(Entity, &Text)>();
            q.iter(app.world())
                .find(|(_, t)| t.0 == name)
                .map(|(e, _)| e)
                .expect("row label")
        };
        let cell = app.world().get::<ChildOf>(label).unwrap().parent();
        let row = app.world().get::<ChildOf>(cell).unwrap().parent();
        app.world().get::<Children>(row).unwrap()[0]
    }

    #[test]
    fn the_running_step_sways_until_it_is_done() {
        use crate::ui::kit::Fill;
        use crate::ui::leaf::{Blink, Pulse, Sway};
        use bevy::time::TimeUpdateStrategy;
        use bevy::window::RequestRedraw;
        use std::time::Duration;
        let mut app = testing::app(fixture::demo(NOW));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            150,
        )));
        show(&mut app);
        let redraws = |app: &mut App| {
            app.world_mut()
                .resource_mut::<Messages<RequestRedraw>>()
                .drain()
                .count()
        };
        let moving = |app: &mut App| {
            let mut q = app.world_mut().query_filtered::<(Entity, &UiTransform), Or<(
                With<Sway>,
                With<Pulse>,
                With<Blink>,
            )>>();
            q.iter(app.world())
                .map(|(e, t)| (e, *t))
                .collect::<Vec<_>>()
        };
        testing::tell(
            &mut app,
            Tell::Step(step(LoadStepKind::Repo, StepStatus::Running, None)),
        );
        testing::settle(&mut app);
        redraws(&mut app);
        let repo = dot(&mut app, "Repo");
        assert!(
            app.world().get::<Blink>(repo).is_some(),
            "the Repo row's dot"
        );
        let row = testing::find::<Leaves>(&mut app, |_| true);
        let sprout = app.world().get::<Children>(row).unwrap()[0];
        assert!(app.world().get::<Sway>(sprout).is_some(), "the Repo leaf");
        let blade = app.world().get::<Children>(sprout).unwrap()[0];
        let mut last = moving(&mut app);
        assert_eq!(last.len(), 3, "the sprout, its leaf and the status dot");
        let mut fills = Vec::new();
        for _ in 0..8 {
            app.update();
            assert!(redraws(&mut app) > 0, "a redraw every frame while running");
            let now = moving(&mut app);
            assert!(now.iter().zip(&last).all(|(a, b)| a != b), "all move");
            let scale = |e: Entity| app.world().get::<UiTransform>(e).unwrap().scale;
            assert_eq!(scale(repo), scale(blade), "the dot pulses with the leaf");
            fills.push(app.world().get::<Fill>(repo).unwrap().0);
            last = now;
        }
        assert!(fills.contains(&Swatch::Green) && fills.contains(&Swatch::GreenSoft));
        testing::tell(
            &mut app,
            Tell::Step(step(
                LoadStepKind::Repo,
                StepStatus::Done,
                Some("/Users/octo/Repos/clusia"),
            )),
        );
        testing::settle(&mut app);
        redraws(&mut app);
        app.update();
        assert_eq!(redraws(&mut app), 0, "idle once no step runs");
        assert!(moving(&mut app).is_empty());
        for name in ["Repo", "Branch", "PR", "Agent"] {
            let d = dot(&mut app, name);
            assert_eq!(
                *app.world().get::<UiTransform>(d).unwrap(),
                UiTransform::IDENTITY
            );
        }
        // The card was rebuilt for the new state.
        let row = testing::find::<Leaves>(&mut app, |_| true);
        let leaves = app.world().get::<Children>(row).unwrap().to_vec();
        for l in leaves {
            assert_eq!(
                *app.world().get::<UiTransform>(l).unwrap(),
                UiTransform::IDENTITY
            );
            for part in app.world().get::<Children>(l).unwrap().iter() {
                if let Some(t) = app.world().get::<UiTransform>(part) {
                    assert!(
                        *t == UiTransform::IDENTITY
                            || *t == UiTransform::from_rotation(Rot2::degrees(45.0)),
                        "settled: {t:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn open_from_cache_and_try_again() {
        let mut app = testing::app(fixture::demo(NOW));
        show(&mut app);
        let (view, _) = fixture::demo_review(NOW);
        testing::tell(
            &mut app,
            Tell::CachedAvailable {
                pr: pr(),
                view: Box::new(view.clone()),
                fetched_at: NOW - 3600,
            },
        );
        testing::settle(&mut app);
        let cache = testing::find::<OpenFromCache>(&mut app, |_| true);
        testing::activate(&mut app, cache);
        assert_eq!(testing::recorded(&mut app), [Ask::OpenCached(pr())]);
        testing::tell(
            &mut app,
            Tell::OpenFailed {
                pr: pr(),
                message: "fatal: couldn't find remote ref refs/pull/123/head".into(),
                cache: true,
            },
        );
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<OpenFromCache>(&mut app),
            1,
            "the cache still opens"
        );
        let again = testing::find::<TryAgain>(&mut app, |_| true);
        testing::activate(&mut app, again);
        assert_eq!(testing::recorded(&mut app), [Ask::OpenReview(pr())]);
        testing::settle(&mut app);
        assert!(matches!(
            app.world().resource::<ReviewTabs>().0[&pr()].phase,
            Phase::Loading {
                cached: Some(_),
                ..
            }
        ));
        assert_eq!(leaves(&mut app), [LeafState::Waiting; 4]);
    }
}
