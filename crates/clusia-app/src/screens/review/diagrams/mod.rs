//! The Diagrams tab: the list of the
//! version's diagrams, the footer with the version stepper, and the canvas the selected diagram
//! is drawn on. `model` holds what the daemon said, `list` the view and its buttons; the canvas
//! fills `DiagramCanvas`.

use bevy::prelude::*;
use clusia_core::PrRef;

use crate::bridge::{Ask, Asks, Connection, Model};
use crate::review_state::ReviewTabs;
use crate::screens::review::ReviewSystems;

pub mod canvas;
pub mod keywords;
pub mod list;
pub mod modal;
pub mod model;
pub mod view;

pub use model::{Diagrams, DiagramsModel};

/// The area the selected diagram is drawn in: an empty node until the canvas fills it. It is
/// spawned while the selected entry is ready and gone while it is not, so a canvas never
/// outlives the diagram it shows.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiagramCanvas(pub PrRef);

pub struct DiagramsPlugin;

impl Plugin for DiagramsPlugin {
    fn build(&self, app: &mut App) {
        list::add_systems(app);
        canvas::add_systems(app);
        app.add_plugins(keywords::KeywordsPlugin);
        app.add_systems(
            Update,
            (
                load_diagrams,
                list::fill_diagrams,
                list::sync_menu,
                canvas::fill_canvas,
                keywords::fill_keywords,
            )
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Asks for the diagrams of each ready tab once the connection is live, forgets the diagrams of
/// closed tabs, and asks for an older version when the tab steps to one that has not arrived.
/// A cached copy asks when the fresh one arrives.
fn load_diagrams(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut diagrams: ResMut<Diagrams>,
    mut asks: ResMut<Asks>,
) {
    if diagrams.0.keys().any(|pr| !tabs.0.contains_key(pr)) {
        diagrams.0.retain(|pr, _| tabs.0.contains_key(pr));
    }
    if model.connection != Connection::Live {
        return;
    }
    for (pr, tab) in &tabs.0 {
        if !tab.ready().is_some_and(|r| r.cached_at.is_none()) {
            continue;
        }
        match diagrams.0.get(pr) {
            Some(m) if m.asked => {
                let Some(number) = tab.ui.diagram_version else {
                    continue;
                };
                let wanted = m.received
                    && !m.failed.contains(&number)
                    && !m.loaded.contains_key(&number)
                    && !m.requested.contains(&number)
                    && m.versions.iter().any(|v| v.number == number);
                if wanted && let Some(m) = diagrams.0.get_mut(pr) {
                    m.requested.insert(number);
                    asks.send(Ask::GetDiagramVersion {
                        pr: pr.clone(),
                        number,
                    });
                }
            }
            _ => {
                diagrams.0.entry(pr.clone()).or_default().asked = true;
                asks.send(Ask::GetDiagrams(pr.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Connection, Tell};
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use clusia_protocol::DiagramsState;

    fn set_version(app: &mut App, pr: &PrRef, number: u32) {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        tabs.0.get_mut(pr).unwrap().ui.diagram_version = Some(number);
    }

    #[test]
    fn a_ready_tab_asks_for_its_diagrams_once() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        assert!(
            app.world().resource::<Diagrams>().0[&pr].asked,
            "asked while opening"
        );
        testing::settle(&mut app);
        let again = testing::recorded(&mut app);
        assert!(!again.contains(&Ask::GetDiagrams(pr.clone())), "only once");
        testing::tell(&mut app, Tell::Lost("gone".into()));
        assert!(!app.world().resource::<Diagrams>().0[&pr].asked);
    }

    #[test]
    fn the_diagrams_are_asked_again_after_a_reconnect() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::recorded(&mut app);
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "nothing is asked while the connection is gone"
        );
        app.world_mut().resource_mut::<Model>().connection = Connection::Live;
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).contains(&Ask::GetDiagrams(pr)));
    }

    #[test]
    fn a_cached_copy_asks_when_the_fresh_one_arrives() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::recorded(&mut app);
        app.world_mut().resource_mut::<Model>().connection = Connection::Live;
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .and_then(|t| t.ready_mut())
            .expect("a ready tab")
            .cached_at = Some(NOW - 3600);
        testing::settle(&mut app);
        assert!(!testing::recorded(&mut app).contains(&Ask::GetDiagrams(pr)));
    }

    #[test]
    fn a_closed_tab_forgets_its_diagrams() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::open_diagrams(&mut app, &pr);
        assert!(app.world().resource::<Diagrams>().0.contains_key(&pr));
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .close_review(&pr);
        testing::settle(&mut app);
        assert!(app.world().resource::<Diagrams>().0.is_empty());
    }

    #[test]
    fn an_older_version_is_asked_for_only_when_the_daemon_lists_it() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::open_diagrams(&mut app, &pr);
        testing::recorded(&mut app);
        set_version(&mut app, &pr, 9);
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "version 9 is not listed"
        );
        set_version(&mut app, &pr, 1);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetDiagramVersion { pr, number: 1 }]
        );
    }

    #[test]
    fn the_status_bar_says_which_diagram_is_being_drawn() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::open_diagrams(&mut app, &pr);
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        assert!(!testing::shows(&mut app, "drawing diagrams"));
        testing::tell(
            &mut app,
            Tell::Diagrams(crate::bridge::DiagramsTell::State {
                pr: pr.clone(),
                state: DiagramsState::Drawing {
                    done: 1,
                    total: 5,
                    key: "deps".into(),
                    step: None,
                },
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "Claude Code · drawing diagrams 2 of 5"
        ));
        testing::tell(
            &mut app,
            Tell::Diagrams(crate::bridge::DiagramsTell::State {
                pr,
                state: DiagramsState::Idle,
            }),
        );
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "drawing diagrams"));
    }
}
