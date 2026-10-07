//! Brings a composer into view in the scroll area around it when it opens or takes the
//! keyboard, so its text area, preview and the caller's footer are all visible.
//!
//! What is revealed is the composer's frame — its parent, which every caller draws around it
//! together with its buttons. Only an editor that opens is revealed on its own, once per opening:
//! the Finalize composers are always on screen and would otherwise pull the page to the last one.
//! Any composer is revealed when its text area gets the keyboard. The scroll moves only as far as
//! needed, so a frame already in view stays where it is. For a short while after, a frame that
//! grows (a preview drawn, a picture loaded) is kept in view the same way.

use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::ui::{ComputedNode, OverflowAxis, UiGlobalTransform};

use crate::ui::composer::{Composer, ComposerArea, ComposerKey, Slot};

/// Space kept between a revealed frame and the edges of its scroll area.
const MARGIN: f32 = 12.0;

/// How long after a reveal a growing frame is kept in view, in seconds.
const FOLLOW_SECS: f64 = 2.0;

/// The scroll offset that shows an item whose top and bottom are `top` and `bottom` pixels
/// below the top of a `view`-tall scroll area currently scrolled to `offset`. An item taller than
/// the view shows its top.
pub(crate) fn reveal_offset(offset: f32, view: f32, top: f32, bottom: f32) -> f32 {
    let to_top = offset + top - MARGIN;
    let to_bottom = offset + bottom - view + MARGIN;
    let want = if top < MARGIN || bottom - top > view - 2.0 * MARGIN {
        to_top
    } else if bottom > view - MARGIN {
        to_bottom
    } else {
        offset
    };
    want.max(0.0)
}

/// The composer waiting to be revealed, and the editor revealed last (so a rebuild of the region
/// around an editor that stays open does not pull the view back to it).
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub(crate) struct Reveal {
    pub pending: Option<ComposerKey>,
    shown: Option<ComposerKey>,
    /// The frame revealed last, its height then and until when its growth is followed.
    follow: Option<(ComposerKey, f32, f64)>,
}

pub(crate) struct RevealPlugin;

impl Plugin for RevealPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Reveal>()
            .add_systems(Update, queue_reveal)
            .add_systems(PostUpdate, reveal.after(bevy::ui::UiSystems::PostLayout));
    }
}

fn queue_reveal(
    mut reveal: ResMut<Reveal>,
    focus: Res<InputFocus>,
    mut focused: Local<Option<Entity>>,
    added: Query<&Composer, Added<Composer>>,
    composers: Query<&Composer>,
    areas: Query<&ComposerArea>,
) {
    if let Some(shown) = reveal.shown.clone()
        && !composers.iter().any(|c| c.0 == shown)
    {
        reveal.shown = None;
    }
    for Composer(key) in &added {
        if matches!(key.1, Slot::Edit(_)) && reveal.shown.as_ref() != Some(key) {
            reveal.pending = Some(key.clone());
        }
    }
    let now = focus.get();
    if now != *focused {
        *focused = now;
        if let Some(ComposerArea(key)) = now.and_then(|e| areas.get(e).ok()) {
            reveal.pending = Some(key.clone());
        }
    }
}

/// The node's top and bottom on screen, in logical pixels.
fn span(node: &ComputedNode, at: &UiGlobalTransform) -> (f32, f32) {
    let scale = node.inverse_scale_factor;
    let center = at.translation.y * scale;
    let half = node.size.y * scale / 2.0;
    (center - half, center + half)
}

/// Scrolls the nearest vertical scroll area around the pending composer's frame — or around
/// the frame revealed last when it has grown since. It waits while the frame has not been laid
/// out yet.
fn reveal(
    mut reveal: ResMut<Reveal>,
    time: Res<Time>,
    composers: Query<(Entity, &Composer)>,
    parents: Query<&ChildOf>,
    nodes: Query<(&Node, &ComputedNode, &UiGlobalTransform)>,
    mut scrolls: Query<&mut ScrollPosition>,
) {
    let now = time.elapsed_secs_f64();
    let grown = reveal.follow.clone().filter(|(_, _, until)| now < *until);
    let Some(key) = reveal
        .pending
        .clone()
        .or_else(|| grown.clone().map(|f| f.0))
    else {
        return;
    };
    let Some((composer, _)) = composers.iter().find(|(_, c)| c.0 == key) else {
        return;
    };
    let frame = parents.get(composer).map_or(composer, ChildOf::parent);
    let Ok((_, frame_node, frame_at)) = nodes.get(frame) else {
        return;
    };
    let height = frame_node.size.y * frame_node.inverse_scale_factor;
    if height <= 0.0 {
        return;
    }
    let pending = reveal.pending.is_some();
    if !pending && grown.is_some_and(|(_, was, _)| (height - was).abs() < 0.5) {
        return;
    }
    let (top, bottom) = span(frame_node, frame_at);
    let view = parents.iter_ancestors(frame).find(|e| {
        nodes
            .get(*e)
            .is_ok_and(|(n, _, _)| n.overflow.y == OverflowAxis::Scroll)
    });
    if let Some(view) = view
        && let Ok((_, view_node, view_at)) = nodes.get(view)
        && let Ok(mut scroll) = scrolls.get_mut(view)
    {
        let (view_top, view_bottom) = span(view_node, view_at);
        let want = reveal_offset(
            scroll.y,
            view_bottom - view_top,
            top - view_top,
            bottom - view_top,
        );
        if (want - scroll.y).abs() > 0.5 {
            scroll.y = want;
        }
    }
    let until = if pending {
        now + FOLLOW_SECS
    } else {
        reveal.follow.as_ref().map_or(now, |f| f.2)
    };
    reveal.follow = Some((key.clone(), height, until));
    reveal.pending = None;
    if matches!(key.1, Slot::Edit(_)) {
        reveal.shown = Some(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::review_state::{EditTarget, Modal, ReviewTabs};
    use crate::testing::{self, NOW};
    use crate::ui::composer::testkit::open;
    use bevy::input_focus::FocusCause;

    fn pending(app: &App) -> Option<Slot> {
        app.world()
            .resource::<Reveal>()
            .pending
            .clone()
            .map(|k| k.1)
    }

    #[test]
    fn an_opening_editor_and_a_focused_composer_are_revealed() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, area) = open(&mut app, EditTarget::General, "");
        assert_eq!(pending(&app), Some(Slot::Edit(EditTarget::General)));
        app.world_mut().resource_mut::<Reveal>().pending = None;
        // The always-present Finalize composers are not revealed just for being drawn.
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .modal = Some(Modal::Finalize);
        testing::settle(&mut app);
        assert!(testing::count::<Composer>(&mut app) > 1);
        assert_eq!(pending(&app), None);
        let summary = testing::find::<ComposerArea>(&mut app, |a| a.0.1 == Slot::FinalizeSummary);
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(summary, FocusCause::Navigated);
        app.update();
        assert_eq!(pending(&app), Some(Slot::FinalizeSummary));
        app.world_mut().resource_mut::<Reveal>().pending = None;
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(area, FocusCause::Navigated);
        app.update();
        assert_eq!(pending(&app), Some(Slot::Edit(EditTarget::General)));
    }

    #[test]
    fn an_item_in_view_does_not_scroll() {
        assert_eq!(reveal_offset(100.0, 600.0, 40.0, 300.0), 100.0);
    }

    #[test]
    fn an_item_below_the_view_scrolls_up_just_enough() {
        // Bottom at 700 in a 600 view: scroll by 100 plus the margin.
        assert_eq!(reveal_offset(0.0, 600.0, 450.0, 700.0), 100.0 + MARGIN);
    }

    #[test]
    fn an_item_above_the_view_scrolls_down_to_its_top() {
        assert_eq!(reveal_offset(500.0, 600.0, -80.0, 120.0), 420.0 - MARGIN);
        assert_eq!(
            reveal_offset(20.0, 600.0, -10.0, 120.0),
            0.0,
            "never before the start"
        );
    }

    #[test]
    fn an_item_taller_than_the_view_shows_its_top() {
        assert_eq!(reveal_offset(0.0, 300.0, 200.0, 800.0), 200.0 - MARGIN);
    }
}
