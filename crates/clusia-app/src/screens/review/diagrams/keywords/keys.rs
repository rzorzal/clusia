//! The keyboard of the 3D view: one tab stop whose Tab key walks the points.

use bevy::input::ButtonInput;
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input_focus::{FocusLost, FocusedInput};
use bevy::prelude::*;
use bevy::window::RequestRedraw;
use clusia_diagram::keywords::{ELSEWHERE, PointShape, Scene3D};

use super::input::{orbit_by, zoom_by};
use super::overlays::set_use_case;
use super::render::{view_pose, visible};
use super::{KeywordsCanvas, KeywordsRuntime};
use crate::review_state::{CameraPose, KeywordsUi, ReviewTabs, View3D};

/// Radians one arrow key turns the camera.
pub const ORBIT_STEP: f32 = 0.1;
/// The factor one `+` or `-` multiplies the camera's distance by (or divides it by).
pub const ZOOM_STEP: f32 = 1.2;

/// The shown points in the order Tab visits them: what the PR changed, then declarations, then
/// bundles, then the other uses by use case with the places of no use case last; ties by index.
pub fn tab_order(scene: &Scene3D, ui: &KeywordsUi) -> Vec<u32> {
    let mut order: Vec<(u8, u32, u32)> = scene
        .points
        .iter()
        .enumerate()
        .filter(|(_, p)| visible(ui, p))
        .map(|(i, p)| {
            let class = if p.changed {
                0
            } else if p.shape == PointShape::Declared {
                1
            } else if p.shape == PointShape::Bundle {
                2
            } else {
                3
            };
            let rank = if p.use_case == ELSEWHERE {
                u32::MAX
            } else {
                u32::from(p.use_case)
            };
            (class, rank, i as u32)
        })
        .collect();
    order.sort_unstable();
    order.into_iter().map(|(_, _, i)| i).collect()
}

/// The point Tab (or Shift-Tab) moves to from `hover`; `None` past either end.
fn step(order: &[u32], hover: Option<u32>, shift: bool) -> Option<u32> {
    let at = hover.and_then(|h| order.iter().position(|&i| i == h));
    let next = match (at, shift) {
        (Some(i), false) => i.checked_add(1),
        (Some(i), true) => i.checked_sub(1),
        (None, false) => Some(0),
        (None, true) => order.len().checked_sub(1),
    };
    next.and_then(|i| order.get(i)).copied()
}

/// Applies `key` to the tab's state and the focused point. `true` means the view used the key,
/// so it goes no further; an unused Tab leaves the view (it is never a keyboard trap).
pub fn apply_key(
    ui: &mut KeywordsUi,
    hover: &mut Option<u32>,
    scene: &Scene3D,
    key: &Key,
    shift: bool,
) -> bool {
    match key {
        Key::Tab => {
            let order = tab_order(scene, ui);
            *hover = step(&order, *hover, shift);
            hover.is_some()
        }
        Key::Enter => match *hover {
            Some(i) => {
                ui.card = Some(i);
                true
            }
            None => false,
        },
        Key::Escape => {
            if ui.card.is_some() {
                ui.card = None;
                true
            } else if ui.use_case.is_some() {
                set_use_case(ui, scene, None);
                true
            } else {
                false
            }
        }
        Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown => {
            if ui.view != View3D::Free {
                return false;
            }
            let (dyaw, dpitch) = match key {
                Key::ArrowLeft => (-ORBIT_STEP, 0.0),
                Key::ArrowRight => (ORBIT_STEP, 0.0),
                Key::ArrowUp => (0.0, ORBIT_STEP),
                _ => (0.0, -ORBIT_STEP),
            };
            orbit_by(&mut ui.camera, dyaw, dpitch);
            true
        }
        Key::Character(c) => character(ui, c.as_str()),
        _ => false,
    }
}

fn character(ui: &mut KeywordsUi, c: &str) -> bool {
    match c {
        "+" | "=" => zoom_by(&mut ui.camera, 1.0 / ZOOM_STEP),
        "-" | "_" => zoom_by(&mut ui.camera, ZOOM_STEP),
        "0" => ui.camera = view_pose(ui.view, &CameraPose::default()),
        "1" | "2" | "3" | "4" => {
            let view = match c {
                "1" => View3D::Free,
                "2" => View3D::XY,
                "3" => View3D::XZ,
                _ => View3D::ZY,
            };
            ui.view = view;
            ui.camera = view_pose(view, &ui.camera);
        }
        _ => return false,
    }
    true
}

/// Makes each new canvas one tab stop that takes the keys.
pub(super) fn wire_canvas(mut commands: Commands, new: Query<Entity, Added<KeywordsCanvas>>) {
    for canvas in &new {
        commands
            .entity(canvas)
            .insert_if_new(bevy::input_focus::tab_navigation::TabIndex(0))
            .observe(on_key)
            .observe(on_focus_lost);
    }
}

fn on_key(
    mut event: On<FocusedInput<KeyboardInput>>,
    mut canvases: Query<(&KeywordsCanvas, &mut KeywordsRuntime)>,
    mut tabs: ResMut<ReviewTabs>,
    keys: Res<ButtonInput<KeyCode>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if event.input.state != ButtonState::Pressed {
        return;
    }
    if keys.any_pressed([
        KeyCode::SuperLeft,
        KeyCode::SuperRight,
        KeyCode::ControlLeft,
        KeyCode::ControlRight,
        KeyCode::AltLeft,
        KeyCode::AltRight,
    ]) {
        return;
    }
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    let Ok((KeywordsCanvas(pr), mut runtime)) = canvases.get_mut(event.focused_entity) else {
        return;
    };
    let Some(current) = tabs.0.get(pr).map(|t| t.ui.keywords.clone()) else {
        return;
    };
    let (mut ui, mut hover) = (current.clone(), runtime.hover);
    let used = apply_key(
        &mut ui,
        &mut hover,
        &runtime.scene,
        &event.input.logical_key,
        shift,
    );
    if hover != runtime.hover {
        runtime.hover = hover;
        // The tooltip sits at the point, not at a stale mouse position.
        runtime.cursor = None;
        redraw.write(RequestRedraw);
    }
    if ui != current
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.keywords = ui;
    }
    if used {
        event.propagate(false);
    }
}

fn on_focus_lost(event: On<FocusLost>, mut runtimes: Query<&mut KeywordsRuntime>) {
    if let Ok(mut runtime) = runtimes.get_mut(event.entity)
        && runtime.hover.is_some()
    {
        runtime.hover = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_state::{CameraPose, KeywordsUi, View3D};
    use crate::screens::review::diagrams::keywords::overlays::fixture_map::small_map;
    use crate::screens::review::diagrams::keywords::render::{view_pose, visible};
    use clusia_diagram::keywords::{ELSEWHERE, Point3, PointShape, Scene3D, scene};

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
    }

    fn walk(ui: &mut KeywordsUi, scene: &Scene3D, shift: bool) -> Vec<u32> {
        let mut hover = None;
        let mut seen = Vec::new();
        while apply_key(ui, &mut hover, scene, &Key::Tab, shift) {
            seen.push(hover.expect("a focused point"));
            assert!(seen.len() <= scene.points.len(), "the walk ends");
        }
        assert_eq!(hover, None, "past the end the view lets go");
        seen
    }

    fn synthetic(n: usize) -> Scene3D {
        Scene3D {
            points: (0..n)
                .map(|i| Point3 {
                    places: vec![i as u32],
                    pos: [0.0; 3],
                    shape: PointShape::Used,
                    family: (i % 12) as u16,
                    use_case: (i % 8) as u16,
                    changed: i % 7 == 0,
                    radius: 0.028,
                })
                .collect(),
            lines: Vec::new(),
            clouds: Vec::new(),
            more: 0,
        }
    }

    #[test]
    fn tab_goes_changed_then_declared_then_bundles_then_uses() {
        let scene = scene(&small_map(true));
        let ui = KeywordsUi::default();
        let order = tab_order(&scene, &ui);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            (0..scene.points.len() as u32).collect::<Vec<_>>(),
            "every point once, bundles included"
        );
        let class = |i: &u32| {
            let p = &scene.points[*i as usize];
            if p.changed {
                0
            } else if p.shape == PointShape::Declared {
                1
            } else if p.shape == PointShape::Bundle {
                2
            } else {
                3
            }
        };
        let classes: Vec<u8> = order.iter().map(class).collect();
        assert!(classes.windows(2).all(|w| w[0] <= w[1]), "{classes:?}");
        assert_eq!(classes.iter().filter(|c| **c == 0).count(), 2);
        assert_eq!(classes.iter().filter(|c| **c == 2).count(), 1);
        let uses: Vec<u16> = order
            .iter()
            .filter(|i| class(i) == 3)
            .map(|i| scene.points[*i as usize].use_case)
            .collect();
        let key = |u: &u16| if *u == ELSEWHERE { u32::MAX } else { *u as u32 };
        assert!(
            uses.windows(2).all(|w| key(&w[0]) <= key(&w[1])),
            "by use case, Elsewhere last"
        );
    }

    #[test]
    fn hidden_points_are_skipped() {
        let scene = scene(&small_map(false));
        let ui = KeywordsUi {
            hidden_families: 0b10,
            ..Default::default()
        };
        let order = tab_order(&scene, &ui);
        assert!(
            order
                .iter()
                .all(|i| visible(&ui, &scene.points[*i as usize]))
        );
        assert!(order.len() < scene.points.len());
        assert!(!order.is_empty());
    }

    #[test]
    fn tab_walks_forward_and_shift_tab_backward_then_both_let_go() {
        let scene = scene(&small_map(true));
        let mut ui = KeywordsUi::default();
        let order = tab_order(&scene, &ui);
        assert_eq!(walk(&mut ui, &scene, false), order);
        let mut back = order.clone();
        back.reverse();
        assert_eq!(walk(&mut ui, &scene, true), back);
    }

    #[test]
    fn tab_reaches_exactly_400_points_and_none() {
        let mut ui = KeywordsUi::default();
        let full = synthetic(400);
        assert_eq!(walk(&mut ui, &full, false).len(), 400);
        let mut hidden = KeywordsUi {
            hidden_families: u16::MAX,
            ..Default::default()
        };
        let mut hover = None;
        assert!(!apply_key(&mut hidden, &mut hover, &full, &Key::Tab, false));
        assert_eq!(hover, None);
        assert!(!apply_key(
            &mut hidden,
            &mut hover,
            &synthetic(0),
            &Key::Tab,
            true
        ));
        assert!(!apply_key(
            &mut hidden,
            &mut hover,
            &synthetic(0),
            &Key::Enter,
            false
        ));
    }

    #[test]
    fn enter_opens_the_focused_point() {
        let scene = scene(&small_map(false));
        let mut ui = KeywordsUi::default();
        let mut hover = None;
        assert!(!apply_key(&mut ui, &mut hover, &scene, &Key::Enter, false));
        assert_eq!(ui.card, None);
        hover = Some(3);
        assert!(apply_key(&mut ui, &mut hover, &scene, &Key::Enter, false));
        assert_eq!(ui.card, Some(3));
    }

    #[test]
    fn escape_closes_the_card_then_the_use_case_then_is_not_used() {
        let scene = scene(&small_map(false));
        let mut ui = KeywordsUi {
            card: Some(2),
            use_case: Some(1),
            ..Default::default()
        };
        let mut hover = None;
        assert!(apply_key(&mut ui, &mut hover, &scene, &Key::Escape, false));
        assert_eq!((ui.card, ui.use_case), (None, Some(1)));
        assert!(apply_key(&mut ui, &mut hover, &scene, &Key::Escape, false));
        assert_eq!(ui.use_case, None);
        assert_eq!(ui.camera, CameraPose::default());
        assert!(!apply_key(&mut ui, &mut hover, &scene, &Key::Escape, false));
    }

    #[test]
    fn arrows_orbit_in_3d_only() {
        let scene = scene(&small_map(false));
        let mut ui = KeywordsUi::default();
        let mut hover = None;
        let start = ui.camera;
        assert!(apply_key(
            &mut ui,
            &mut hover,
            &scene,
            &Key::ArrowRight,
            false
        ));
        assert!(((ui.camera.yaw - start.yaw).abs() - ORBIT_STEP).abs() < 1e-6);
        assert_eq!(ui.camera.pitch, start.pitch);
        let after_right = ui.camera.yaw;
        assert!(apply_key(
            &mut ui,
            &mut hover,
            &scene,
            &Key::ArrowLeft,
            false
        ));
        assert!(
            (ui.camera.yaw - start.yaw).abs() < 1e-6,
            "left undoes right"
        );
        assert_ne!(after_right, start.yaw);
        assert!(apply_key(&mut ui, &mut hover, &scene, &Key::ArrowUp, false));
        assert_ne!(ui.camera.pitch, start.pitch);
        ui.view = View3D::XY;
        ui.camera = view_pose(View3D::XY, &ui.camera);
        let flat = ui.camera;
        assert!(!apply_key(
            &mut ui,
            &mut hover,
            &scene,
            &Key::ArrowRight,
            false
        ));
        assert_eq!(ui.camera, flat);
    }

    #[test]
    fn plus_minus_zoom_and_zero_resets() {
        let scene = scene(&small_map(false));
        let mut ui = KeywordsUi::default();
        let mut hover = None;
        let start = ui.camera.distance;
        for plus in ["+", "="] {
            ui.camera.distance = start;
            assert!(apply_key(&mut ui, &mut hover, &scene, &ch(plus), false));
            assert!(ui.camera.distance < start, "{plus} comes closer");
        }
        ui.camera.distance = start;
        assert!(apply_key(&mut ui, &mut hover, &scene, &ch("-"), false));
        assert!(ui.camera.distance > start);
        assert!(apply_key(&mut ui, &mut hover, &scene, &ch("0"), false));
        assert_eq!(ui.camera, CameraPose::default());
        ui.view = View3D::XZ;
        assert!(apply_key(&mut ui, &mut hover, &scene, &ch("0"), false));
        assert_eq!(ui.camera, view_pose(View3D::XZ, &CameraPose::default()));
    }

    #[test]
    fn the_number_keys_pick_the_view() {
        let scene = scene(&small_map(false));
        let mut ui = KeywordsUi {
            use_case: Some(0),
            card: Some(1),
            hidden_families: 0b1,
            ..Default::default()
        };
        let mut hover = None;
        for (key, view) in [
            ("2", View3D::XY),
            ("3", View3D::XZ),
            ("4", View3D::ZY),
            ("1", View3D::Free),
        ] {
            let before = ui.camera;
            assert!(apply_key(&mut ui, &mut hover, &scene, &ch(key), false));
            assert_eq!(ui.view, view);
            assert_eq!(ui.camera, view_pose(view, &before));
        }
        assert_eq!(
            (ui.use_case, ui.card, ui.hidden_families),
            (Some(0), Some(1), 0b1)
        );
        assert!(
            !apply_key(&mut ui, &mut hover, &scene, &ch("x"), false),
            "other keys are not ours"
        );
        assert!(!apply_key(&mut ui, &mut hover, &scene, &ch("5"), false));
    }

    #[test]
    fn the_view_is_one_tab_stop_and_losing_focus_clears_the_tooltip() {
        use crate::fixture;
        use crate::screens::review::diagrams::keywords::{KeywordsCanvas, KeywordsRuntime};
        use crate::testing;
        use bevy::input_focus::{FocusLost, tab_navigation::TabIndex};
        let mut app = testing::app(fixture::demo(testing::NOW));
        let _pr = testing::open_keywords(&mut app);
        let canvas = testing::find::<KeywordsCanvas>(&mut app, |_| true);
        assert_eq!(app.world().get::<TabIndex>(canvas), Some(&TabIndex(0)));
        app.world_mut()
            .get_mut::<KeywordsRuntime>(canvas)
            .expect("a runtime")
            .hover = Some(0);
        app.world_mut().trigger(FocusLost { entity: canvas });
        app.update();
        assert_eq!(
            app.world().get::<KeywordsRuntime>(canvas).unwrap().hover,
            None
        );
    }
}
