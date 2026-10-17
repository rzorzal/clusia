//! Config opened at one setting: the Harness page scrolls to the row and tints it for a moment,
//! so the setting a button pointed at is the one the eye lands on.

use bevy::prelude::*;
use bevy::ui::{ComputedNode, OverflowAxis, UiGlobalTransform};

use super::ConfigField;
use super::harness::OpenToggle;
use crate::nav::{Nav, Screen, Section};
use crate::theme::Swatch;
use crate::ui::composer::reveal::{reveal_offset, span};
use crate::ui::kit::Fill;

/// How long the row stays tinted, in seconds.
pub const SPOTLIGHT_SECS: f64 = 2.0;

/// The request being shown: the key asked for, which ask it was, when its row was first seen on the page (on the
/// `Time` clock), and whether the page has been scrolled to it.
#[derive(Resource, Debug, Default, PartialEq)]
pub struct Spotlight {
    pub key: Option<&'static str>,
    /// `Nav::requests` when the request was taken: asking again restarts the tint.
    pub request: u32,
    pub started: Option<f64>,
    pub scrolled: bool,
}

/// What a tinted row looked like before, to put back when the tint ends.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Spotlit {
    padding: UiRect,
    margin: UiRect,
    border_radius: BorderRadius,
}

/// The padding that gives the tint room around a row, undone by an equal negative margin so the
/// page does not move. A row with padding or a margin of its own is tinted as it is.
const BREATHING: f32 = 6.0;

fn tint(commands: &mut Commands, row: Entity, node: &Node) {
    commands.entity(row).insert((
        Spotlit {
            padding: node.padding,
            margin: node.margin,
            border_radius: node.border_radius,
        },
        BackgroundColor::default(),
        Fill(Swatch::GreenSoft),
    ));
    let bare = node.padding == UiRect::DEFAULT && node.margin == UiRect::DEFAULT;
    commands
        .entity(row)
        .entry::<Node>()
        .and_modify(move |mut n| {
            if bare {
                n.padding = UiRect::all(px(BREATHING));
                n.margin = UiRect::all(px(-BREATHING));
            }
            n.border_radius = BorderRadius::all(px(6));
        });
}

fn untint(commands: &mut Commands, row: Entity, was: &Spotlit) {
    let was = was.clone();
    commands
        .entity(row)
        .entry::<Node>()
        .and_modify(move |mut n| {
            n.padding = was.padding;
            n.margin = was.margin;
            n.border_radius = was.border_radius;
        });
    commands
        .entity(row)
        .remove::<Spotlit>()
        .insert(Fill(Swatch::Clear));
}

/// Tints the row of the setting Config was opened at and scrolls it into view, then lets go
/// after `SPOTLIGHT_SECS`. The tint is put back each frame, since a rebuilt page has new rows.
/// A request left over from another screen is dropped.
pub fn spotlight(
    mut nav: ResMut<Nav>,
    time: Res<Time>,
    mut state: ResMut<Spotlight>,
    mut commands: Commands,
    fields: Query<(Entity, &ConfigField)>,
    toggles: Query<(Entity, &OpenToggle)>,
    spotlit: Query<&Spotlit>,
    parents: Query<&ChildOf>,
    nodes: Query<(&Node, &ComputedNode, &UiGlobalTransform)>,
    mut scrolls: Query<&mut ScrollPosition>,
) {
    let Some(key) = nav.focus else {
        if *state != Spotlight::default() {
            *state = Spotlight::default();
        }
        return;
    };
    if nav.screen != Screen::Config(Section::Harness) {
        nav.focus = None;
        return;
    }
    if state.key != Some(key) || state.request != nav.requests {
        *state = Spotlight {
            key: Some(key),
            request: nav.requests,
            ..default()
        };
    }
    let control = fields
        .iter()
        .find(|(_, f)| f.0 == key)
        .map(|(e, _)| e)
        .or_else(|| toggles.iter().find(|(_, t)| t.0 == key).map(|(e, _)| e));
    let Some(row) = control
        .and_then(|c| parents.get(c).ok())
        .map(ChildOf::parent)
    else {
        return;
    };
    let now = time.elapsed_secs_f64();
    let started = *state.started.get_or_insert(now);
    if now - started >= SPOTLIGHT_SECS {
        if let Ok(was) = spotlit.get(row) {
            untint(&mut commands, row, was);
        }
        nav.focus = None;
        *state = Spotlight::default();
        return;
    }
    if spotlit.get(row).is_err()
        && let Ok((node, _, _)) = nodes.get(row)
    {
        tint(&mut commands, row, node);
    }
    if state.scrolled {
        return;
    }
    let Ok((_, row_node, row_at)) = nodes.get(row) else {
        return;
    };
    if row_node.size.y <= 0.0 {
        return;
    }
    let Some(view) = parents.iter_ancestors(row).find(|e| {
        nodes
            .get(*e)
            .is_ok_and(|(n, _, _)| n.overflow.y == OverflowAxis::Scroll)
    }) else {
        return;
    };
    if let Ok((_, view_node, view_at)) = nodes.get(view)
        && let Ok(mut scroll) = scrolls.get_mut(view)
    {
        let (view_top, view_bottom) = span(view_node, view_at);
        let (top, bottom) = span(row_node, row_at);
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
    state.scrolled = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::{self, NOW};

    fn row_of<C: Component>(app: &mut App, want: impl Fn(&C) -> bool) -> Entity {
        let control = testing::find::<C>(app, want);
        app.world().get::<ChildOf>(control).unwrap().parent()
    }

    fn fill_of(app: &App, e: Entity) -> Option<Swatch> {
        app.world().get::<Fill>(e).map(|f| f.0)
    }

    fn config_at(key: &'static str) -> App {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .open_setting(Section::Harness, key);
        testing::settle(&mut app);
        app
    }

    #[test]
    fn the_program_row_is_tinted_when_config_opens_at_it() {
        let mut app = config_at("harness.program");
        let row = row_of::<ConfigField>(&mut app, |f| f.0 == "harness.program");
        assert_eq!(fill_of(&app, row), Some(Swatch::GreenSoft));
        let other = row_of::<ConfigField>(&mut app, |f| f.0 == "harness.extra_args");
        assert_ne!(fill_of(&app, other), Some(Swatch::GreenSoft));
    }

    #[test]
    fn a_checkbox_row_is_tinted_with_its_label() {
        for key in [
            "harness.diagrams",
            "harness.revalidate",
            "harness.keywords_3d",
        ] {
            let mut app = config_at(key);
            let row = row_of::<OpenToggle>(&mut app, |t| t.0 == key);
            assert_eq!(fill_of(&app, row), Some(Swatch::GreenSoft), "{key}");
        }
    }

    #[test]
    fn the_tint_survives_a_rebuild_of_the_page() {
        let mut app = config_at("harness.diagrams");
        testing::set_config_locally(&mut app, "harness.diagrams", "true");
        testing::settle(&mut app);
        let row = row_of::<OpenToggle>(&mut app, |t| t.0 == "harness.diagrams");
        assert_eq!(fill_of(&app, row), Some(Swatch::GreenSoft));
    }

    #[test]
    fn the_tint_ends_and_the_request_is_forgotten() {
        let mut app = config_at("harness.program");
        app.world_mut().resource_mut::<Spotlight>().started = Some(-SPOTLIGHT_SECS - 1.0);
        testing::settle(&mut app);
        let row = row_of::<ConfigField>(&mut app, |f| f.0 == "harness.program");
        assert_eq!(fill_of(&app, row), Some(Swatch::Clear));
        assert_eq!(app.world().resource::<Nav>().focus, None);
        assert_eq!(*app.world().resource::<Spotlight>(), Spotlight::default());
    }

    #[test]
    fn a_request_nobody_showed_is_dropped_when_config_is_left() {
        let mut app = config_at("harness.program");
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Editor);
        app.world_mut().resource_mut::<Nav>().focus = Some("harness.program");
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<Nav>().focus, None);
    }

    /// A scroll area 600 px tall at the top of the screen with one row `top` px below its top.
    fn scrolled_scene(top: f32) -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Spotlight>()
            .insert_resource(Nav::new(&clusia_protocol::WindowTarget::Home))
            .add_systems(Update, spotlight);
        let view = app
            .world_mut()
            .spawn((
                Node {
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ComputedNode {
                    size: Vec2::new(400.0, 600.0),
                    ..ComputedNode::DEFAULT
                },
                UiGlobalTransform::from_xy(200.0, 300.0),
                ScrollPosition::default(),
            ))
            .id();
        let row = app
            .world_mut()
            .spawn((
                Node::default(),
                ComputedNode {
                    size: Vec2::new(400.0, 30.0),
                    ..ComputedNode::DEFAULT
                },
                UiGlobalTransform::from_xy(200.0, top + 15.0),
                ChildOf(view),
            ))
            .id();
        app.world_mut().spawn((
            Node::default(),
            ConfigField("harness.program"),
            ChildOf(row),
        ));
        app.world_mut()
            .resource_mut::<Nav>()
            .open_setting(Section::Harness, "harness.program");
        (app, view, row)
    }

    #[test]
    fn a_row_below_the_fold_is_scrolled_into_view() {
        let (mut app, view, _) = scrolled_scene(900.0);
        app.update();
        let y = app.world().get::<ScrollPosition>(view).unwrap().y;
        assert!(y > 0.0, "scrolled to {y}");
        // 900 below the top, 30 tall: the bottom edge (930) ends up inside the 600 px view.
        assert!(930.0 - y <= 600.0, "the row's bottom is in view at {y}");
    }

    #[test]
    fn a_row_already_in_view_leaves_the_scroll_alone() {
        let (mut app, view, _) = scrolled_scene(200.0);
        app.update();
        assert_eq!(app.world().get::<ScrollPosition>(view).unwrap().y, 0.0);
    }

    #[test]
    fn the_row_is_as_it_was_once_the_tint_ends() {
        for padded in [false, true] {
            let mut app = testing::app(fixture::demo(NOW));
            app.world_mut()
                .resource_mut::<Nav>()
                .open_section(Section::Harness);
            testing::settle(&mut app);
            let row = row_of::<ConfigField>(&mut app, |f| f.0 == "harness.program");
            if padded {
                let mut node = app.world_mut().get_mut::<Node>(row).unwrap();
                node.padding = UiRect::axes(px(3), px(5));
                node.margin = UiRect::top(px(7));
                node.border_radius = BorderRadius::all(px(2));
            }
            let before = app.world().get::<Node>(row).unwrap().clone();
            app.world_mut()
                .resource_mut::<Nav>()
                .open_setting(Section::Harness, "harness.program");
            testing::settle(&mut app);
            assert_eq!(fill_of(&app, row), Some(Swatch::GreenSoft));
            app.world_mut().resource_mut::<Spotlight>().started = Some(-SPOTLIGHT_SECS - 1.0);
            testing::settle(&mut app);
            assert_eq!(fill_of(&app, row), Some(Swatch::Clear));
            assert_eq!(
                *app.world().get::<Node>(row).unwrap(),
                before,
                "padded: {padded}"
            );
        }
    }

    #[test]
    fn asking_again_for_the_same_setting_restarts_the_tint() {
        let mut app = config_at("harness.program");
        app.world_mut().resource_mut::<Spotlight>().started = Some(-SPOTLIGHT_SECS + 0.5);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_setting(Section::Harness, "harness.program");
        testing::settle(&mut app);
        let started = app.world().resource::<Spotlight>().started.unwrap();
        assert!(started > 0.0 - f64::EPSILON, "restarted at {started}");
        assert_eq!(app.world().resource::<Nav>().focus, Some("harness.program"));
    }

    #[test]
    fn the_scroll_waits_until_the_row_has_a_size() {
        let (mut app, view, row) = scrolled_scene(900.0);
        app.world_mut()
            .entity_mut(row)
            .insert(ComputedNode::DEFAULT);
        app.update();
        assert_eq!(app.world().get::<ScrollPosition>(view).unwrap().y, 0.0);
        assert!(!app.world().resource::<Spotlight>().scrolled);
        app.world_mut().entity_mut(row).insert(ComputedNode {
            size: Vec2::new(400.0, 30.0),
            ..ComputedNode::DEFAULT
        });
        app.update();
        assert!(app.world().get::<ScrollPosition>(view).unwrap().y > 0.0);
        assert!(app.world().resource::<Spotlight>().scrolled);
    }
}
