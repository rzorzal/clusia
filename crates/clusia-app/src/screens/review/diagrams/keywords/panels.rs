//! The panels laid over the 3D scene. This file holds the header: the title, the hint, the view
//! switch (3D and the three flat views, named after the plan's axes) and the link to the
//! setting that turns the view off. `spawn_overlays` is where the panels that sit on the scene
//! itself are added.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};

use clusia_core::PrRef;
use clusia_core::keywords::{Axis, KeywordMap};
use clusia_core::printable::printable;

use super::render::view_pose;
use super::{KeywordsCanvas, KeywordsRuntime};
use crate::fonts::UiFonts;
use crate::review_state::{ReviewTabs, View3D};
use crate::screens::review::shell::setting_link;
use crate::ui::kit::{Tone, Type, badge, segment, segments, text};

pub const HINT: &str = "Drag to turn · scroll to zoom · click a point to read its code";
pub const CHIP: &str = "Keywords 3D";
/// The longest an axis name gets in the view switch.
const SWITCH_NAME_MAX: usize = 18;

/// Everything the header draws, already printable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderView {
    pub title: String,
    /// The labels of `[Free, XY, XZ, ZY]`.
    pub switch: [String; 4],
    pub view: View3D,
}

fn cut(s: &str, max: usize) -> String {
    let s = printable(s);
    if s.chars().count() <= max {
        return s;
    }
    let mut short: String = s.chars().take(max).collect();
    short.push('…');
    short
}

/// `3D`, then the three planes by axis name: `X × Y`, `X × Z`, `Z × Y`.
pub fn switch_labels(axes: &[Axis; 3]) -> [String; 4] {
    let n = |i: usize| cut(&axes[i].name, SWITCH_NAME_MAX);
    [
        "3D".to_string(),
        format!("{} × {}", n(0), n(1)),
        format!("{} × {}", n(0), n(2)),
        format!("{} × {}", n(2), n(1)),
    ]
}

pub fn header_view(map: &KeywordMap, view: View3D) -> HeaderView {
    HeaderView {
        title: cut(&map.plan.title, 80),
        switch: switch_labels(&map.plan.axes),
        view,
    }
}

pub const VIEWS: [View3D; 4] = [View3D::Free, View3D::XY, View3D::XZ, View3D::ZY];

/// The header's node; `fill_header` fills it while its view differs from what was built.
#[derive(Component, Debug)]
pub struct HeaderRegion {
    pub pr: PrRef,
    pub built: Option<HeaderView>,
}

/// One segment of the view switch.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ViewButton {
    pub pr: PrRef,
    pub view: View3D,
}

pub(super) fn fill_header(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    fonts: Res<UiFonts>,
    runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
    mut regions: Query<(Entity, &mut HeaderRegion)>,
) {
    for (entity, mut region) in &mut regions {
        let Some(tab) = tabs.0.get(&region.pr) else {
            continue;
        };
        let Some((_, rt)) = runtimes.iter().find(|(c, _)| c.0 == region.pr) else {
            continue;
        };
        let view = header_view(&rt.map, tab.ui.keywords.view);
        if region.built.as_ref() == Some(&view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let pr = region.pr.clone();
        commands
            .entity(entity)
            .with_children(|p| draw_header(p, &fonts, &pr, &view));
        region.built = Some(view);
    }
}

fn draw_header(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &HeaderView) {
    p.spawn(Node {
        flex_grow: 1.0,
        flex_shrink: 1.0,
        min_width: px(0),
        column_gap: px(10),
        align_items: AlignItems::Center,
        overflow: Overflow::clip(),
        ..default()
    })
    .with_children(|l| {
        l.spawn((
            text(fonts, v.title.clone(), Type::STRONG.size(14.0)),
            TextLayout::no_wrap(),
        ));
        l.spawn(badge(fonts, CHIP, Tone::Neutral));
        l.spawn((text(fonts, HINT, Type::META), TextLayout::no_wrap()));
    });
    p.spawn(Node {
        flex_shrink: 0.0,
        column_gap: px(12),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|r| {
        r.spawn(segments()).with_children(|s| {
            for (view, label) in VIEWS.into_iter().zip(&v.switch) {
                s.spawn((
                    segment(fonts, label, v.view == view),
                    ViewButton {
                        pr: pr.clone(),
                        view,
                    },
                    observe(on_view),
                ));
            }
        });
        setting_link(r, fonts, "Turn off in Config", "harness.keywords_3d");
    });
}

fn on_view(activate: On<Activate>, buttons: Query<&ViewButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(button) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&button.pr) {
        let ui = &mut tab.ui.keywords;
        ui.camera = view_pose(button.view, &ui.camera);
        ui.view = button.view;
    }
}

/// Adds the panels that sit on the scene (the use-case pills, the focus card, the filters and
/// the legend).
pub fn spawn_overlays(_p: &mut ChildSpawnerCommands, _fonts: &UiFonts, _pr: &PrRef) {}

/// Adds what floats above the panels: the hover tooltip and the code card.
pub fn spawn_floating(_p: &mut ChildSpawnerCommands, _fonts: &UiFonts, _pr: &PrRef) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::review_state::CameraPose;
    use crate::testing::{self, NOW};
    use clusia_core::keywords::Axis;

    fn axes(a: &str, b: &str, c: &str) -> [Axis; 3] {
        let axis = |name: &str| Axis {
            name: name.into(),
            low: "low".into(),
            high: "high".into(),
        };
        [axis(a), axis(b), axis(c)]
    }

    #[test]
    fn the_switch_is_named_after_the_axes() {
        assert_eq!(
            switch_labels(&axes("Layer", "Moment", "Distance")),
            [
                "3D",
                "Layer × Moment",
                "Layer × Distance",
                "Distance × Moment"
            ]
        );
    }

    #[test]
    fn a_long_axis_name_is_cut_in_the_switch_and_a_control_character_is_shown_escaped() {
        let long = "A very long axis name that would stretch the switch";
        let labels = switch_labels(&axes(long, "Mo\u{202e}ment", "Z"));
        assert!(
            labels[1].starts_with("A very long axis n… × "),
            "{labels:?}"
        );
        assert!(labels.iter().all(|l| !l.contains('\u{202e}')), "{labels:?}");
        assert!(labels.iter().all(|l| l.chars().count() <= 18 * 2 + 5));
    }

    #[test]
    fn the_header_names_the_entry_and_the_view() {
        let map = fixture::keywords_map();
        let v = header_view(&map, View3D::XZ);
        assert_eq!(v.title, "Where the words live");
        assert_eq!(v.view, View3D::XZ);
        assert_eq!(v.switch[0], "3D");
    }

    #[test]
    fn the_header_shows_its_texts_and_the_setting_link() {
        let mut app = testing::app(fixture::demo(NOW));
        let _pr = testing::open_keywords(&mut app);
        for needle in [
            "Where the words live",
            "Keywords 3D",
            HINT,
            "Layer × Moment",
            "Distance × Moment",
            "Turn off in Config",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        testing::open_setting(&mut app, "harness.keywords_3d");
        assert_eq!(
            testing::config_focus(&app).1,
            Some("harness.keywords_3d"),
            "the link opens Config at the setting"
        );
    }

    #[test]
    fn a_click_on_the_switch_sets_the_view_and_its_pose_and_keeps_the_rest() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_keywords_with(&mut app, fixture::keywords_map(), |ui| {
            ui.use_case = Some(1);
            ui.card = Some(2);
            ui.hidden_families = 0b10;
        });
        let button = testing::find::<ViewButton>(&mut app, |b| b.view == View3D::XZ);
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        let ui = testing::tab(&app, &pr).ui.keywords;
        assert_eq!(ui.view, View3D::XZ);
        assert_eq!(ui.camera, view_pose(View3D::XZ, &CameraPose::default()));
        assert_eq!(
            (ui.use_case, ui.card, ui.hidden_families),
            (Some(1), Some(2), 0b10)
        );
        let back = testing::find::<ViewButton>(&mut app, |b| b.view == View3D::Free);
        testing::activate(&mut app, back);
        testing::settle(&mut app);
        let ui = testing::tab(&app, &pr).ui.keywords;
        assert_eq!(ui.view, View3D::Free);
        assert_eq!(
            ui.camera.yaw,
            CameraPose::default().yaw,
            "the default angles are back"
        );
    }
}
