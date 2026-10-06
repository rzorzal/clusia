//! `--demo --scene <name>`: stages the demo review in one state, so every review screen can be
//! rendered (screenshots) without a daemon.

use bevy::prelude::*;
use clusia_core::Verdict;
use clusia_core::draft::{DraftKind, ThreadRef};
use clusia_protocol::{LoadStep, LoadStepKind, StepStatus, WindowTarget};

use crate::args::Scene;
use crate::clock::Clock;
use crate::fixture;
use crate::nav::Nav;
use crate::review_state::{DiffMode, Modal, Phase, Ready, ReviewSection, ReviewTabs, Tab, TabUi};
use crate::screens::open_pr::Palette;

/// Stages `scene` once at startup.
pub struct ScenePlugin(pub Scene);

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        let scene = self.0;
        app.add_systems(Startup, move |world: &mut World| stage(world, scene));
    }
}

fn steps(pr: &clusia_core::PrRef) -> Vec<LoadStep> {
    let step = |step, status, message: Option<&str>| LoadStep {
        pr: pr.clone(),
        step,
        status,
        message: message.map(String::from),
    };
    vec![
        step(LoadStepKind::Repo, StepStatus::Done, Some("~/Repos/clusia")),
        step(
            LoadStepKind::Branch,
            StepStatus::Done,
            Some("~/Library/Application Support/Clusia/worktrees/rzorzal~clusia~123"),
        ),
        step(LoadStepKind::Pr, StepStatus::Running, None),
    ]
}

/// Puts the demo review `rzorzal/clusia#123` in the state `scene` shows.
pub fn stage(world: &mut World, scene: Scene) {
    let now = world.resource::<Clock>().now();
    let (view, news) = fixture::demo_review(now);
    let pr = view.review.pr.clone();
    let mut ready = Ready {
        view,
        news,
        cached_at: None,
    };
    if scene != Scene::WhatsNew {
        // Only the What's new scene shows the What's new modal (it opens on a fresh review
        // with news); the other scenes clear the news.
        ready.news.clear();
    }
    let mut ui = TabUi::default();
    match scene {
        Scene::Diff | Scene::Loading | Scene::Failed | Scene::Palette => {}
        Scene::Split => ui.mode = DiffMode::Split,
        Scene::Comments => {
            ui.section = ReviewSection::Comments;
            let thread = ready.view.conversation.as_ref().and_then(|c| {
                c.review_threads
                    .iter()
                    .find(|t| !t.is_resolved && !t.comments.is_empty())
                    .map(|t| ThreadRef {
                        id: t.id.clone(),
                        author: t.comments[0].author.clone(),
                        path: Some(t.path.clone()),
                        line: t.line,
                    })
            });
            if let Some(thread) = thread {
                let _ = ready.view.review.draft.add(
                    DraftKind::Reply,
                    None,
                    Some(thread),
                    "Agree with 30 seconds; the skew we saw on CI was under 10.",
                    now,
                );
            }
        }
        Scene::Finalize => {
            ui.modal = Some(Modal::Finalize);
            ui.finalize.verdict = Some(Verdict::RequestChanges);
            ui.finalize.summary =
                "Nice change overall. Two things on the token store before this goes in.".into();
        }
        Scene::WhatsNew => ui.modal = Some(Modal::WhatsNew),
        Scene::Leave => ui.modal = Some(Modal::Leave { window: false }),
    }
    if matches!(scene, Scene::Loading | Scene::Failed) {
        // The copy under a load is the cached one: read-only, nothing is sent from it.
        ready.cached_at = Some(now - 3600);
    }
    let phase = match scene {
        Scene::Loading => Phase::Loading {
            steps: steps(&pr),
            cached: Some(Box::new(ready)),
        },
        Scene::Failed => Phase::Failed {
            step: Some(LoadStepKind::Branch),
            message: "fatal: couldn't find remote ref refs/pull/123/head".into(),
            cached: Some(Box::new(ready)),
        },
        _ => Phase::Ready(Box::new(ready)),
    };
    world
        .resource_mut::<ReviewTabs>()
        .0
        .insert(pr.clone(), Tab { phase, ui });
    {
        let mut nav = world.resource_mut::<Nav>();
        nav.go(&WindowTarget::Review { pr });
        if scene == Scene::Palette {
            nav.go(&WindowTarget::Home);
        }
    }
    if scene == Scene::Palette {
        *world.resource_mut::<Palette>() = Palette {
            open: true,
            query: "site".into(),
            cursor: 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::Screen;
    use crate::screens::open_pr::PaletteRoot;
    use crate::screens::review::comments::CommentsRegion;
    use crate::screens::review::diff::DiffRegion;
    use crate::screens::review::finalize::FinalizeModal;
    use crate::screens::review::leave::LeaveModal;
    use crate::screens::review::shell::ModalFor;
    use crate::testing::{self, NOW};
    use clap::ValueEnum;

    fn staged(scene: Scene) -> App {
        let mut app = testing::app(fixture::demo(NOW));
        stage(app.world_mut(), scene);
        testing::settle(&mut app);
        app
    }

    fn modal_shown(app: &mut App, modal: Modal) -> bool {
        let mut q = app.world_mut().query::<&ModalFor>();
        q.iter(app.world()).any(|m| m.modal == modal)
    }

    #[test]
    fn every_scene_stages_the_demo_review() {
        let pr = fixture::demo_pr();
        for &scene in Scene::value_variants() {
            let mut app = staged(scene);
            let tab = testing::tab(&app, &pr);
            let screen = app.world().resource::<Nav>().screen.clone();
            match scene {
                Scene::Palette => {
                    assert_eq!(screen, Screen::Home);
                    assert_eq!(testing::count::<PaletteRoot>(&mut app), 1);
                }
                _ => assert_eq!(screen, Screen::Review(pr.clone()), "{scene:?}"),
            }
            match scene {
                Scene::Diff | Scene::Palette => {
                    assert_eq!(tab.ui.mode, DiffMode::Unified);
                    if scene == Scene::Diff {
                        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
                    }
                }
                Scene::Split => {
                    assert_eq!(tab.ui.mode, DiffMode::Split);
                    assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
                }
                Scene::Comments => {
                    assert_eq!(testing::count::<CommentsRegion>(&mut app), 1);
                    let Phase::Ready(r) = &tab.phase else {
                        panic!("ready")
                    };
                    assert!(
                        r.view
                            .review
                            .draft
                            .items
                            .iter()
                            .any(|i| i.kind == DraftKind::Reply),
                        "the mockup's draft reply"
                    );
                }
                Scene::Finalize => {
                    assert_eq!(testing::count::<FinalizeModal>(&mut app), 1);
                    assert_eq!(tab.ui.finalize.verdict, Some(Verdict::RequestChanges));
                }
                Scene::WhatsNew => assert!(modal_shown(&mut app, Modal::WhatsNew)),
                Scene::Leave => assert_eq!(testing::count::<LeaveModal>(&mut app), 1),
                Scene::Loading => {
                    let Phase::Loading { steps, cached } = &tab.phase else {
                        panic!("loading")
                    };
                    assert_eq!(steps.len(), 3);
                    assert!(cached.as_ref().is_some_and(|c| c.cached_at.is_some()));
                }
                Scene::Failed => {
                    let Phase::Failed {
                        step,
                        message,
                        cached,
                    } = &tab.phase
                    else {
                        panic!("failed")
                    };
                    assert!(cached.as_ref().is_some_and(|c| c.cached_at.is_some()));
                    assert_eq!(*step, Some(LoadStepKind::Branch));
                    assert!(message.starts_with("fatal: couldn't find remote ref"));
                }
            }
        }
    }
}
