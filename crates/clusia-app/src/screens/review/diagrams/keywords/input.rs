//! The pointer side of the 3D keyword view: orbit on drag, zoom on scroll and pinch, the screen-
//! space picking, the hover that lights a family, and the click that opens a card or lets go.
//!
//! The observers sit on the `SceneImage` node. A drag never opens a card or lets go: the click
//! that ends it is swallowed, as on the box canvas.

use std::f32::consts::{PI, TAU};

use bevy::diagnostic::FrameCount;
use bevy::input::gestures::PinchGesture;
use bevy::input::mouse::MouseScrollUnit;
use bevy::picking::events::{Click, Drag, DragEnd, Move, Out, Pointer, Press, Scroll};
use bevy::picking::pointer::PointerButton;
use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform};
use bevy::window::{PrimaryWindow, RequestRedraw};

use clusia_diagram::keywords::Scene3D;

use super::overlays::set_use_case;
use super::render::{Lens, SCALE, clamped, pixel_radius, project, visible, world};
use super::{KeywordsCanvas, KeywordsRuntime, SceneImage};
use crate::review_state::{CameraPose, KeywordsUi, ReviewTabs, View3D};
use crate::screens::review::diagrams::view::{pinch_factor, wheel_pixels};

/// A drag longer than this (in pixels) is a turn: the click that ends it opens nothing.
pub const DRAG_SLOP: f32 = 4.0;
/// How long after its last move a drag still swallows the click that ends it, in seconds.
pub const DRAG_GRACE: f64 = 0.25;
/// Pixels around a point's disc that still hit it.
pub const TOLERANCE_PX: f32 = 6.0;
/// Radians a drag of one pixel turns the camera.
pub const ORBIT_PER_PIXEL: f32 = 0.008;
/// The camera never goes over the poles.
pub const MAX_PITCH: f32 = 1.5;

/// What the pointer did on the image: how far the primary button was dragged and when.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq)]
pub struct DragState {
    pub dragged: f32,
    pub drag_at: f64,
    pub dragging: bool,
    pub drag_end_frame: Option<u32>,
}

pub fn press(state: &mut DragState) {
    *state = DragState::default();
}

pub fn drag(state: &mut DragState, distance: Vec2, now: f64) {
    state.dragged = state.dragged.max(distance.length());
    state.drag_at = now;
    state.dragging = true;
}

/// The drag was released in `frame`. The click that ends a drag can arrive before or after
/// this, in the same frame, so both orders are swallowed.
pub fn end_drag(state: &mut DragState, frame: u32) {
    state.dragging = false;
    state.drag_end_frame = Some(frame);
}

/// Whether a click at `now`, in `frame`, is the end of a drag longer than `DRAG_SLOP`: one
/// still going on, released in this frame, or moved less than `DRAG_GRACE` ago.
pub fn swallows(state: &DragState, now: f64, frame: u32) -> bool {
    state.dragged > DRAG_SLOP
        && (state.dragging
            || state.drag_end_frame == Some(frame)
            || now - state.drag_at < DRAG_GRACE)
}

/// Turns the camera by `dyaw` around Y and `dpitch` up.
pub fn orbit_by(pose: &mut CameraPose, dyaw: f32, dpitch: f32) {
    if !(dyaw.is_finite() && dpitch.is_finite()) {
        return;
    }
    pose.yaw = (pose.yaw + dyaw + PI).rem_euclid(TAU) - PI;
    pose.pitch = (pose.pitch + dpitch).clamp(-MAX_PITCH, MAX_PITCH);
    *pose = clamped(*pose);
}

/// Multiplies the camera's distance by `factor` (above 1 moves away), inside its limits.
pub fn zoom_by(pose: &mut CameraPose, factor: f32) {
    if factor.is_finite() && factor > 0.0 {
        pose.distance *= factor;
        *pose = clamped(*pose);
    }
}

/// The distance factor of a scroll over `pixels`: up (positive) moves closer, 100 px by about
/// 22 %.
pub fn wheel_zoom(pixels: f32) -> f32 {
    (-pixels * 0.0025).exp()
}

/// The point under `cursor` (canvas pixels): each shown point is a disc of its projected
/// radius plus `TOLERANCE_PX`; the nearest to the camera wins.
pub fn pick(
    scene: &Scene3D,
    ui: &KeywordsUi,
    pose: &CameraPose,
    lens: Lens,
    size: Vec2,
    cursor: Vec2,
) -> Option<u32> {
    let mut best: Option<(f32, u32)> = None;
    for (i, p) in scene.points.iter().enumerate() {
        if !visible(ui, p) {
            continue;
        }
        let Some((at, depth)) = project(pose, lens, size, world(p.pos)) else {
            continue;
        };
        let reach = pixel_radius(pose, lens, size, p.radius * SCALE, depth) + TOLERANCE_PX;
        if at.distance(cursor) <= reach && best.is_none_or(|(d, _)| depth < d) {
            best = Some((depth, i as u32));
        }
    }
    best.map(|(_, i)| i)
}

/// The pointer's place on the image node, in the node's logical pixels.
fn local(position: Vec2, node: &ComputedNode, at: &UiGlobalTransform) -> Vec2 {
    let s = node.inverse_scale_factor;
    position - (at.translation * s - node.size * s / 2.0)
}

fn logical_size(node: &ComputedNode) -> Vec2 {
    node.size * node.inverse_scale_factor
}

type Images<'w, 's> = Query<
    'w,
    's,
    (
        &'static SceneImage,
        &'static ComputedNode,
        &'static UiGlobalTransform,
    ),
>;
type Runtimes<'w, 's> = Query<'w, 's, (&'static KeywordsCanvas, &'static mut KeywordsRuntime)>;

/// Puts the observers on every new image node.
pub(super) fn attach_input(mut commands: Commands, added: Query<Entity, Added<SceneImage>>) {
    for image in &added {
        commands
            .entity(image)
            .insert(DragState::default())
            .observe(
                move |moved: On<Pointer<Move>>,
                      tabs: Res<ReviewTabs>,
                      images: Images,
                      drags: Query<&DragState>,
                      mut runtimes: Runtimes,
                      mut redraw: MessageWriter<RequestRedraw>| {
                    let Ok((scene_image, node, at)) = images.get(image) else {
                        return;
                    };
                    if drags.get(image).is_ok_and(|d| d.dragging) {
                        return;
                    }
                    let cursor = local(moved.pointer_location.position, node, at);
                    let (Some(tab), Some((_, mut rt))) = (
                        tabs.0.get(&scene_image.0),
                        runtimes.iter_mut().find(|(c, _)| c.0 == scene_image.0),
                    ) else {
                        return;
                    };
                    let hit = pick(
                        &rt.scene,
                        &tab.ui.keywords,
                        &rt.pose,
                        rt.lens,
                        logical_size(node),
                        cursor,
                    );
                    let cursor = hit.map(|_| cursor);
                    if rt.hover != hit || rt.cursor != cursor {
                        rt.hover = hit;
                        rt.cursor = cursor;
                        redraw.write(RequestRedraw);
                    }
                },
            )
            .observe(
                move |_: On<Pointer<Out>>,
                      images: Images,
                      mut runtimes: Runtimes,
                      mut redraw: MessageWriter<RequestRedraw>| {
                    let Ok((scene_image, _, _)) = images.get(image) else {
                        return;
                    };
                    if let Some((_, mut rt)) =
                        runtimes.iter_mut().find(|(c, _)| c.0 == scene_image.0)
                        && (rt.hover.is_some() || rt.cursor.is_some())
                    {
                        rt.hover = None;
                        rt.cursor = None;
                        redraw.write(RequestRedraw);
                    }
                },
            )
            .observe(
                move |pressed: On<Pointer<Press>>,
                      images: Images,
                      mut drags: Query<&mut DragState>,
                      mut runtimes: Runtimes| {
                    if pressed.event.button != PointerButton::Primary {
                        return;
                    }
                    if let Ok(mut state) = drags.get_mut(image) {
                        press(&mut state);
                    }
                    if let Ok((scene_image, _, _)) = images.get(image)
                        && let Some((_, mut rt)) =
                            runtimes.iter_mut().find(|(c, _)| c.0 == scene_image.0)
                    {
                        rt.dragging = false;
                    }
                },
            )
            .observe(
                move |dragged: On<Pointer<Drag>>,
                      time: Res<Time>,
                      tabs: Res<ReviewTabs>,
                      images: Images,
                      mut drags: Query<&mut DragState>,
                      mut runtimes: Runtimes,
                      mut redraw: MessageWriter<RequestRedraw>| {
                    if dragged.event.button != PointerButton::Primary {
                        return;
                    }
                    if let Ok(mut state) = drags.get_mut(image) {
                        drag(&mut state, dragged.event.distance, time.elapsed_secs_f64());
                    }
                    let Ok((scene_image, _, _)) = images.get(image) else {
                        return;
                    };
                    let (Some(tab), Some((_, mut rt))) = (
                        tabs.0.get(&scene_image.0),
                        runtimes.iter_mut().find(|(c, _)| c.0 == scene_image.0),
                    ) else {
                        return;
                    };
                    rt.dragging = true;
                    rt.hover = None;
                    rt.cursor = None;
                    if tab.ui.keywords.view == View3D::Free {
                        let delta = dragged.event.delta;
                        orbit_by(
                            &mut rt.pose,
                            -delta.x * ORBIT_PER_PIXEL,
                            delta.y * ORBIT_PER_PIXEL,
                        );
                    }
                    redraw.write(RequestRedraw);
                },
            )
            .observe(
                move |ended: On<Pointer<DragEnd>>,
                      frame: Res<FrameCount>,
                      images: Images,
                      mut drags: Query<&mut DragState>,
                      mut runtimes: Runtimes,
                      mut tabs: ResMut<ReviewTabs>| {
                    if ended.event.button != PointerButton::Primary {
                        return;
                    }
                    if let Ok(mut state) = drags.get_mut(image) {
                        end_drag(&mut state, frame.0);
                    }
                    let Ok((scene_image, _, _)) = images.get(image) else {
                        return;
                    };
                    let Some((_, mut rt)) = runtimes.iter_mut().find(|(c, _)| c.0 == scene_image.0)
                    else {
                        return;
                    };
                    rt.dragging = false;
                    if let Some(tab) = tabs.0.get_mut(&scene_image.0)
                        && tab.ui.keywords.camera != rt.pose
                    {
                        tab.ui.keywords.camera = rt.pose;
                    }
                },
            )
            .observe(
                move |clicked: On<Pointer<Click>>,
                      time: Res<Time>,
                      frame: Res<FrameCount>,
                      images: Images,
                      mut drags: Query<&mut DragState>,
                      runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
                      mut tabs: ResMut<ReviewTabs>| {
                    if clicked.event.button != PointerButton::Primary {
                        return;
                    }
                    if let Ok(mut state) = drags.get_mut(image)
                        && swallows(&state, time.elapsed_secs_f64(), frame.0)
                    {
                        state.dragged = 0.0;
                        return;
                    }
                    let Ok((scene_image, node, at)) = images.get(image) else {
                        return;
                    };
                    let cursor = local(clicked.pointer_location.position, node, at);
                    let Some((_, rt)) = runtimes.iter().find(|(c, _)| c.0 == scene_image.0) else {
                        return;
                    };
                    let Some(tab) = tabs.0.get_mut(&scene_image.0) else {
                        return;
                    };
                    let hit = pick(
                        &rt.scene,
                        &tab.ui.keywords,
                        &rt.pose,
                        rt.lens,
                        logical_size(node),
                        cursor,
                    );
                    let ui = &mut tab.ui.keywords;
                    match hit {
                        Some(i) if ui.card != Some(i) => ui.card = Some(i),
                        None if ui.card.is_some() || ui.use_case.is_some() => {
                            ui.card = None;
                            // Letting go of a focused use case also returns the camera, as the
                            // pills and Esc do.
                            if ui.use_case.is_some() {
                                set_use_case(ui, &rt.scene, None);
                            }
                        }
                        _ => {}
                    }
                },
            )
            .observe(
                move |mut scrolled: On<Pointer<Scroll>>,
                      images: Images,
                      mut runtimes: Runtimes,
                      mut tabs: ResMut<ReviewTabs>,
                      mut redraw: MessageWriter<RequestRedraw>| {
                    scrolled.propagate(false);
                    let Ok((scene_image, _, _)) = images.get(image) else {
                        return;
                    };
                    let line = scrolled.event.unit == MouseScrollUnit::Line;
                    let Some((_, mut rt)) = runtimes.iter_mut().find(|(c, _)| c.0 == scene_image.0)
                    else {
                        return;
                    };
                    zoom_by(
                        &mut rt.pose,
                        wheel_zoom(wheel_pixels(line, scrolled.event.y)),
                    );
                    // The tab's pose waits for the drag to end, which writes it.
                    if !rt.dragging
                        && let Some(tab) = tabs.0.get_mut(&scene_image.0)
                    {
                        tab.ui.keywords.camera = rt.pose;
                    }
                    redraw.write(RequestRedraw);
                },
            );
    }
}

/// A trackpad pinch over the image zooms the camera.
pub(super) fn pinch(
    mut pinches: MessageReader<PinchGesture>,
    windows: Query<&Window, With<PrimaryWindow>>,
    images: Images,
    mut runtimes: Runtimes,
    mut tabs: ResMut<ReviewTabs>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let cursor = windows.iter().next().and_then(Window::cursor_position);
    for pinch in pinches.read() {
        let Some(cursor) = cursor else { continue };
        for (scene_image, node, at) in &images {
            let relative = local(cursor, node, at);
            if relative.cmpge(Vec2::ZERO).all()
                && relative.cmple(logical_size(node)).all()
                && let Some((_, mut rt)) = runtimes.iter_mut().find(|(c, _)| c.0 == scene_image.0)
            {
                zoom_by(&mut rt.pose, 1.0 / pinch_factor(pinch.0));
                if let Some(tab) = tabs.0.get_mut(&scene_image.0) {
                    tab.ui.keywords.camera = rt.pose;
                }
                redraw.write(RequestRedraw);
            }
        }
    }
}

pub(super) fn add_systems(app: &mut App) {
    app.add_systems(Update, (attach_input, pinch).after(super::fill_keywords));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::screens::review::diagrams::keywords::panels::{HeaderRegion, ViewButton};
    use crate::screens::review::diagrams::keywords::render::{MAX_DISTANCE, MIN_DISTANCE};
    use crate::screens::review::diagrams::keywords::{Label3, SceneImage, render};
    use crate::testing::{self, NOW};
    use crate::ui::spinner::Motion;
    use clusia_core::PrRef;
    use clusia_diagram::keywords::{Point3, PointShape, scene};

    const SIZE: Vec2 = Vec2::new(800.0, 600.0);

    fn one(pos: [f32; 3], family: u16) -> Scene3D {
        Scene3D {
            points: vec![Point3 {
                places: vec![0],
                pos,
                shape: PointShape::Used,
                family,
                use_case: 0,
                changed: false,
                radius: 0.028,
            }],
            lines: vec![],
            clouds: vec![],
            more: 0,
        }
    }

    fn centre(scene: &Scene3D, i: usize) -> Vec2 {
        project(
            &CameraPose::default(),
            Lens::Perspective,
            SIZE,
            world(scene.points[i].pos),
        )
        .expect("in front")
        .0
    }

    fn pick_at(scene: &Scene3D, ui: &KeywordsUi, at: Vec2) -> Option<u32> {
        pick(
            scene,
            ui,
            &CameraPose::default(),
            Lens::Perspective,
            SIZE,
            at,
        )
    }

    #[test]
    fn a_drag_longer_than_the_slop_swallows_the_click_that_ends_it() {
        let mut s = DragState::default();
        press(&mut s);
        drag(&mut s, Vec2::new(3.0, 0.0), 1.0);
        assert!(!swallows(&s, 1.1, 5), "a wobble is a click");
        drag(&mut s, Vec2::new(12.0, 0.0), 1.0);
        assert!(swallows(&s, 1.1, 5), "still going on");
        end_drag(&mut s, 5);
        assert!(swallows(&s, 9.0, 5), "released in this very frame");
        assert!(!swallows(&s, 9.0, 6), "a later click is a click");
        press(&mut s);
        assert_eq!(s, DragState::default(), "a press forgets the last drag");
    }

    #[test]
    fn a_drag_that_paused_before_the_release_is_still_a_drag() {
        let mut s = DragState::default();
        drag(&mut s, Vec2::new(30.0, 0.0), 1.0);
        assert!(swallows(&s, 9.0, 1), "the button is still down");
    }

    #[test]
    fn the_camera_turns_without_going_over_the_poles_and_the_yaw_wraps() {
        let mut p = CameraPose::default();
        orbit_by(&mut p, 10.0, 10.0);
        assert_eq!(p.pitch, MAX_PITCH);
        assert!(p.yaw > -PI - 1e-4 && p.yaw <= PI + 1e-4, "{}", p.yaw);
        orbit_by(&mut p, 0.0, -10.0);
        assert_eq!(p.pitch, -MAX_PITCH);
        let before = p;
        orbit_by(&mut p, f32::NAN, 1.0);
        assert_eq!(p, before, "NaN changes nothing");
    }

    #[test]
    fn the_camera_zooms_between_its_limits_and_a_bad_factor_changes_nothing() {
        let mut p = CameraPose::default();
        let d = p.distance;
        zoom_by(&mut p, 1.2);
        assert!((p.distance - d * 1.2).abs() < 1e-4);
        zoom_by(&mut p, 1000.0);
        assert_eq!(p.distance, MAX_DISTANCE);
        zoom_by(&mut p, 0.0001);
        assert_eq!(p.distance, MIN_DISTANCE);
        let before = p;
        for bad in [f32::NAN, 0.0, -2.0, f32::INFINITY] {
            zoom_by(&mut p, bad);
        }
        assert_eq!(p, before);
    }

    #[test]
    fn scrolling_up_moves_closer_and_down_moves_away() {
        assert!(wheel_zoom(100.0) < 1.0);
        assert!(wheel_zoom(-100.0) > 1.0);
        assert_eq!(wheel_zoom(0.0), 1.0);
    }

    #[test]
    fn a_click_picks_the_point_under_it_and_nothing_in_the_clear() {
        let scene = scene(&fixture::keywords_map());
        let ui = KeywordsUi::default();
        let mut picked = 0;
        for i in 0..scene.points.len() {
            let Some(hit) = pick_at(&scene, &ui, centre(&scene, i)) else {
                panic!("point {i} is not pickable at its own centre");
            };
            let (a, b) = (&scene.points[i], &scene.points[hit as usize]);
            let depth = |p: &Point3| {
                project(
                    &CameraPose::default(),
                    Lens::Perspective,
                    SIZE,
                    world(p.pos),
                )
                .unwrap()
                .1
            };
            assert!(
                depth(b) <= depth(a) + 1e-4,
                "only something nearer can take it"
            );
            picked += usize::from(hit as usize == i);
        }
        assert!(picked > 0, "some point is on top at its own centre");
        assert_eq!(pick_at(&scene, &ui, Vec2::new(2.0, 2.0)), None);
    }

    #[test]
    fn the_nearest_point_on_a_ray_wins() {
        let mut scene = one([0.0; 3], 0);
        let towards_the_camera = CameraPose::default().eye().normalize() * 0.4;
        let mut near = scene.points[0].clone();
        near.pos = towards_the_camera.to_array();
        scene.points.push(near);
        assert_eq!(pick_at(&scene, &KeywordsUi::default(), SIZE / 2.0), Some(1));
    }

    #[test]
    fn the_tolerance_is_six_pixels_around_the_disc() {
        let scene = one([0.0; 3], 0);
        let ui = KeywordsUi::default();
        let at = centre(&scene, 0);
        let depth = CameraPose::default().distance;
        let r = pixel_radius(
            &CameraPose::default(),
            Lens::Perspective,
            SIZE,
            0.028 * SCALE,
            depth,
        );
        assert_eq!(pick_at(&scene, &ui, at + Vec2::new(r + 5.5, 0.0)), Some(0));
        assert_eq!(pick_at(&scene, &ui, at + Vec2::new(r + 6.5, 0.0)), None);
    }

    #[test]
    fn a_point_the_filters_hide_or_behind_the_camera_cannot_be_picked() {
        let scene = one([0.0; 3], 3);
        let ui = KeywordsUi {
            hidden_families: 1 << 3,
            ..KeywordsUi::default()
        };
        assert_eq!(pick_at(&scene, &ui, centre(&scene, 0)), None);
        let behind = one(
            (CameraPose::default().eye().normalize() * 5.0).to_array(),
            0,
        );
        assert_eq!(pick_at(&behind, &KeywordsUi::default(), SIZE / 2.0), None);
    }

    fn open() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_keywords(&mut app);
        testing::size_keywords(&mut app, &pr, Vec2::new(900.0, 600.0));
        (app, pr)
    }

    fn blank() -> Vec2 {
        testing::KEYWORDS_ORIGIN + Vec2::new(3.0, 3.0)
    }

    #[test]
    fn hovering_a_point_lights_its_family_and_leaving_lets_go() {
        let (mut app, pr) = open();
        let i = testing::pickable(&mut app, &pr)[0];
        let at = testing::point_screen_pos(&mut app, &pr, i);
        testing::hover_at(&mut app, &pr, at);
        let (scene, hover) = testing::keywords_runtime(&app, &pr);
        assert_eq!(hover, Some(i));
        assert!(testing::runtime(&app, &pr).cursor.is_some());
        testing::settle(&mut app);
        let lit = scene.points[i as usize].family;
        let other = scene
            .points
            .iter()
            .position(|p| p.family != lit)
            .expect("another family");
        let rt = testing::runtime(&app, &pr);
        let material = &app
            .world()
            .get::<MeshMaterial3d<StandardMaterial>>(rt.points[other])
            .unwrap()
            .0;
        assert_eq!(
            *material,
            rt.assets.family[usize::from(scene.points[other].family)].dim
        );
        testing::leave(&mut app, &pr);
        testing::settle(&mut app);
        assert_eq!(testing::keywords_runtime(&app, &pr).1, None);
        assert!(testing::runtime(&app, &pr).cursor.is_none());
    }

    #[test]
    fn hovering_empty_space_hovers_nothing() {
        let (mut app, pr) = open();
        testing::hover_at(&mut app, &pr, blank());
        assert_eq!(testing::keywords_runtime(&app, &pr).1, None);
    }

    #[test]
    fn a_hover_rebuilds_no_node() {
        let (mut app, pr) = open();
        let ids = |app: &mut App| {
            (
                testing::entities::<Label3>(app),
                testing::entities::<HeaderRegion>(app),
                testing::entities::<SceneImage>(app),
                testing::entities::<ViewButton>(app),
            )
        };
        let before = ids(&mut app);
        let i = testing::pickable(&mut app, &pr)[0];
        let at = testing::point_screen_pos(&mut app, &pr, i);
        for _ in 0..3 {
            testing::hover_at(&mut app, &pr, at);
            testing::hover_at(&mut app, &pr, blank());
        }
        testing::settle(&mut app);
        assert_eq!(ids(&mut app), before);
    }

    #[test]
    fn a_click_on_a_point_opens_its_card_and_a_click_on_empty_space_lets_go() {
        let (mut app, pr) = open();
        let i = testing::pickable(&mut app, &pr)[0];
        let at = testing::point_screen_pos(&mut app, &pr, i);
        testing::click_at(&mut app, &pr, at);
        assert_eq!(testing::keywords_ui(&app, &pr).card, Some(i));
        testing::click_at(&mut app, &pr, at);
        assert_eq!(
            testing::keywords_ui(&app, &pr).card,
            Some(i),
            "the same point stays open"
        );
        let flown = CameraPose {
            distance: 3.0,
            ..CameraPose::default()
        };
        testing::set_keywords(&mut app, &pr, |ui| {
            ui.use_case = Some(1);
            ui.camera = flown;
        });
        testing::click_at(&mut app, &pr, blank());
        let ui = testing::keywords_ui(&app, &pr);
        assert_eq!((ui.card, ui.use_case), (None, None));
        assert_eq!(
            ui.camera,
            render::view_pose(ui.view, &CameraPose::default()),
            "letting go of the focus returns the camera"
        );
        testing::set_keywords(&mut app, &pr, |ui| ui.camera = flown);
        testing::click_at(&mut app, &pr, blank());
        assert_eq!(
            testing::keywords_ui(&app, &pr).camera,
            flown,
            "with nothing focused the camera stays where the reviewer left it"
        );
    }

    #[test]
    fn a_drag_turns_the_camera_and_neither_opens_a_card_nor_lets_go() {
        let (mut app, pr) = open();
        let i = testing::pickable(&mut app, &pr)[0];
        testing::set_keywords(&mut app, &pr, |ui| {
            ui.card = Some(i);
            ui.use_case = Some(1);
        });
        let before = testing::runtime(&app, &pr).pose;
        testing::drag_by(&mut app, &pr, blank(), Vec2::new(60.0, -20.0));
        let after = testing::runtime(&app, &pr).pose;
        assert!((after.yaw - (before.yaw - 60.0 * ORBIT_PER_PIXEL)).abs() < 1e-4);
        assert!((after.pitch - (before.pitch - 20.0 * ORBIT_PER_PIXEL)).abs() < 1e-4);
        let ui = testing::keywords_ui(&app, &pr);
        assert_eq!(
            ui.camera, after,
            "the tab holds the pose once the drag ends"
        );
        assert_eq!(
            (ui.card, ui.use_case),
            (Some(i), Some(1)),
            "the drag's click is swallowed"
        );
        assert!(!testing::runtime(&app, &pr).dragging);
        // A drag that starts on a point opens nothing either.
        testing::set_keywords(&mut app, &pr, |ui| ui.card = None);
        let at = testing::point_screen_pos(&mut app, &pr, i);
        testing::drag_by(&mut app, &pr, at, Vec2::new(40.0, 0.0));
        assert_eq!(testing::keywords_ui(&app, &pr).card, None);
        // The next click is a click again.
        testing::click_at(&mut app, &pr, blank());
        assert_eq!(testing::keywords_ui(&app, &pr).use_case, None);
    }

    #[test]
    fn a_drag_in_a_flat_view_does_not_turn_the_camera() {
        let mut app = testing::app(fixture::demo(NOW));
        app.insert_resource(Motion { reduced: true });
        let flat = render::view_pose(View3D::XY, &CameraPose::default());
        let pr = testing::open_keywords_with(&mut app, fixture::keywords_map(), |ui| {
            ui.view = View3D::XY;
            ui.camera = flat;
        });
        testing::size_keywords(&mut app, &pr, Vec2::new(900.0, 600.0));
        testing::drag_by(&mut app, &pr, blank(), Vec2::new(80.0, 30.0));
        assert_eq!(testing::runtime(&app, &pr).pose, flat);
        assert_eq!(testing::keywords_ui(&app, &pr).camera, flat);
    }

    #[test]
    fn the_wheel_zooms_and_writes_the_pose_into_the_tab() {
        let (mut app, pr) = open();
        let d = testing::runtime(&app, &pr).pose.distance;
        testing::scroll_at(&mut app, &pr, blank(), 3.0);
        let closer = testing::runtime(&app, &pr).pose.distance;
        assert!(closer < d);
        assert_eq!(testing::keywords_ui(&app, &pr).camera.distance, closer);
        testing::scroll_at(&mut app, &pr, blank(), -300.0);
        assert_eq!(testing::runtime(&app, &pr).pose.distance, MAX_DISTANCE);
        testing::scroll_at(&mut app, &pr, blank(), 300.0);
        assert_eq!(testing::runtime(&app, &pr).pose.distance, MIN_DISTANCE);
    }

    #[test]
    fn turning_and_zooming_a_pose_that_is_out_of_range_brings_it_back_in() {
        let mut far = CameraPose {
            distance: 400.0,
            ..CameraPose::default()
        };
        orbit_by(&mut far, 0.1, 0.1);
        assert_eq!(far.distance, MAX_DISTANCE);
        let mut near = CameraPose {
            distance: f32::NAN,
            ..CameraPose::default()
        };
        orbit_by(&mut near, 0.1, 0.1);
        assert!((MIN_DISTANCE..=MAX_DISTANCE).contains(&near.distance));
    }

    #[test]
    fn a_scroll_during_a_drag_zooms_the_view_but_leaves_the_tabs_pose_to_the_release() {
        let (mut app, pr) = open();
        let held = testing::keywords_ui(&app, &pr).camera;
        let runtime = testing::find::<KeywordsRuntime>(&mut app, |_| true);
        app.world_mut()
            .get_mut::<KeywordsRuntime>(runtime)
            .expect("a runtime")
            .dragging = true;
        testing::scroll_at(&mut app, &pr, blank(), 3.0);
        assert!(testing::runtime(&app, &pr).pose.distance < held.distance);
        assert_eq!(testing::keywords_ui(&app, &pr).camera, held);
    }
}
