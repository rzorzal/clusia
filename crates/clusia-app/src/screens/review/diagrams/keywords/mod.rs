//! The 3D keyword view of the Diagrams tab: `KeywordsCanvas` (spawned empty by the tab while the
//! selected entry is a keyword map) is filled with the image the 3D camera draws into, the labels
//! placed over it, the header and the panels. The scene itself lives outside the UI tree: a
//! camera, two lights and one entity per point, ring and cloud, all on one render layer; the
//! window shows one review at a time, so one scene exists at a time.
//!
//! `KeywordsRuntime` on the canvas holds what changes without the tab knowing: the hovered point,
//! the camera as displayed, its move towards the pose the tab asks for (`KeywordsUi.camera`).
//! Hover never rebuilds a node; every system below compares a small key with what it applied.

use bevy::camera::{ClearColorConfig, RenderTarget};
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform, UiSystems};
use bevy::window::RequestRedraw;

use clusia_core::PrRef;
use clusia_core::diagram::EntryState;
use clusia_core::keywords::{ELSEWHERE_NAME, KeywordMap};
use clusia_diagram::keywords::{ELSEWHERE, PointShape, Scene3D, scene};

use super::Diagrams;
use super::model::picked;
use crate::fonts::UiFonts;
use crate::review_state::{CameraPose, KeywordsUi, ReviewTabs};
use crate::theme::{Swatch, Theme};
use crate::ui::kit::{Fill, Type, text};
use crate::ui::spinner::Motion;

pub mod input;
mod keys;
mod overlays;
pub mod panels;
pub mod render;

use render::{Lens, LookKey, SceneAssets, Stores, Tween};

/// The height of the band the header takes at the top of the canvas.
pub const HEADER_HEIGHT: f32 = 44.0;
/// The size the image starts at, before the first layout says how big the canvas is.
const INITIAL_SIZE: UVec2 = UVec2::new(800, 600);
/// How far above a cloud's top its name floats, in world units.
const CLOUD_LABEL_LIFT: f32 = 0.18;
/// How far above a bundle its count floats, past the bundle's radius.
const BUNDLE_LABEL_LIFT: f32 = 0.05;
/// The longest an axis label gets.
const AXIS_LABEL_MAX: usize = 60;

/// The canvas of a keyword map, empty until `fill_keywords` fills it.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct KeywordsCanvas(pub PrRef);

/// Marks every entity of the 3D scene (camera, lights, points, rings, clouds) of a review.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct KeywordsScene(pub PrRef);

/// Marks a panel the labels keep out of.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct KeepOut(pub PrRef);

/// The node that shows the 3D image; it covers the canvas and takes the pointer.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SceneImage(pub PrRef);

/// The layer that holds the labels.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LabelLayer(pub PrRef);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeywordPoint(pub u32);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeywordRing(pub u32);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeywordCloud(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    /// The name of axis 0, 1 or 2.
    Axis(u8),
    /// The name of a use case.
    Cloud(u16),
    /// The count of a bundle (the point's index).
    Bundle(u32),
}

/// A label placed on the screen from a world position.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Label3 {
    pub pr: PrRef,
    pub kind: LabelKind,
    pub world: Vec3,
}

/// The name of a use case, which is clickable; it joins `Label3`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CloudLabel {
    pub pr: PrRef,
    pub use_case: u16,
}

#[derive(Component)]
pub struct KeywordsRuntime {
    pub number: u32,
    pub key: String,
    pub map: KeywordMap,
    /// What `fingerprint` said of `map`: a refill compares it instead of the whole map.
    pub fingerprint: Fingerprint,
    pub scene: Scene3D,
    pub camera: Entity,
    pub image: Handle<Image>,
    pub assets: SceneAssets,
    pub points: Vec<Entity>,
    pub rings: Vec<Option<Entity>>,
    pub clouds: Vec<Entity>,
    /// The point under the pointer (or with the keyboard focus). Never stored in the tab: a
    /// hover must not touch anything that rebuilds.
    pub hover: Option<u32>,
    /// Where the pointer is, in canvas pixels; `None` when the hover comes from the keyboard.
    pub cursor: Option<Vec2>,
    /// The camera as displayed.
    pub pose: CameraPose,
    pub tween: Option<Tween>,
    pub lens: Lens,
    /// A drag turns the camera: the tab's pose waits until it is released.
    pub dragging: bool,
    /// The size of the image in physical pixels, and when it last changed.
    pub size: UVec2,
    pub resized_at: f64,
    /// Frames drawn since the image last changed size.
    pub frames: u32,
    pub applied: Option<LookKey>,
    pub synced: Option<(CameraPose, Lens)>,
}

/// Adds the systems of the 3D view. `fill_keywords` is in the chain of `DiagramsPlugin`, after
/// the tab has spawned the canvas; everything here runs after it.
pub struct KeywordsPlugin;

impl Plugin for KeywordsPlugin {
    fn build(&self, app: &mut App) {
        input::add_systems(app);
        app.add_plugins(overlays::OverlaysPlugin);
        app.add_systems(
            Update,
            (
                drop_scenes,
                panels::fill_header,
                panels::fit_hint,
                follow_camera,
                panels::sync_card,
                panels::ask_card_code,
            )
                .chain()
                .after(fill_keywords),
        )
        .add_systems(
            PostUpdate,
            (
                sync_camera,
                resize_targets,
                apply_looks,
                restyle,
                place_labels,
                panels::sync_tooltip,
            )
                .chain()
                .after(UiSystems::PostLayout),
        );
        if app.world().contains_resource::<GizmoConfigStore>() {
            app.add_systems(Startup, configure_gizmos)
                .add_systems(PostUpdate, draw_gizmos.after(UiSystems::PostLayout));
        }
    }
}

/// The gizmo lines of the scene go to the scene's layer only, so the window's own camera does
/// not draw them.
fn configure_gizmos(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<DefaultGizmoConfigGroup>();
    config.render_layers = bevy::camera::visibility::RenderLayers::layer(render::LAYER);
    config.line.width = render::LINE_WIDTH;
}

/// A few counts that tell one map of a version's entry from another without walking it.
type Fingerprint = (usize, usize, usize, u32, bool);

fn fingerprint(map: &KeywordMap) -> Fingerprint {
    (
        map.places.len(),
        map.words.len(),
        map.plan.families.len(),
        map.total_places(),
        map.search_cut,
    )
}

/// The name of use case `index` in `map`.
pub fn use_case_name(map: &KeywordMap, index: u16) -> String {
    if index == ELSEWHERE {
        return ELSEWHERE_NAME.to_string();
    }
    map.plan
        .use_cases
        .get(usize::from(index))
        .map_or_else(|| ELSEWHERE_NAME.to_string(), |u| u.name.clone())
}

fn axis_text(map: &KeywordMap, axis: usize) -> String {
    let a = &map.plan.axes[axis];
    let s = format!("{}: {} → {}", a.name, a.low, a.high);
    if s.chars().count() <= AXIS_LABEL_MAX {
        s
    } else {
        let mut cut: String = s.chars().take(AXIS_LABEL_MAX).collect();
        cut.push('…');
        cut
    }
}

/// Fills each `KeywordsCanvas` with the view of the map the tab selected, and again when the
/// map, its version or its entry changes. Otherwise it does nothing, so the camera, the hover
/// and the image keep their state. A card, a focus and filters mean something for one map only:
/// another version or entry starts without them.
pub(super) fn fill_keywords(
    mut commands: Commands,
    mut tabs: ResMut<ReviewTabs>,
    diagrams: Res<Diagrams>,
    theme: Res<Theme>,
    fonts: Res<UiFonts>,
    mut stores: Stores,
    canvases: Query<(Entity, &KeywordsCanvas, Option<&KeywordsRuntime>)>,
    scene_entities: Query<(Entity, &KeywordsScene)>,
) {
    let fresh = canvases.iter().any(|(_, _, rt)| rt.is_none());
    if !(fresh || diagrams.is_changed()) {
        return;
    }
    for (entity, canvas, runtime) in &canvases {
        let (Some(tab), Some(model)) = (tabs.0.get(&canvas.0), diagrams.0.get(&canvas.0)) else {
            continue;
        };
        let Some(p) = picked(model, &tab.ui) else {
            continue;
        };
        let EntryState::ReadyMap { map, .. } = &p.entry.state else {
            continue;
        };
        let key = p.entry.plan.key.clone();
        let prints = fingerprint(map);
        if runtime
            .is_some_and(|rt| rt.number == p.number && rt.key == key && rt.fingerprint == prints)
        {
            continue;
        }
        let number = p.number;
        let key_shown = key.clone();
        let map: KeywordMap = (**map).clone();
        let scene = scene(&map);
        let (pose, view) = (tab.ui.keywords.camera, tab.ui.keywords.view);
        let stale_card = tab
            .ui
            .keywords
            .card
            .is_some_and(|i| i as usize >= scene.points.len());
        for (e, s) in &scene_entities {
            if s.0 == canvas.0 {
                commands.entity(e).despawn();
            }
        }
        let lens = render::lens_of(view);
        let spawned = render::spawn_scene(
            &mut commands,
            &mut stores,
            &scene,
            &canvas.0,
            &theme.tokens,
            &pose,
            lens,
            INITIAL_SIZE,
        );
        commands.entity(entity).despawn_related::<Children>();
        let pr = canvas.0.clone();
        let image = spawned.image.clone();
        commands.entity(entity).with_children(|c| {
            spawn_children(c, &fonts, &pr, image, &scene, &map);
        });
        commands.entity(entity).insert(KeywordsRuntime {
            number,
            key,
            map,
            fingerprint: prints,
            scene,
            camera: spawned.camera,
            image: spawned.image,
            assets: spawned.assets,
            points: spawned.points,
            rings: spawned.rings,
            clouds: spawned.clouds,
            hover: None,
            cursor: None,
            pose,
            tween: None,
            lens,
            dragging: false,
            size: INITIAL_SIZE,
            resized_at: f64::NEG_INFINITY,
            frames: 0,
            applied: None,
            synced: None,
        });
        if let Some(tab) = tabs.0.get_mut(&canvas.0) {
            let ui = &mut tab.ui.keywords;
            let shown = Some((number, key_shown));
            if ui.shown.is_some() && ui.shown != shown {
                *ui = KeywordsUi {
                    view: ui.view,
                    camera: ui.camera,
                    ..KeywordsUi::default()
                };
            } else if stale_card {
                ui.card = None;
            }
            ui.shown = shown;
        }
    }
}

fn full() -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(0),
        top: px(0),
        width: percent(100),
        height: percent(100),
        ..default()
    }
}

fn spawn_children(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    image: Handle<Image>,
    scene: &Scene3D,
    map: &KeywordMap,
) {
    c.spawn((full(), ImageNode::new(image), SceneImage(pr.clone())));
    c.spawn((full(), Pickable::IGNORE, LabelLayer(pr.clone())))
        .with_children(|l| spawn_labels(l, fonts, pr, scene, map));
    c.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            width: percent(100),
            height: px(HEADER_HEIGHT),
            padding: UiRect::axes(px(20), px(0)),
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        },
        KeepOut(pr.clone()),
        panels::HeaderRegion {
            pr: pr.clone(),
            built: None,
        },
    ));
    panels::spawn_overlays(c, fonts, pr);
    panels::spawn_floating(c, fonts, pr);
}

fn label(
    l: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    marker: Label3,
    s: String,
    style: Type,
    backed: bool,
    cloud: Option<CloudLabel>,
) {
    let mut e = l.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            padding: UiRect::axes(px(5), px(1)),
            border_radius: BorderRadius::all(px(4)),
            ..default()
        },
        Visibility::Hidden,
        BackgroundColor::default(),
        Fill(if backed {
            Swatch::SceneBg
        } else {
            Swatch::Clear
        }),
        marker,
        children![(text(fonts, s, style), TextLayout::no_wrap())],
    ));
    match cloud {
        Some(cloud) => e.insert(cloud),
        None => e.insert(Pickable::IGNORE),
    };
}

fn spawn_labels(
    l: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    scene: &Scene3D,
    map: &KeywordMap,
) {
    let o = -render::SCALE;
    let ends = [
        Vec3::new(render::SCALE + 0.15, o, o),
        Vec3::new(o, render::SCALE + 0.12, o),
        Vec3::new(o, o, render::SCALE + 0.15),
    ];
    for (i, world) in ends.into_iter().enumerate() {
        label(
            l,
            fonts,
            Label3 {
                pr: pr.clone(),
                kind: LabelKind::Axis(i as u8),
                world,
            },
            axis_text(map, i),
            Type::META.size(11.0).ink(Swatch::Muted),
            true,
            None,
        );
    }
    for c in &scene.clouds {
        let top =
            render::world(c.centre) + Vec3::Y * (c.radii[1] * render::SCALE + CLOUD_LABEL_LIFT);
        label(
            l,
            fonts,
            Label3 {
                pr: pr.clone(),
                kind: LabelKind::Cloud(c.use_case),
                world: top,
            },
            use_case_name(map, c.use_case),
            Type::STRONG.size(12.0),
            false,
            Some(CloudLabel {
                pr: pr.clone(),
                use_case: c.use_case,
            }),
        );
    }
    for (i, p) in scene.points.iter().enumerate() {
        if p.shape != PointShape::Bundle {
            continue;
        }
        label(
            l,
            fonts,
            Label3 {
                pr: pr.clone(),
                kind: LabelKind::Bundle(i as u32),
                world: render::world(p.pos)
                    + Vec3::Y * (p.radius * render::SCALE + BUNDLE_LABEL_LIFT),
            },
            format!("×{}", p.places.len()),
            Type::MONO.size(10.0),
            false,
            None,
        );
    }
}

/// Removes the 3D entities of a review whose canvas is gone (another diagram, another section,
/// another tab): they are not children of the canvas, so removing it does not remove them.
fn drop_scenes(
    mut commands: Commands,
    mut removed: RemovedComponents<KeywordsCanvas>,
    canvases: Query<&KeywordsCanvas>,
    scene: Query<(Entity, &KeywordsScene)>,
) {
    if removed.read().next().is_none() {
        return;
    }
    for (entity, s) in &scene {
        if !canvases.iter().any(|c| c.0 == s.0) {
            commands.entity(entity).despawn();
        }
    }
}

/// Moves the displayed camera towards the pose the tab asks for.
fn follow_camera(
    time: Res<Time>,
    motion: Option<Res<Motion>>,
    tabs: Res<ReviewTabs>,
    mut runtimes: Query<(&KeywordsCanvas, &mut KeywordsRuntime)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let reduced = motion.is_some_and(|m| m.reduced);
    for (canvas, mut rt) in &mut runtimes {
        let Some(tab) = tabs.0.get(&canvas.0) else {
            continue;
        };
        let ui = &tab.ui.keywords;
        let rt = &mut *rt;
        let moving = render::follow(
            &mut rt.pose,
            &mut rt.tween,
            &ui.camera,
            rt.dragging,
            time.delta_secs(),
            reduced,
        );
        rt.lens = if rt.tween.is_none() {
            render::lens_of(ui.view)
        } else {
            Lens::Perspective
        };
        if moving {
            redraw.write(RequestRedraw);
        }
    }
}

/// Writes the displayed pose into the 3D camera, its projection and the rings that face it.
fn sync_camera(
    mut runtimes: Query<&mut KeywordsRuntime>,
    mut cameras: Query<(&mut Transform, &mut Projection), With<Camera3d>>,
    mut rings: Query<&mut Transform, (With<KeywordRing>, Without<Camera3d>)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for mut rt in &mut runtimes {
        let rt = &mut *rt;
        let state = (rt.pose, rt.lens);
        if rt.synced == Some(state) {
            continue;
        }
        rt.synced = Some(state);
        let transform = rt.pose.transform();
        if let Ok((mut t, mut p)) = cameras.get_mut(rt.camera) {
            *t = transform;
            *p = render::projection(&rt.pose, rt.lens);
        }
        for ring in rt.rings.iter().flatten() {
            if let Ok(mut t) = rings.get_mut(*ring) {
                t.rotation =
                    transform.rotation * Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
            }
        }
        redraw.write(RequestRedraw);
    }
}

/// Resizes the image to the node that shows it, and counts the frames drawn since.
fn resize_targets(
    time: Res<Time>,
    mut images: ResMut<Assets<Image>>,
    nodes: Query<(&SceneImage, &ComputedNode)>,
    mut runtimes: Query<(&KeywordsCanvas, &mut KeywordsRuntime)>,
    mut targets: Query<&mut RenderTarget>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let now = time.elapsed_secs_f64();
    for (canvas, mut rt) in &mut runtimes {
        let Some((_, node)) = nodes.iter().find(|(i, _)| i.0 == canvas.0) else {
            continue;
        };
        if node.size.x <= 0.0 || node.size.y <= 0.0 {
            continue;
        }
        let rt = &mut *rt;
        // The scale changes without the size when the window moves to another monitor.
        let scale = 1.0 / node.inverse_scale_factor;
        let stale = targets
            .get(rt.camera)
            .is_ok_and(|t| matches!(t, RenderTarget::Image(i) if i.scale_factor != scale));
        if stale
            && let Ok(mut target) = targets.get_mut(rt.camera)
            && let RenderTarget::Image(image) = &mut *target
        {
            image.scale_factor = scale;
            redraw.write(RequestRedraw);
        }
        match render::next_size(rt.size, node.size, now - rt.resized_at) {
            Some(size) => {
                render::resize_image(&mut images, &rt.image, size);
                rt.size = size;
                rt.resized_at = now;
                rt.frames = 0;
                redraw.write(RequestRedraw);
            }
            None if render::target_size(node.size) != rt.size => {
                // A change waits out the debounce: ask for another frame to apply it.
                redraw.write(RequestRedraw);
            }
            None => rt.frames = rt.frames.saturating_add(1),
        }
    }
}

/// Applies what the filters, the focused use case and the hover say to every point, ring and
/// cloud, when any of them changed.
fn apply_looks(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    mut runtimes: Query<(&KeywordsCanvas, &mut KeywordsRuntime)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for (canvas, mut rt) in &mut runtimes {
        let Some(tab) = tabs.0.get(&canvas.0) else {
            continue;
        };
        let ui = &tab.ui.keywords;
        let key = LookKey {
            use_case: ui.use_case,
            hidden_families: ui.hidden_families,
            hidden_use_cases: ui.hidden_use_cases,
            only_changed: ui.only_changed,
            hover: rt.hover,
        };
        if rt.applied.as_ref() == Some(&key) {
            continue;
        }
        let rt = &mut *rt;
        let hover_family = rt
            .hover
            .and_then(|i| rt.scene.points.get(i as usize))
            .map(|p| p.family);
        for (i, p) in rt.scene.points.iter().enumerate() {
            let shown = if render::visible(ui, p) {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
            let family = &rt.assets.family[usize::from(p.family).min(rt.assets.family.len() - 1)];
            let material = match (render::look(ui, p, hover_family), p.shape) {
                (render::Look::Dim, _) => &family.dim,
                (render::Look::Normal, PointShape::Bundle) => &family.bundle,
                (render::Look::Normal, _) => &family.normal,
            };
            commands
                .entity(rt.points[i])
                .insert((shown, MeshMaterial3d(material.clone())));
            if let Some(ring) = rt.rings[i] {
                commands.entity(ring).insert(shown);
            }
        }
        for (i, c) in rt.scene.clouds.iter().enumerate() {
            let shown = if render::use_case_hidden(ui, c.use_case) {
                Visibility::Hidden
            } else {
                Visibility::Inherited
            };
            let material = match ui.use_case {
                Some(u) if u == c.use_case => &rt.assets.cloud[1],
                Some(_) => &rt.assets.cloud[2],
                None => &rt.assets.cloud[0],
            };
            commands
                .entity(rt.clouds[i])
                .insert((shown, MeshMaterial3d(material.clone())));
        }
        rt.applied = Some(key);
        redraw.write(RequestRedraw);
    }
}

/// Rewrites the colours the theme sets when it changes.
fn restyle(
    theme: Res<Theme>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    runtimes: Query<&KeywordsRuntime>,
    mut cameras: Query<&mut Camera>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if !theme.is_changed() {
        return;
    }
    for rt in &runtimes {
        render::restyle_assets(&mut materials, &rt.assets, &theme.tokens);
        if let Ok(mut camera) = cameras.get_mut(rt.camera) {
            camera.clear_color = ClearColorConfig::Custom(theme.tokens.scene_bg);
        }
        redraw.write(RequestRedraw);
    }
}

/// The cube's frame and the family lines, drawn each frame the app runs (the lines are
/// immediate-mode).
fn draw_gizmos(
    mut gizmos: Gizmos,
    theme: Res<Theme>,
    tabs: Res<ReviewTabs>,
    runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
) {
    for (canvas, rt) in &runtimes {
        let Some(tab) = tabs.0.get(&canvas.0) else {
            continue;
        };
        let ui = &tab.ui.keywords;
        for (a, b, color) in render::frame_lines(&theme.tokens) {
            gizmos.line(a, b, color);
        }
        let hover_family = rt
            .hover
            .and_then(|i| rt.scene.points.get(i as usize))
            .map(|p| p.family);
        for &(a, b) in &rt.scene.lines {
            let (Some(pa), Some(pb)) = (
                rt.scene.points.get(a as usize),
                rt.scene.points.get(b as usize),
            ) else {
                continue;
            };
            if !render::visible(ui, pa) || !render::visible(ui, pb) {
                continue;
            }
            let color = theme.tokens.family[usize::from(pa.family).min(11)]
                .with_alpha(render::line_alpha(ui, pa, pb, hover_family));
            gizmos.line(render::world(pa.pos), render::world(pb.pos), color);
        }
    }
}

/// The top-left corner of a UI node on the window, and its size, in logical pixels.
fn rect_of(node: &ComputedNode, at: &UiGlobalTransform) -> Rect {
    let s = node.inverse_scale_factor;
    Rect::from_center_size(at.translation * s, node.size * s)
}

/// Puts every label where its world position projects, clamped into the canvas and out of the
/// panels; a label behind the camera is hidden, and so is one of a cloud or a bundle whose
/// anchor is off the canvas (an axis name always stays, at the edge). A bundle's count that
/// would sit on a cloud or an axis name is hidden too, so a name is never written over.
fn place_labels(
    runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
    images: Query<(&SceneImage, &ComputedNode, &UiGlobalTransform)>,
    keep: Query<(&KeepOut, &ComputedNode, &UiGlobalTransform)>,
    mut labels: Query<(&Label3, &ComputedNode, &mut Node, &mut Visibility)>,
) {
    let mut placed: Vec<(Mut<Node>, Mut<Visibility>, Option<(Vec2, Vec2)>, &Label3)> = Vec::new();
    for (label, own, node, visibility) in &mut labels {
        let spot = (|| {
            let (_, rt) = runtimes.iter().find(|(c, _)| c.0 == label.pr)?;
            let (_, canvas_node, canvas_at) = images.iter().find(|(i, _, _)| i.0 == label.pr)?;
            let canvas = rect_of(canvas_node, canvas_at);
            let size = canvas.size();
            let (screen, _) = render::project(&rt.pose, rt.lens, size, label.world)?;
            let outside = screen.cmplt(Vec2::ZERO).any() || screen.cmpgt(size).any();
            if outside && !matches!(label.kind, LabelKind::Axis(_)) {
                return None;
            }
            let panels: Vec<Rect> = keep
                .iter()
                .filter(|(k, _, _)| k.0 == label.pr)
                .map(|(_, n, at)| {
                    let r = rect_of(n, at);
                    Rect::from_corners(r.min - canvas.min, r.max - canvas.min)
                })
                .collect();
            let label_size = own.size * own.inverse_scale_factor;
            Some((
                render::clamp_label(screen, label_size, size, &panels),
                label_size,
            ))
        })();
        placed.push((node, visibility, spot, label));
    }
    // The names a bundle's count must keep clear of.
    let names: Vec<(PrRef, Rect)> = placed
        .iter()
        .filter(|(_, _, _, l)| !matches!(l.kind, LabelKind::Bundle(_)))
        .filter_map(|(_, _, spot, l)| {
            let (corner, size) = (*spot)?;
            Some((l.pr.clone(), Rect::from_corners(corner, corner + size)))
        })
        .collect();
    for (node, visibility, spot, label) in &mut placed {
        let spot = spot.filter(|(corner, size)| {
            !matches!(label.kind, LabelKind::Bundle(_))
                || !names.iter().any(|(pr, r)| {
                    let mine = Rect::from_corners(*corner, *corner + *size);
                    *pr == label.pr && !mine.intersect(r.inflate(2.0)).is_empty()
                })
        });
        let Some((corner, _)) = spot else {
            if **visibility != Visibility::Hidden {
                **visibility = Visibility::Hidden;
            }
            continue;
        };
        let (left, top) = (px(corner.x), px(corner.y));
        if node.left != left || node.top != top {
            node.left = left;
            node.top = top;
        }
        if **visibility != Visibility::Inherited {
            **visibility = Visibility::Inherited;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::review_state::{ReviewSection, View3D};
    use crate::testing::{self, NOW};
    use crate::theme::{DARK, LIGHT};
    use bevy::light::DirectionalLight;
    use clusia_core::keywords::KEYWORDS_KEY;

    fn open() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_keywords(&mut app);
        (app, pr)
    }

    fn scene_of(app: &App, pr: &PrRef) -> Scene3D {
        testing::keywords_runtime(app, pr).0
    }

    fn edit(app: &mut App, pr: &PrRef, f: impl FnOnce(&mut crate::review_state::KeywordsUi)) {
        testing::set_keywords(app, pr, f);
    }

    #[test]
    fn the_ready_map_spawns_the_canvas_and_the_whole_scene() {
        let (mut app, pr) = open();
        assert_eq!(testing::count::<KeywordsCanvas>(&mut app), 1);
        assert_eq!(
            testing::count::<super::super::DiagramCanvas>(&mut app),
            0,
            "not the box canvas"
        );
        let scene = scene_of(&app, &pr);
        assert!(scene.points.len() > 20);
        assert_eq!(testing::count::<KeywordPoint>(&mut app), scene.points.len());
        let changed = scene.points.iter().filter(|p| p.changed).count();
        assert!(changed > 0);
        assert_eq!(testing::count::<KeywordRing>(&mut app), changed);
        assert_eq!(testing::count::<KeywordCloud>(&mut app), scene.clouds.len());
        assert_eq!(testing::count::<Camera3d>(&mut app), 1);
        assert_eq!(testing::count::<DirectionalLight>(&mut app), 1);
        let rt = testing::runtime(&app, &pr);
        let target = app
            .world()
            .get::<RenderTarget>(rt.camera)
            .expect("the scene camera's target");
        assert!(matches!(target, RenderTarget::Image(_)));
    }

    #[test]
    fn leaving_the_entry_removes_the_scene_with_the_canvas() {
        let (mut app, pr) = open();
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        for count in [
            testing::count::<KeywordsCanvas>(&mut app),
            testing::count::<KeywordsScene>(&mut app),
            testing::count::<Camera3d>(&mut app),
        ] {
            assert_eq!(count, 0);
        }
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        assert_eq!(testing::count::<Camera3d>(&mut app), 1, "it comes back");
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .diagram = Some("flow".into());
        testing::settle(&mut app);
        assert_eq!(testing::count::<KeywordsScene>(&mut app), 0);
        assert_eq!(testing::count::<super::super::DiagramCanvas>(&mut app), 1);
    }

    #[test]
    fn the_scene_starts_at_the_pose_the_tab_holds() {
        let mut app = testing::app(fixture::demo(NOW));
        let pose = render::view_pose(View3D::XZ, &CameraPose::default());
        let pr = testing::open_keywords_with(&mut app, fixture::keywords_map(), |ui| {
            ui.view = View3D::XZ;
            ui.camera = pose;
        });
        let rt = testing::runtime(&app, &pr);
        assert_eq!((rt.pose, rt.lens), (pose, Lens::Orthographic));
        assert!(rt.tween.is_none(), "no move at the start");
    }

    #[test]
    fn a_new_pose_in_the_tab_moves_the_camera_and_reduced_motion_snaps() {
        let (mut app, pr) = open();
        let goal = render::view_pose(View3D::XY, &CameraPose::default());
        edit(&mut app, &pr, |ui| {
            ui.view = View3D::XY;
            ui.camera = goal;
        });
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        assert!(rt.tween.is_some() || rt.pose == goal, "moving or there");
        assert_eq!(rt.lens == Lens::Orthographic, rt.pose == goal);
        app.insert_resource(Motion { reduced: true });
        edit(&mut app, &pr, |ui| ui.camera = CameraPose::default());
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        assert_eq!(rt.pose, CameraPose::default());
        assert!(rt.tween.is_none());
    }

    #[test]
    fn the_image_follows_the_node_in_even_sizes_with_a_debounce() {
        let (mut app, pr) = open();
        testing::size_keywords(&mut app, &pr, Vec2::new(901.0, 601.0));
        let rt = testing::runtime(&app, &pr);
        assert_eq!(rt.size, UVec2::new(900, 600));
        let handle = rt.image.clone();
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&handle)
            .unwrap();
        assert_eq!(image.size(), UVec2::new(900, 600));
        testing::size_keywords(&mut app, &pr, Vec2::new(1000.0, 700.0));
        let rt = testing::runtime(&app, &pr);
        assert_eq!(
            rt.size,
            UVec2::new(900, 600),
            "too soon after the last change"
        );
        let runtime = testing::find::<KeywordsRuntime>(&mut app, |_| true);
        app.world_mut()
            .get_mut::<KeywordsRuntime>(runtime)
            .expect("a runtime")
            .resized_at -= 1.0;
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        assert_eq!(rt.size, UVec2::new(1000, 700));
        assert_eq!(rt.frames, 2, "counted since the change, once settled");
    }

    #[test]
    fn a_theme_change_restyles_the_materials_and_the_clear_colour() {
        let (mut app, pr) = open();
        testing::set_config_locally(&mut app, "appearance.theme", "dark");
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        let materials = app.world().resource::<Assets<StandardMaterial>>();
        let family = &rt.assets.family[0];
        assert_eq!(
            materials.get(&family.normal).unwrap().base_color,
            DARK.family[0]
        );
        assert_eq!(
            materials.get(&family.dim).unwrap().base_color,
            DARK.family[0].with_alpha(render::DIM_ALPHA)
        );
        assert_eq!(
            materials.get(&rt.assets.ring_material).unwrap().base_color,
            DARK.green
        );
        let camera = app.world().get::<Camera>(rt.camera).unwrap();
        assert!(matches!(
            camera.clear_color,
            ClearColorConfig::Custom(c) if c == DARK.scene_bg
        ));
        testing::set_config_locally(&mut app, "appearance.theme", "light");
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        let materials = app.world().resource::<Assets<StandardMaterial>>();
        assert_eq!(
            materials
                .get(&rt.assets.family[0].normal)
                .unwrap()
                .base_color,
            LIGHT.family[0]
        );
    }

    #[test]
    fn filters_and_focus_dim_and_hide_points_without_rebuilding_them() {
        let (mut app, pr) = open();
        let before: Vec<Entity> = testing::runtime(&app, &pr).points.clone();
        edit(&mut app, &pr, |ui| {
            ui.hidden_families = 0b1;
            ui.use_case = Some(0);
        });
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        let scene = &rt.scene;
        assert_eq!(rt.points, before, "the same entities");
        for (i, p) in scene.points.iter().enumerate() {
            let e = rt.points[i];
            let vis = *app.world().get::<Visibility>(e).unwrap();
            assert_eq!(vis == Visibility::Hidden, p.family == 0, "point {i}");
            if p.family != 0 {
                let family = &rt.assets.family[usize::from(p.family)];
                let want = if p.use_case != 0 {
                    &family.dim
                } else if p.shape == PointShape::Bundle {
                    &family.bundle
                } else {
                    &family.normal
                };
                let got = &app
                    .world()
                    .get::<MeshMaterial3d<StandardMaterial>>(e)
                    .unwrap()
                    .0;
                assert_eq!(got, want, "point {i}");
            }
        }
    }

    /// Adds a version 3 that is a copy of the shown one, so the latest version changes.
    fn step_to_a_new_version(app: &mut App, pr: &PrRef) {
        let mut diagrams = app.world_mut().resource_mut::<Diagrams>();
        let model = diagrams.0.get_mut(pr).expect("the model of the review");
        let mut next = model.loaded[&2].clone();
        next.number = 3;
        model.versions.push(next.info());
        model.loaded.insert(3, next);
        testing::settle(app);
    }

    #[test]
    fn stepping_to_another_version_resets_the_card_the_focus_and_the_filters() {
        let (mut app, pr) = open();
        edit(&mut app, &pr, |ui| {
            ui.card = Some(0);
            ui.use_case = Some(1);
            ui.hidden_families = 0b1;
            ui.hidden_use_cases = 0b10;
            ui.only_changed = true;
        });
        let kept = testing::keywords_ui(&app, &pr);
        assert_eq!(
            (
                kept.card,
                kept.use_case,
                kept.hidden_families,
                kept.only_changed
            ),
            (Some(0), Some(1), 0b1, true),
            "the same map keeps what was chosen"
        );
        assert_eq!(testing::runtime(&app, &pr).number, 2);
        step_to_a_new_version(&mut app, &pr);
        assert_eq!(testing::runtime(&app, &pr).number, 3);
        let ui = testing::keywords_ui(&app, &pr);
        assert_eq!(
            (
                ui.card,
                ui.use_case,
                ui.hidden_families,
                ui.hidden_use_cases,
                ui.only_changed
            ),
            (None, None, 0, 0, false)
        );
    }

    #[test]
    fn the_first_fill_keeps_what_the_tab_already_chose() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_keywords_with(&mut app, fixture::keywords_map(), |ui| {
            ui.use_case = Some(1);
            ui.hidden_families = 0b10;
        });
        let ui = testing::keywords_ui(&app, &pr);
        assert_eq!((ui.use_case, ui.hidden_families), (Some(1), 0b10));
    }

    #[test]
    fn every_family_hidden_leaves_an_empty_scene_that_still_works() {
        let (mut app, pr) = open();
        edit(&mut app, &pr, |ui| ui.hidden_families = 0xFFF);
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        for e in &rt.points {
            assert_eq!(
                *app.world().get::<Visibility>(*e).unwrap(),
                Visibility::Hidden
            );
        }
        edit(&mut app, &pr, |ui| ui.hidden_families = 0);
        testing::settle(&mut app);
        let rt = testing::runtime(&app, &pr);
        assert_ne!(
            *app.world().get::<Visibility>(rt.points[0]).unwrap(),
            Visibility::Hidden
        );
    }

    #[test]
    fn zero_and_exactly_four_hundred_points_both_build() {
        for (places, points) in [(0, 0), (400, 400), (450, 400)] {
            let mut app = testing::app(fixture::demo(NOW));
            let pr =
                testing::open_keywords_with(&mut app, fixture::keywords_map_of(places), |_| {});
            let scene = scene_of(&app, &pr);
            assert_eq!(scene.points.len(), points, "{places} places");
            assert_eq!(testing::count::<KeywordPoint>(&mut app), points);
            assert_eq!(scene.more, places.saturating_sub(400));
        }
    }

    #[test]
    fn a_stale_card_is_cleared_when_the_map_changes() {
        let (mut app, pr) = open();
        edit(&mut app, &pr, |ui| ui.card = Some(10_000));
        let versions = fixture::with_keywords_map(
            fixture::demo_diagrams(NOW, &pr),
            fixture::keywords_map_of(3),
        );
        testing::tell(&mut app, fixture::diagrams_tell_of(&pr, versions));
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.keywords.card, None);
        assert_eq!(scene_of(&app, &pr).points.len(), 3);
    }

    #[test]
    fn axis_labels_name_the_axes_and_stay_in_the_canvas_and_out_of_the_header() {
        let (mut app, pr) = open();
        let origin = testing::size_keywords(&mut app, &pr, Vec2::new(900.0, 600.0));
        let header = testing::find::<KeepOut>(&mut app, |k| k.0 == pr);
        testing::place_node(&mut app, header, origin, Vec2::new(900.0, HEADER_HEIGHT));
        for e in testing::entities::<Label3>(&mut app) {
            testing::place_node(&mut app, e, Vec2::ZERO, Vec2::new(120.0, 20.0));
        }
        testing::settle(&mut app);
        for needle in [
            "Layer: client → storage",
            "Moment: before send → after reply",
            "Distance: near → far",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        let mut q = app.world_mut().query::<(&Label3, &Node, &Visibility)>();
        let mut axis = 0;
        for (l, node, vis) in q.iter(app.world()) {
            if *vis == Visibility::Hidden {
                continue;
            }
            let (Val::Px(x), Val::Px(y)) = (node.left, node.top) else {
                panic!("pixel position");
            };
            assert!(x >= 0.0 && x + 120.0 <= 900.0 && y + 20.0 <= 600.0, "{l:?}");
            assert!(
                y >= HEADER_HEIGHT,
                "{:?} sits under the header at y {y}",
                l.kind
            );
            if matches!(l.kind, LabelKind::Axis(_)) {
                axis += 1;
            }
        }
        assert_eq!(axis, 3, "all three axis names stay");
    }

    #[test]
    fn a_bundle_count_never_sits_on_a_name() {
        let (mut app, pr) = open();
        testing::size_keywords(&mut app, &pr, Vec2::new(900.0, 600.0));
        for e in testing::entities::<Label3>(&mut app) {
            testing::place_node(&mut app, e, Vec2::ZERO, Vec2::new(160.0, 30.0));
        }
        testing::settle(&mut app);
        let mut q = app.world_mut().query::<(&Label3, &Node, &Visibility)>();
        let (mut names, mut counts) = (Vec::new(), Vec::new());
        for (l, node, vis) in q.iter(app.world()) {
            if *vis == Visibility::Hidden {
                continue;
            }
            let (Val::Px(x), Val::Px(y)) = (node.left, node.top) else {
                panic!("pixel position");
            };
            let rect = Rect::from_corners(Vec2::new(x, y), Vec2::new(x + 160.0, y + 30.0));
            match l.kind {
                LabelKind::Bundle(_) => counts.push(rect),
                _ => names.push(rect),
            }
        }
        assert!(!names.is_empty());
        for c in &counts {
            assert!(
                names.iter().all(|n| c.intersect(*n).is_empty()),
                "a count at {c:?} sits on a name"
            );
        }
        let mut all = app.world_mut().query::<&Label3>();
        let bundles = all
            .iter(app.world())
            .filter(|l| matches!(l.kind, LabelKind::Bundle(_)))
            .count();
        assert!(counts.len() < bundles, "the collisions hid some counts");
    }

    #[test]
    fn a_theme_change_while_a_use_case_is_focused_keeps_the_focus_and_recolours_the_clouds() {
        let (mut app, pr) = open();
        edit(&mut app, &pr, |ui| ui.use_case = Some(1));
        testing::set_config_locally(&mut app, "appearance.theme", "dark");
        testing::settle(&mut app);
        assert_eq!(testing::keywords_ui(&app, &pr).use_case, Some(1));
        let rt = testing::runtime(&app, &pr);
        let materials = app.world().resource::<Assets<StandardMaterial>>();
        let focused = materials.get(&rt.assets.cloud[1]).unwrap().base_color;
        assert_eq!(
            focused,
            DARK.cloud.with_alpha(DARK.cloud.alpha() * 13.0 / 7.0)
        );
        let cloud = rt
            .scene
            .clouds
            .iter()
            .position(|c| c.use_case == 1)
            .expect("the focused use case has a cloud");
        let shown = app
            .world()
            .get::<MeshMaterial3d<StandardMaterial>>(rt.clouds[cloud])
            .unwrap();
        assert_eq!(
            shown.0, rt.assets.cloud[1],
            "the focused cloud keeps its look"
        );
        let dim = rt
            .scene
            .points
            .iter()
            .position(|p| p.use_case != 1)
            .expect("a point elsewhere");
        let material = app
            .world()
            .get::<MeshMaterial3d<StandardMaterial>>(rt.points[dim])
            .unwrap();
        assert_eq!(
            materials.get(&material.0).unwrap().base_color.alpha(),
            render::DIM_ALPHA,
            "the others stay dimmed"
        );
    }

    #[test]
    fn resizing_the_window_again_and_again_ends_on_the_last_size() {
        let (mut app, pr) = open();
        let runtime = testing::find::<KeywordsRuntime>(&mut app, |_| true);
        for (w, h) in [
            (900.0, 600.0),
            (640.0, 480.0),
            (1280.0, 720.0),
            (301.0, 199.0),
            (1100.0, 650.0),
        ] {
            testing::size_keywords(&mut app, &pr, Vec2::new(w, h));
            app.world_mut()
                .get_mut::<KeywordsRuntime>(runtime)
                .expect("a runtime")
                .resized_at -= 1.0;
            testing::settle(&mut app);
        }
        let rt = testing::runtime(&app, &pr);
        assert_eq!(rt.size, UVec2::new(1100, 650));
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&rt.image)
            .unwrap();
        assert_eq!(image.size(), UVec2::new(1100, 650));
        assert!(testing::point_screen_pos(&mut app, &pr, 0).is_finite());
    }

    #[test]
    fn a_node_given_a_size_keeps_it_across_frames() {
        let (mut app, pr) = open();
        let image = testing::find::<SceneImage>(&mut app, |s| s.0 == pr);
        testing::size_keywords(&mut app, &pr, Vec2::new(900.0, 600.0));
        for _ in 0..5 {
            app.update();
        }
        let node = app.world().get::<ComputedNode>(image).expect("a node");
        assert_eq!(
            node.size,
            Vec2::new(900.0, 600.0),
            "if layout ever rewrites it, drive the size through `Node.width` and `Node.height` instead"
        );
    }

    #[test]
    fn use_case_names_are_the_plans_and_elsewhere_is_named() {
        let map = fixture::keywords_map();
        assert_eq!(use_case_name(&map, 0), "Token refresh");
        assert_eq!(use_case_name(&map, ELSEWHERE), ELSEWHERE_NAME);
        assert_eq!(use_case_name(&map, 99), ELSEWHERE_NAME);
        assert_eq!(KEYWORDS_KEY, "keywords");
    }

    #[test]
    fn the_target_follows_the_scale_factor_even_when_the_size_stays() {
        let (mut app, pr) = open();
        testing::size_keywords(&mut app, &pr, Vec2::new(900.0, 600.0));
        let scale = |app: &App, pr: &PrRef| {
            let rt = testing::runtime(app, pr);
            match app.world().get::<RenderTarget>(rt.camera) {
                Some(RenderTarget::Image(image)) => image.scale_factor,
                _ => panic!("an image target"),
            }
        };
        let image = testing::find::<SceneImage>(&mut app, |s| s.0 == pr);
        app.world_mut().entity_mut(image).insert(ComputedNode {
            size: Vec2::new(900.0, 600.0),
            inverse_scale_factor: 0.5,
            ..ComputedNode::DEFAULT
        });
        testing::settle(&mut app);
        assert_eq!(
            scale(&app, &pr),
            2.0,
            "a monitor swap changes only the scale"
        );
    }
}
