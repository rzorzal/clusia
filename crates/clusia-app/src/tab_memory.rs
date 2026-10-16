//! The review tabs outlive the window: the daemon keeps them (`OpenTabs`), they come back when
//! the window opens again, and every change is sent back. Only a tab's × closes a review.

use std::collections::HashMap;

use bevy::prelude::*;
use clusia_core::{OpenTabs, PrRef};

use crate::bridge::{Ask, Asks, Connection, Model};
use crate::nav::{Nav, NavSystems, Screen};
use crate::review_state::{ReviewSection, ReviewTabs, open_new_tabs};

/// The daemon's answer to `RestoreTabs`, once per window.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct TabsRestored(pub OpenTabs);

#[derive(Resource, Debug, Default)]
pub struct TabMemory {
    /// The saved tabs came back. Nothing is sent before: it would overwrite them.
    restored: bool,
    /// What the daemon last heard; `None` after a lost connection, so it hears it again.
    sent: Option<OpenTabs>,
    /// The sections of restored tabs whose `Tab` does not exist yet (`open_new_tabs` takes them).
    pub sections: HashMap<PrRef, ReviewSection>,
}

pub struct TabMemoryPlugin;

impl Plugin for TabMemoryPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TabMemory>()
            .add_message::<TabsRestored>()
            .add_systems(Update, restore.before(NavSystems))
            .add_systems(Update, remember.after(open_new_tabs));
    }
}

fn restore(
    mut restored: MessageReader<TabsRestored>,
    mut nav: ResMut<Nav>,
    mut memory: ResMut<TabMemory>,
) {
    for TabsRestored(saved) in restored.read() {
        if memory.restored {
            continue;
        }
        memory.restored = true;
        nav.restore(saved);
        memory.sections = saved
            .tabs
            .iter()
            .filter_map(|t| Some((t.pr.clone(), ReviewSection::from_id(&t.section)?)))
            .collect();
    }
}

/// The window's tabs as the daemon keeps them.
fn current(nav: &Nav, tabs: &ReviewTabs, memory: &TabMemory) -> OpenTabs {
    OpenTabs {
        tabs: nav
            .reviews
            .iter()
            .map(|pr| {
                let section = tabs
                    .0
                    .get(pr)
                    .map(|t| t.ui.section)
                    .or_else(|| memory.sections.get(pr).copied())
                    .unwrap_or_default();
                OpenTabs::tab(pr.clone(), section.id())
            })
            .collect(),
        selected: match &nav.screen {
            Screen::Review(pr) => Some(pr.clone()),
            _ => None,
        },
    }
}

fn remember(
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut memory: ResMut<TabMemory>,
    mut asks: ResMut<Asks>,
) {
    if !memory.restored {
        return;
    }
    if model.connection != Connection::Live {
        memory.sent = None;
        return;
    }
    let now = current(&nav, &tabs, &memory);
    if memory.sent.as_ref() != Some(&now) {
        asks.send(Ask::SetOpenTabs(now.clone()));
        memory.sent = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use clusia_protocol::WindowTarget;

    use super::*;
    use crate::bridge::Tell;
    use crate::fixture;
    use crate::testing::{self, NOW};

    fn pr(s: &str) -> PrRef {
        s.parse().unwrap()
    }

    /// Three tabs, the second one shown, each in its own section.
    fn saved() -> OpenTabs {
        OpenTabs {
            tabs: vec![
                OpenTabs::tab(fixture::demo_pr(), "comments"),
                OpenTabs::tab(pr("rzorzal/clusia#98"), "security"),
                OpenTabs::tab(pr("acme/widgets#5"), "diff"),
            ],
            selected: Some(pr("rzorzal/clusia#98")),
        }
    }

    fn sent(asks: &[Ask]) -> Vec<OpenTabs> {
        asks.iter()
            .filter_map(|a| match a {
                Ask::SetOpenTabs(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn sections(app: &App) -> Vec<ReviewSection> {
        let nav = app.world().resource::<Nav>();
        nav.reviews
            .iter()
            .map(|pr| testing::tab(app, pr).ui.section)
            .collect()
    }

    #[test]
    fn the_saved_tabs_come_back_with_the_one_shown_and_their_sections() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::tell(&mut app, Tell::Restored(saved()));
        testing::settle(&mut app);
        let nav = app.world().resource::<Nav>().clone();
        let prs: Vec<PrRef> = saved().tabs.into_iter().map(|t| t.pr).collect();
        assert_eq!(nav.reviews, prs, "the same tabs, in the same order");
        assert_eq!(nav.screen, Screen::Review(pr("rzorzal/clusia#98")));
        assert_eq!(
            sections(&app),
            [
                ReviewSection::Comments,
                ReviewSection::Security,
                ReviewSection::Diff
            ]
        );
        let asks = testing::recorded(&mut app);
        let opens: Vec<&Ask> = asks
            .iter()
            .filter(|a| matches!(a, Ask::OpenReview(_)))
            .collect();
        assert_eq!(opens.len(), 3, "each opens (the cache first): {asks:?}");
        assert_eq!(
            sent(&asks),
            [saved()],
            "and the daemon hears them back once"
        );
    }

    #[test]
    fn nothing_is_sent_before_the_saved_tabs_are_back() {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review {
                pr: pr("acme/widgets#5"),
            });
        testing::settle(&mut app);
        assert!(
            sent(&testing::recorded(&mut app)).is_empty(),
            "would overwrite them"
        );
        testing::tell(&mut app, Tell::Restored(saved()));
        testing::settle(&mut app);
        let nav = app.world().resource::<Nav>();
        assert_eq!(nav.reviews.len(), 3, "the open one was already among them");
        assert_eq!(
            nav.screen,
            Screen::Review(pr("acme/widgets#5")),
            "the tab asked for stays shown"
        );
        let asks = testing::recorded(&mut app);
        assert_eq!(sent(&asks).len(), 1);
        assert_eq!(sent(&asks)[0].selected, Some(pr("acme/widgets#5")));
        // A second answer (after a reconnect) changes nothing.
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Home);
        testing::tell(&mut app, Tell::Restored(OpenTabs::default()));
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<Nav>().reviews.len(), 3);
        assert_eq!(app.world().resource::<Nav>().screen, Screen::Home);
    }

    #[test]
    fn after_a_refused_restore_the_window_still_saves_its_tabs() {
        let mut app = testing::app(fixture::demo(NOW));
        // What the bridge tells when the daemon refused or failed `RestoreTabs`.
        testing::tell(&mut app, Tell::Restored(OpenTabs::default()));
        testing::settle(&mut app);
        testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review {
                pr: pr("acme/widgets#5"),
            });
        testing::settle(&mut app);
        let now = sent(&testing::recorded(&mut app));
        assert_eq!(now.len(), 1, "its first change is saved");
        assert_eq!(now[0].selected, Some(pr("acme/widgets#5")));
    }

    #[test]
    fn every_change_is_sent_once() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::tell(&mut app, Tell::Restored(saved()));
        testing::settle(&mut app);
        testing::recorded(&mut app);
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "no change, nothing sent"
        );
        let demo = fixture::demo_pr();
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&demo)
            .unwrap()
            .ui
            .section = ReviewSection::Tests;
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: demo.clone() });
        testing::settle(&mut app);
        let now = sent(&testing::recorded(&mut app));
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].tabs[0].section, "tests");
        assert_eq!(now[0].selected, Some(demo));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        testing::settle(&mut app);
        assert_eq!(sent(&testing::recorded(&mut app))[0].selected, None);
    }

    #[test]
    fn a_lost_connection_sends_the_tabs_again_once_it_is_back() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::tell(&mut app, Tell::Restored(saved()));
        testing::settle(&mut app);
        testing::recorded(&mut app);
        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        app.world_mut()
            .resource_mut::<Nav>()
            .close_review(&pr("acme/widgets#5"));
        testing::settle(&mut app);
        assert!(
            sent(&testing::recorded(&mut app)).is_empty(),
            "not while lost"
        );
        app.world_mut().resource_mut::<Model>().connection = Connection::Live;
        testing::settle(&mut app);
        let now = sent(&testing::recorded(&mut app));
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].tabs.len(), 2);
    }
}
