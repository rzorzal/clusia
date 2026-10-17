//! The 3D scene of the keyword view: a `Camera3d` that draws into an image the canvas shows, the
//! meshes and materials of points, rings and clouds, the gizmo lines, and the pure maths that
//! places labels and answers picking without a GPU.

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ClearColorConfig, ImageRenderTarget, RenderTarget, ScalingMode};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::ecs::system::SystemParam;
use bevy::light::{AmbientLight, DirectionalLight};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureFormat};

use super::{KeywordCloud, KeywordPoint, KeywordRing, KeywordsRuntime, KeywordsScene};
use crate::review_state::{CameraPose, KeywordsUi, View3D};
use crate::theme::{SCENE_LIGHT, Tokens};
use clusia_core::PrRef;
use clusia_diagram::keywords::{ELSEWHERE, Point3, PointShape, Scene3D};

/// Cube units to world units: the mockup's cube spans ±1.25.
pub const SCALE: f32 = 1.25;
pub const FOV_DEG: f32 = 40.0;
/// How far the camera sits from the target in a 2D view.
pub const FLAT_DISTANCE: f32 = 5.2;
pub const MIN_DISTANCE: f32 = 1.2;
pub const MAX_DISTANCE: f32 = 14.0;
pub const LAYER: usize = 1;
/// Seconds a camera move lasts.
pub const TWEEN_SECS: f32 = 0.25;
/// The opacity of a point that is dimmed.
pub const DIM_ALPHA: f32 = 0.15;
/// The opacity of a bundle.
pub const BUNDLE_ALPHA: f32 = 0.55;
/// Ambient light and the sun, in the units of Bevy's default exposure, from the mockup's 0.75
/// and 0.55 (the sun in lux is `0.55 · π` times the exposure's `1 / 0.001`).
pub const AMBIENT_BRIGHTNESS: f32 = 750.0;
pub const SUN_ILLUMINANCE: f32 = 1725.0;
/// The width, in pixels, of the lines.
pub const LINE_WIDTH: f32 = 1.5;

// ---- the camera

impl Default for CameraPose {
    /// The mockup's camera: at (3.4, 2.3, 3.6) looking at the origin.
    fn default() -> Self {
        let eye = Vec3::new(3.4, 2.3, 3.6);
        let distance = eye.length();
        Self {
            yaw: eye.x.atan2(eye.z),
            pitch: (eye.y / distance).asin(),
            distance,
            target: [0.0; 3],
        }
    }
}

impl CameraPose {
    pub fn eye(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::from(self.target) + self.distance * Vec3::new(cp * sy, sp, cp * cy)
    }

    /// The direction that is up on the screen; defined at every pitch, so a view from straight
    /// above has no gimbal flip (up is then `-Z` at yaw 0).
    pub fn up(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(-sp * sy, cp, -sp * cy)
    }

    pub fn transform(&self) -> Transform {
        Transform::from_translation(self.eye()).looking_at(Vec3::from(self.target), self.up())
    }
}

/// A cube position (−1..1 on each axis) in the world.
pub fn world(cube: [f32; 3]) -> Vec3 {
    Vec3::from(cube) * SCALE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    Perspective,
    Orthographic,
}

fn tan_half() -> f32 {
    (FOV_DEG.to_radians() / 2.0).tan()
}

/// The world height an orthographic camera shows at `distance`: what the perspective camera
/// shows at the target's plane, so the two views swap without a jump.
pub fn ortho_height(distance: f32) -> f32 {
    2.0 * distance * tan_half()
}

/// `world` on a canvas of `size` logical pixels, and its depth in front of the camera; `None`
/// behind the near plane. The same maths as the camera's, so labels and picking agree with
/// what the image shows.
pub fn project(pose: &CameraPose, lens: Lens, size: Vec2, world: Vec3) -> Option<(Vec2, f32)> {
    if size.x <= 0.0 || size.y <= 0.0 {
        return None;
    }
    let t = pose.transform();
    let v = world - t.translation;
    let right = t.rotation * Vec3::X;
    let up = t.rotation * Vec3::Y;
    let forward = t.rotation * Vec3::NEG_Z;
    let (x, y, z) = (v.dot(right), v.dot(up), v.dot(forward));
    let aspect = size.x / size.y;
    let (nx, ny) = match lens {
        Lens::Perspective => {
            if z <= 0.1 {
                return None;
            }
            (x / (z * tan_half() * aspect), y / (z * tan_half()))
        }
        Lens::Orthographic => {
            // The orthographic camera sees no depth, but what is behind its plane is not drawn.
            if z < 0.0 {
                return None;
            }
            let h = ortho_height(pose.distance);
            (x / (h / 2.0 * aspect), y / (h / 2.0))
        }
    };
    Some((
        Vec2::new((nx + 1.0) / 2.0 * size.x, (1.0 - ny) / 2.0 * size.y),
        z,
    ))
}

/// The radius, in pixels, of a sphere of `radius` world units at `depth`.
pub fn pixel_radius(pose: &CameraPose, lens: Lens, size: Vec2, radius: f32, depth: f32) -> f32 {
    match lens {
        Lens::Perspective => radius * size.y / (2.0 * depth * tan_half()),
        Lens::Orthographic => radius * size.y / ortho_height(pose.distance),
    }
}

pub fn lens_of(view: View3D) -> Lens {
    if view == View3D::Free {
        Lens::Perspective
    } else {
        Lens::Orthographic
    }
}

/// `pose` with its distance inside the camera's limits (a distance that is not a number goes to
/// the default's), so a pose is never beyond what zooming allows.
pub fn clamped(pose: CameraPose) -> CameraPose {
    let distance = if pose.distance.is_finite() {
        pose.distance.clamp(MIN_DISTANCE, MAX_DISTANCE)
    } else {
        CameraPose::default().distance
    };
    CameraPose { distance, ..pose }
}

fn flat(yaw: f32, pitch: f32, target: [f32; 3]) -> CameraPose {
    CameraPose {
        yaw,
        pitch,
        distance: FLAT_DISTANCE,
        target,
    }
}

fn is_flat(p: &CameraPose) -> bool {
    let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
    (near(p.pitch, 0.0) && (near(p.yaw, 0.0) || near(p.yaw, -FRAC_PI_2)))
        || near(p.pitch, FRAC_PI_2)
}

/// The pose a view switch moves the camera to. XY looks along −Z, XZ down from above (up on the
/// screen is −Z), ZY along +X from the −X side so Z grows to the right. Free restores the
/// default angles when the camera was in a flat view, and keeps the pose otherwise.
pub fn view_pose(view: View3D, pose: &CameraPose) -> CameraPose {
    match view {
        View3D::Free if is_flat(pose) => {
            let d = CameraPose::default();
            clamped(CameraPose {
                yaw: d.yaw,
                pitch: d.pitch,
                ..*pose
            })
        }
        View3D::Free => clamped(*pose),
        View3D::XY => flat(0.0, 0.0, pose.target),
        View3D::XZ => flat(0.0, FRAC_PI_2, pose.target),
        View3D::ZY => flat(-FRAC_PI_2, 0.0, pose.target),
    }
}

/// The camera of a use case focus: at the mockup's offset from the cloud's centre.
pub fn focus_pose(scene: &Scene3D, use_case: u16) -> CameraPose {
    let Some(cloud) = scene.clouds.iter().find(|c| c.use_case == use_case) else {
        return CameraPose::default();
    };
    let offset = Vec3::new(1.7, 1.1, 1.8);
    let distance = offset.length();
    CameraPose {
        yaw: offset.x.atan2(offset.z),
        pitch: (offset.y / distance).asin(),
        distance,
        target: world(cloud.centre).to_array(),
    }
}

// ---- the camera move

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tween {
    pub from: CameraPose,
    pub to: CameraPose,
    pub elapsed: f32,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp_pose(a: &CameraPose, b: &CameraPose, t: f32) -> CameraPose {
    let mut dyaw = (b.yaw - a.yaw) % TAU;
    if dyaw > PI {
        dyaw -= TAU;
    } else if dyaw < -PI {
        dyaw += TAU;
    }
    CameraPose {
        yaw: a.yaw + dyaw * t,
        pitch: lerp(a.pitch, b.pitch, t),
        distance: lerp(a.distance, b.distance, t),
        target: [
            lerp(a.target[0], b.target[0], t),
            lerp(a.target[1], b.target[1], t),
            lerp(a.target[2], b.target[2], t),
        ],
    }
}

/// Moves the displayed `pose` towards `want`. A drag in progress owns the pose. A new `want`
/// starts a move from where the camera is (a snap when motion is reduced); a move running
/// towards another pose restarts. Returns whether the camera moved or is still moving.
pub fn follow(
    pose: &mut CameraPose,
    tween: &mut Option<Tween>,
    want: &CameraPose,
    dragging: bool,
    dt: f32,
    reduced: bool,
) -> bool {
    if dragging {
        *tween = None;
        return false;
    }
    if tween.as_ref().is_some_and(|t| t.to != *want) {
        *tween = None;
    }
    if tween.is_none() && *pose != *want {
        if reduced {
            *pose = *want;
            return true;
        }
        *tween = Some(Tween {
            from: *pose,
            to: *want,
            elapsed: 0.0,
        });
        return true;
    }
    let Some(t) = tween.as_mut() else {
        return false;
    };
    t.elapsed += dt.clamp(0.0, 0.05);
    let k = (t.elapsed / TWEEN_SECS).min(1.0);
    let eased = k * (2.0 - k);
    *pose = lerp_pose(&t.from, &t.to, eased);
    if k >= 1.0 {
        *pose = t.to;
        *tween = None;
    }
    true
}

// ---- what a point looks like

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Normal,
    Dim,
}

fn bit(mask: u16, index: u16) -> bool {
    index < 16 && mask & (1 << index) != 0
}

/// Whether the use case filter hides `use_case`; the places that belong to none are never
/// hidden by it.
pub fn use_case_hidden(ui: &KeywordsUi, use_case: u16) -> bool {
    use_case != ELSEWHERE && bit(ui.hidden_use_cases, use_case)
}

/// Whether the filters leave the point on the screen.
pub fn visible(ui: &KeywordsUi, p: &Point3) -> bool {
    if bit(ui.hidden_families, p.family) || use_case_hidden(ui, p.use_case) {
        return false;
    }
    !ui.only_changed || p.changed || p.shape == PointShape::Declared
}

/// A hovered point lights its family; else a focused use case lights its own points.
pub fn look(ui: &KeywordsUi, p: &Point3, hover_family: Option<u16>) -> Look {
    match (hover_family, ui.use_case) {
        (Some(f), _) => {
            if p.family == f {
                Look::Normal
            } else {
                Look::Dim
            }
        }
        (None, Some(u)) => {
            if p.use_case == u {
                Look::Normal
            } else {
                Look::Dim
            }
        }
        (None, None) => Look::Normal,
    }
}

/// The opacity of the line between two points.
pub fn line_alpha(ui: &KeywordsUi, a: &Point3, b: &Point3, hover_family: Option<u16>) -> f32 {
    match (hover_family, ui.use_case) {
        (Some(f), _) => {
            if a.family == f {
                0.75
            } else {
                0.05
            }
        }
        (None, Some(u)) => {
            if a.use_case == u || b.use_case == u {
                0.6
            } else {
                0.03
            }
        }
        (None, None) => 0.22,
    }
}

/// What `apply_looks` last applied: it runs again only when this changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookKey {
    pub use_case: Option<u16>,
    pub hidden_families: u16,
    pub hidden_use_cases: u16,
    pub only_changed: bool,
    pub hover: Option<u32>,
}

// ---- assets

#[derive(Debug, Clone)]
pub struct FamilyMaterials {
    pub normal: Handle<StandardMaterial>,
    pub dim: Handle<StandardMaterial>,
    pub bundle: Handle<StandardMaterial>,
}

#[derive(Debug, Clone)]
pub struct SceneAssets {
    pub sphere: Handle<Mesh>,
    pub octahedron: Handle<Mesh>,
    pub ring: Handle<Mesh>,
    pub family: Vec<FamilyMaterials>,
    pub ring_material: Handle<StandardMaterial>,
    /// The cloud's material at rest, focused, and dimmed.
    pub cloud: [Handle<StandardMaterial>; 3],
}

#[derive(SystemParam)]
pub struct Stores<'w> {
    pub images: ResMut<'w, Assets<Image>>,
    pub meshes: ResMut<'w, Assets<Mesh>>,
    pub materials: ResMut<'w, Assets<StandardMaterial>>,
}

/// An eight-faced solid with flat normals: the shape of a declaration.
pub fn octahedron() -> Mesh {
    let v = [
        Vec3::X,
        Vec3::NEG_X,
        Vec3::Y,
        Vec3::NEG_Y,
        Vec3::Z,
        Vec3::NEG_Z,
    ];
    const FACES: [[usize; 3]; 8] = [
        [0, 2, 4],
        [2, 1, 4],
        [1, 3, 4],
        [3, 0, 4],
        [2, 0, 5],
        [1, 2, 5],
        [3, 1, 5],
        [0, 3, 5],
    ];
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    for f in FACES {
        let (a, b, c) = (v[f[0]], v[f[1]], v[f[2]]);
        let n = (b - a).cross(c - a).normalize();
        for p in [a, b, c] {
            positions.push(p.to_array());
            normals.push(n.to_array());
        }
    }
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_indices(Indices::U32((0..24).collect()))
}

fn lit(color: Color, mode: AlphaMode) -> StandardMaterial {
    StandardMaterial {
        base_color: color,
        perceptual_roughness: 1.0,
        alpha_mode: mode,
        ..default()
    }
}

fn unlit(color: Color) -> StandardMaterial {
    StandardMaterial {
        base_color: color,
        unlit: true,
        alpha_mode: if color.alpha() < 1.0 {
            AlphaMode::Blend
        } else {
            AlphaMode::Opaque
        },
        ..default()
    }
}

/// The cloud's three opacities: at rest the token's, focused a little more, dimmed far less.
fn cloud_colors(tokens: &Tokens) -> [Color; 3] {
    let base = tokens.cloud.alpha();
    [
        tokens.cloud,
        tokens.cloud.with_alpha(base * 13.0 / 7.0),
        tokens.cloud.with_alpha(base * 2.5 / 7.0),
    ]
}

fn family_colors(tokens: &Tokens, i: usize) -> [Color; 3] {
    let c = tokens.family[i];
    [c, c.with_alpha(DIM_ALPHA), c.with_alpha(BUNDLE_ALPHA)]
}

fn build_assets(stores: &mut Stores, tokens: &Tokens) -> SceneAssets {
    let family = (0..tokens.family.len())
        .map(|i| {
            let [normal, dim, bundle] = family_colors(tokens, i);
            FamilyMaterials {
                normal: stores.materials.add(lit(normal, AlphaMode::Opaque)),
                dim: stores.materials.add(lit(dim, AlphaMode::Blend)),
                bundle: stores.materials.add(lit(bundle, AlphaMode::Blend)),
            }
        })
        .collect();
    let [c0, c1, c2] = cloud_colors(tokens);
    SceneAssets {
        sphere: stores
            .meshes
            .add(Sphere::new(1.0).mesh().ico(3).expect("a subdivided sphere")),
        octahedron: stores.meshes.add(octahedron()),
        ring: stores.meshes.add(Torus {
            minor_radius: 0.12,
            major_radius: 1.0,
        }),
        family,
        ring_material: stores.materials.add(unlit(tokens.green)),
        cloud: [
            stores.materials.add(unlit(c0)),
            stores.materials.add(unlit(c1)),
            stores.materials.add(unlit(c2)),
        ],
    }
}

/// Rewrites every colour the theme sets: materials, the image's clear colour.
pub fn restyle_assets(materials: &mut Assets<StandardMaterial>, a: &SceneAssets, t: &Tokens) {
    let mut set = |h: &Handle<StandardMaterial>, c: Color| {
        if let Some(mut m) = materials.get_mut(h) {
            m.base_color = c;
            m.alpha_mode = if c.alpha() < 1.0 {
                AlphaMode::Blend
            } else {
                AlphaMode::Opaque
            };
        }
    };
    for (i, f) in a.family.iter().enumerate() {
        let [n, d, b] = family_colors(t, i);
        set(&f.normal, n);
        set(&f.dim, d);
        set(&f.bundle, b);
    }
    set(&a.ring_material, t.green);
    for (h, c) in a.cloud.iter().zip(cloud_colors(t)) {
        set(h, c);
    }
}

/// The image the camera draws into, at `size` physical pixels.
pub fn target_image(size: UVec2) -> Image {
    Image::new_target_texture(size.x, size.y, TextureFormat::Rgba8UnormSrgb, None)
}

pub struct Spawned {
    pub camera: Entity,
    pub image: Handle<Image>,
    pub assets: SceneAssets,
    pub points: Vec<Entity>,
    pub rings: Vec<Option<Entity>>,
    pub clouds: Vec<Entity>,
}

fn layers() -> RenderLayers {
    RenderLayers::layer(LAYER)
}

/// Spawns the camera, the lights and one entity per point, ring and cloud of `scene`.
pub fn spawn_scene(
    commands: &mut Commands,
    stores: &mut Stores,
    scene: &Scene3D,
    pr: &PrRef,
    tokens: &Tokens,
    pose: &CameraPose,
    lens: Lens,
    size: UVec2,
) -> Spawned {
    let image = stores.images.add(target_image(size));
    let assets = build_assets(stores, tokens);
    let marker = KeywordsScene(pr.clone());
    let camera = commands
        .spawn((
            Camera3d::default(),
            Camera {
                order: -1,
                clear_color: ClearColorConfig::Custom(tokens.scene_bg),
                ..default()
            },
            RenderTarget::Image(ImageRenderTarget {
                handle: image.clone(),
                scale_factor: 1.0,
            }),
            projection(pose, lens),
            Tonemapping::None,
            Msaa::Sample4,
            layers(),
            AmbientLight {
                color: SCENE_LIGHT,
                brightness: AMBIENT_BRIGHTNESS,
                ..default()
            },
            pose.transform(),
            marker.clone(),
        ))
        .id();
    commands.spawn((
        DirectionalLight {
            illuminance: SUN_ILLUMINANCE,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(3.0, 5.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
        layers(),
        marker.clone(),
    ));
    let mut points = Vec::with_capacity(scene.points.len());
    let mut rings = Vec::with_capacity(scene.points.len());
    for (i, p) in scene.points.iter().enumerate() {
        let r = p.radius * SCALE;
        let family = &assets.family[usize::from(p.family).min(assets.family.len() - 1)];
        let (mesh, material) = match p.shape {
            PointShape::Declared => (&assets.octahedron, &family.normal),
            PointShape::Used => (&assets.sphere, &family.normal),
            PointShape::Bundle => (&assets.sphere, &family.bundle),
        };
        let at = world(p.pos);
        points.push(
            commands
                .spawn((
                    Mesh3d(mesh.clone()),
                    MeshMaterial3d(material.clone()),
                    Transform::from_translation(at).with_scale(Vec3::splat(r)),
                    layers(),
                    marker.clone(),
                    KeywordPoint(i as u32),
                ))
                .id(),
        );
        rings.push(p.changed.then(|| {
            commands
                .spawn((
                    Mesh3d(assets.ring.clone()),
                    MeshMaterial3d(assets.ring_material.clone()),
                    ring_transform(at, r, pose.transform().rotation),
                    layers(),
                    marker.clone(),
                    KeywordRing(i as u32),
                ))
                .id()
        }));
    }
    let clouds = scene
        .clouds
        .iter()
        .map(|c| {
            commands
                .spawn((
                    Mesh3d(assets.sphere.clone()),
                    MeshMaterial3d(assets.cloud[0].clone()),
                    Transform::from_translation(world(c.centre))
                        .with_scale(Vec3::from(c.radii) * SCALE),
                    layers(),
                    marker.clone(),
                    KeywordCloud(c.use_case),
                ))
                .id()
        })
        .collect();
    Spawned {
        camera,
        image,
        assets,
        points,
        rings,
        clouds,
    }
}

/// The ring of a changed point faces the camera: the torus lies in the XZ plane, so it is turned
/// a quarter about X under the camera's rotation.
pub fn ring_transform(at: Vec3, point_radius: f32, camera: Quat) -> Transform {
    Transform::from_translation(at)
        .with_rotation(camera * Quat::from_rotation_x(FRAC_PI_2))
        .with_scale(Vec3::splat(point_radius * 1.6))
}

pub fn projection(pose: &CameraPose, lens: Lens) -> Projection {
    match lens {
        Lens::Perspective => Projection::Perspective(PerspectiveProjection {
            fov: FOV_DEG.to_radians(),
            ..default()
        }),
        Lens::Orthographic => Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::FixedVertical {
                viewport_height: ortho_height(pose.distance),
            },
            ..OrthographicProjection::default_3d()
        }),
    }
}

/// The axes of the cube and the grid on its floor, as line segments.
pub fn frame_lines(tokens: &Tokens) -> Vec<(Vec3, Vec3, Color)> {
    let o = -SCALE;
    let corner = Vec3::splat(o);
    let mut lines = vec![
        (corner, Vec3::new(SCALE, o, o), tokens.axis),
        (corner, Vec3::new(o, SCALE, o), tokens.axis),
        (corner, Vec3::new(o, o, SCALE), tokens.axis),
    ];
    for i in 1..=4 {
        let t = o + i as f32 * (2.0 * SCALE / 4.0);
        lines.push((Vec3::new(t, o, o), Vec3::new(t, o, SCALE), tokens.grid));
        lines.push((Vec3::new(o, o, t), Vec3::new(SCALE, o, t), tokens.grid));
    }
    lines
}

/// The size of the image for a node of `node` physical pixels: even, never under 16 px.
pub fn target_size(node: Vec2) -> UVec2 {
    let even = |v: f32| ((v.round() as u32).max(16) / 2) * 2;
    UVec2::new(even(node.x), even(node.y))
}

/// The next size of the image for a node of `node` physical pixels, or `None` to leave it: not
/// again within a tenth of a second of the last change.
pub fn next_size(have: UVec2, node: Vec2, since: f64) -> Option<UVec2> {
    let want = target_size(node);
    (want != have && since >= 0.1).then_some(want)
}

/// Resizes `image` in place.
pub fn resize_image(images: &mut Assets<Image>, handle: &Handle<Image>, size: UVec2) {
    if let Some(mut image) = images.get_mut(handle) {
        image.resize(Extent3d {
            width: size.x,
            height: size.y,
            depth_or_array_layers: 1,
        });
    }
}

/// Keeps a label of `size` centred on `centre` inside a canvas of `canvas`, out of the
/// `keep_out` rectangles; returns its top-left corner. A label that overlaps a panel moves to
/// the nearest place beside any panel where it overlaps none.
pub fn clamp_label(centre: Vec2, size: Vec2, canvas: Vec2, keep_out: &[Rect]) -> Vec2 {
    const MARGIN: f32 = 4.0;
    let half = size / 2.0;
    let low = half + Vec2::splat(MARGIN);
    let high = (canvas - half - Vec2::splat(MARGIN)).max(low);
    let start = centre.clamp(low, high);
    let clear = |c: Vec2| {
        let l = Rect::from_center_size(c, size);
        keep_out.iter().all(|r| {
            l.min.x >= r.max.x || l.max.x <= r.min.x || l.min.y >= r.max.y || l.max.y <= r.min.y
        })
    };
    if clear(start) {
        return start - half;
    }
    let beside = keep_out.iter().flat_map(|r| {
        [
            Vec2::new(start.x, r.min.y - half.y),
            Vec2::new(start.x, r.max.y + half.y),
            Vec2::new(r.min.x - half.x, start.y),
            Vec2::new(r.max.x + half.x, start.y),
        ]
    });
    let best = beside
        .filter(|o| o.cmpge(low).all() && o.cmple(high).all() && clear(*o))
        .min_by(|a, b| a.distance(start).total_cmp(&b.distance(start)));
    best.unwrap_or(start) - half
}

pub fn is_ready(rt: &KeywordsRuntime) -> bool {
    rt.frames >= 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_diagram::keywords::Cloud;

    const SIZE: Vec2 = Vec2::new(800.0, 600.0);

    fn point(shape: PointShape, family: u16, use_case: u16, changed: bool) -> Point3 {
        Point3 {
            places: vec![0],
            pos: [0.0; 3],
            shape,
            family,
            use_case,
            changed,
            radius: 0.028,
        }
    }

    fn near(a: Vec2, b: Vec2) -> bool {
        a.distance(b) < 0.01
    }

    #[test]
    fn the_default_pose_is_the_mockups_camera() {
        let pose = CameraPose::default();
        assert!(pose.eye().distance(Vec3::new(3.4, 2.3, 3.6)) < 1e-4);
        assert_eq!(pose.target, [0.0; 3]);
        assert!((pose.distance - 5.4599).abs() < 1e-3);
    }

    #[test]
    fn the_target_is_the_middle_of_the_canvas_at_the_distance_of_the_camera() {
        let pose = CameraPose::default();
        for lens in [Lens::Perspective, Lens::Orthographic] {
            let (at, depth) = project(&pose, lens, SIZE, Vec3::ZERO).expect("in front");
            assert!(near(at, SIZE / 2.0), "{lens:?}: {at}");
            assert!((depth - pose.distance).abs() < 1e-3);
        }
    }

    #[test]
    fn a_point_behind_the_camera_has_no_place_on_the_canvas() {
        let pose = CameraPose::default();
        assert_eq!(
            project(&pose, Lens::Perspective, SIZE, Vec3::new(6.8, 4.6, 7.2)),
            None
        );
        assert_eq!(
            project(&pose, Lens::Perspective, Vec2::ZERO, Vec3::ZERO),
            None
        );
    }

    #[test]
    fn the_flat_views_show_the_planes_their_names_say() {
        let at = |view: View3D, w: Vec3| {
            let pose = view_pose(view, &CameraPose::default());
            project(&pose, Lens::Orthographic, SIZE, w)
                .expect("shown")
                .0
        };
        let origin = SIZE / 2.0;
        // X × Y: x grows to the right, y grows up (screen y grows down).
        let p = at(View3D::XY, Vec3::new(1.0, 1.0, 0.0));
        assert!(p.x > origin.x && p.y < origin.y);
        // X × Z from above: x to the right, z grows downwards.
        let p = at(View3D::XZ, Vec3::new(1.0, 0.0, 1.0));
        assert!(p.x > origin.x && p.y > origin.y);
        // Z × Y: z grows to the right, y up.
        let p = at(View3D::ZY, Vec3::new(0.0, 1.0, 1.0));
        assert!(p.x > origin.x && p.y < origin.y);
        // Depth does not move a point in a flat view.
        let a = at(View3D::XY, Vec3::new(0.5, 0.2, -1.0));
        let b = at(View3D::XY, Vec3::new(0.5, 0.2, 1.0));
        assert!(near(a, b));
    }

    #[test]
    fn both_lenses_agree_on_the_plane_through_the_target() {
        let pose = view_pose(View3D::XY, &CameraPose::default());
        let w = Vec3::new(0.7, -0.4, 0.0);
        let p = project(&pose, Lens::Perspective, SIZE, w).unwrap().0;
        let o = project(&pose, Lens::Orthographic, SIZE, w).unwrap().0;
        assert!(near(p, o), "{p} {o}");
    }

    #[test]
    fn the_view_from_straight_above_is_not_flipped() {
        let top = view_pose(View3D::XZ, &CameraPose::default());
        assert!(top.up().distance(Vec3::NEG_Z) < 1e-5);
        assert!(top.eye().distance(Vec3::new(0.0, FLAT_DISTANCE, 0.0)) < 1e-4);
        let zy = view_pose(View3D::ZY, &CameraPose::default());
        assert!(zy.eye().distance(Vec3::new(-FLAT_DISTANCE, 0.0, 0.0)) < 1e-4);
    }

    #[test]
    fn a_view_switch_keeps_the_target_and_free_restores_the_angles() {
        let moved = CameraPose {
            target: [0.4, 0.1, -0.3],
            distance: 3.0,
            ..CameraPose::default()
        };
        let flat = view_pose(View3D::XY, &moved);
        assert_eq!(
            (flat.target, flat.distance),
            ([0.4, 0.1, -0.3], FLAT_DISTANCE)
        );
        let back = view_pose(View3D::Free, &flat);
        assert_eq!(
            (back.yaw, back.pitch),
            (CameraPose::default().yaw, CameraPose::default().pitch)
        );
        assert_eq!(back.target, [0.4, 0.1, -0.3]);
        assert_eq!(
            view_pose(View3D::Free, &moved),
            moved,
            "an orbit is left alone"
        );
    }

    #[test]
    fn a_focus_looks_at_the_cloud_from_the_mockups_offset() {
        let scene = Scene3D {
            points: vec![],
            lines: vec![],
            clouds: vec![Cloud {
                use_case: 2,
                centre: [0.4, -0.2, 0.6],
                radii: [0.3; 3],
                places: 5,
            }],
            more: 0,
        };
        let pose = focus_pose(&scene, 2);
        assert!(
            pose.eye()
                .distance(Vec3::new(0.5, -0.25, 0.75) + Vec3::new(1.7, 1.1, 1.8))
                < 1e-4
        );
        assert_eq!(
            focus_pose(&scene, 9),
            CameraPose::default(),
            "no such cloud"
        );
    }

    #[test]
    fn a_sphere_looks_smaller_from_further_away() {
        let pose = CameraPose::default();
        let near = pixel_radius(&pose, Lens::Perspective, SIZE, 0.05, 3.0);
        let far = pixel_radius(&pose, Lens::Perspective, SIZE, 0.05, 6.0);
        assert!((near / far - 2.0).abs() < 1e-4);
        let flat = pixel_radius(&pose, Lens::Orthographic, SIZE, 0.05, 3.0);
        assert!((flat - 0.05 * 600.0 / ortho_height(pose.distance)).abs() < 1e-4);
    }

    #[test]
    fn the_camera_moves_in_steps_and_ends_on_the_goal() {
        let (mut pose, mut tween) = (CameraPose::default(), None);
        let goal = view_pose(View3D::XY, &pose);
        assert!(
            follow(&mut pose, &mut tween, &goal, false, 0.0, false),
            "starts"
        );
        assert_eq!(pose, CameraPose::default(), "no jump on the first frame");
        let mut last = pose.distance;
        let mut steps = 0;
        while tween.is_some() {
            assert!(follow(&mut pose, &mut tween, &goal, false, 0.05, false));
            steps += 1;
            assert!(steps < 20, "it ends");
            assert!(pose.distance != last || tween.is_none());
            last = pose.distance;
        }
        assert_eq!(pose, goal);
        assert!(
            !follow(&mut pose, &mut tween, &goal, false, 0.05, false),
            "idle at rest"
        );
        assert!(
            (4..=6).contains(&steps),
            "a quarter of a second: {steps} steps"
        );
    }

    #[test]
    fn a_long_idle_frame_does_not_throw_the_camera() {
        let (mut pose, mut tween) = (CameraPose::default(), None);
        let goal = view_pose(View3D::XY, &pose);
        follow(&mut pose, &mut tween, &goal, false, 0.0, false);
        follow(&mut pose, &mut tween, &goal, false, 5.0, false);
        assert!(tween.is_some(), "five seconds count as one frame");
    }

    #[test]
    fn reduced_motion_snaps_and_a_drag_owns_the_pose() {
        let (mut pose, mut tween) = (CameraPose::default(), None);
        let goal = view_pose(View3D::XZ, &pose);
        assert!(follow(&mut pose, &mut tween, &goal, false, 0.0, true));
        assert_eq!((pose, tween), (goal, None));
        let (mut pose, mut tween) = (CameraPose::default(), None);
        assert!(!follow(&mut pose, &mut tween, &goal, true, 0.05, false));
        assert_eq!((pose, tween), (CameraPose::default(), None));
    }

    #[test]
    fn a_new_goal_restarts_the_move_from_where_the_camera_is() {
        let (mut pose, mut tween) = (CameraPose::default(), None);
        let a = view_pose(View3D::XY, &pose);
        follow(&mut pose, &mut tween, &a, false, 0.0, false);
        follow(&mut pose, &mut tween, &a, false, 0.05, false);
        let here = pose;
        let b = view_pose(View3D::ZY, &CameraPose::default());
        follow(&mut pose, &mut tween, &b, false, 0.0, false);
        let t = tween.expect("moving");
        assert_eq!((t.from, t.to), (here, b));
    }

    #[test]
    fn the_yaw_takes_the_short_way_round() {
        let a = CameraPose {
            yaw: 3.0,
            ..CameraPose::default()
        };
        let b = CameraPose {
            yaw: -3.0,
            ..CameraPose::default()
        };
        let mid = lerp_pose(&a, &b, 0.5);
        assert!(
            mid.yaw.abs() > 3.0 || (mid.yaw.abs() - PI).abs() < 0.2,
            "{}",
            mid.yaw
        );
    }

    #[test]
    fn filters_hide_by_family_use_case_and_change() {
        let mut ui = KeywordsUi::default();
        let used = point(PointShape::Used, 1, 2, false);
        assert!(visible(&ui, &used));
        ui.hidden_families = 0b10;
        assert!(!visible(&ui, &used));
        ui.hidden_families = 0;
        ui.hidden_use_cases = 0b100;
        assert!(!visible(&ui, &used));
        assert!(
            visible(&ui, &point(PointShape::Used, 1, ELSEWHERE, false)),
            "never hidden"
        );
        ui.hidden_use_cases = 0;
        ui.only_changed = true;
        assert!(!visible(&ui, &used));
        assert!(visible(&ui, &point(PointShape::Used, 1, 2, true)));
        assert!(visible(&ui, &point(PointShape::Declared, 1, 2, false)));
        assert!(!visible(&ui, &point(PointShape::Bundle, 1, 2, false)));
    }

    #[test]
    fn hover_lights_a_family_and_a_focus_lights_a_use_case() {
        let mut ui = KeywordsUi::default();
        let p = point(PointShape::Used, 1, 2, false);
        assert_eq!(look(&ui, &p, None), Look::Normal);
        assert_eq!(look(&ui, &p, Some(1)), Look::Normal);
        assert_eq!(look(&ui, &p, Some(0)), Look::Dim);
        ui.use_case = Some(2);
        assert_eq!(look(&ui, &p, None), Look::Normal);
        ui.use_case = Some(3);
        assert_eq!(look(&ui, &p, None), Look::Dim);
        assert_eq!(
            look(&ui, &p, Some(1)),
            Look::Normal,
            "hover wins over the focus"
        );
    }

    #[test]
    fn lines_follow_the_mockups_opacities() {
        let mut ui = KeywordsUi::default();
        let a = point(PointShape::Declared, 1, 2, false);
        let b = point(PointShape::Used, 1, 3, false);
        assert_eq!(line_alpha(&ui, &a, &b, None), 0.22);
        assert_eq!(line_alpha(&ui, &a, &b, Some(1)), 0.75);
        assert_eq!(line_alpha(&ui, &a, &b, Some(0)), 0.05);
        ui.use_case = Some(3);
        assert_eq!(line_alpha(&ui, &a, &b, None), 0.6);
        ui.use_case = Some(0);
        assert_eq!(line_alpha(&ui, &a, &b, None), 0.03);
    }

    #[test]
    fn the_octahedron_has_eight_flat_unit_faces() {
        let mesh = octahedron();
        assert_eq!(mesh.count_vertices(), 24);
        let Some(bevy::mesh::VertexAttributeValues::Float32x3(normals)) =
            mesh.attribute(Mesh::ATTRIBUTE_NORMAL)
        else {
            panic!("normals");
        };
        for n in normals {
            assert!((Vec3::from(*n).length() - 1.0).abs() < 1e-5);
        }
        let Some(bevy::mesh::VertexAttributeValues::Float32x3(positions)) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        else {
            panic!("positions");
        };
        for (p, n) in positions.iter().zip(normals) {
            assert!(
                Vec3::from(*p).dot(Vec3::from(*n)) > 0.5,
                "faces point outwards"
            );
        }
    }

    #[test]
    fn the_frame_is_three_axes_from_one_corner_and_a_floor_grid() {
        let tokens = crate::theme::LIGHT;
        let lines = frame_lines(&tokens);
        assert_eq!(lines.len(), 3 + 8);
        let corner = Vec3::splat(-SCALE);
        for (i, expect) in [Vec3::X, Vec3::Y, Vec3::Z].into_iter().enumerate() {
            assert_eq!(lines[i].0, corner);
            assert_eq!(lines[i].1, corner + expect * 2.0 * SCALE);
            assert_eq!(lines[i].2, tokens.axis);
        }
        assert!(
            lines[3..]
                .iter()
                .all(|l| l.2 == tokens.grid && l.0.y == -SCALE && l.1.y == -SCALE)
        );
    }

    #[test]
    fn the_ring_of_a_changed_point_faces_the_camera() {
        let camera = CameraPose::default().transform().rotation;
        let ring = ring_transform(Vec3::ONE, 0.05, camera);
        assert_eq!(ring.translation, Vec3::ONE);
        assert!((ring.scale.x - 0.08).abs() < 1e-6);
        // The torus's own axis (Y) ends up along the camera's viewing direction.
        let axis = ring.rotation * Vec3::Y;
        assert!(axis.dot(camera * Vec3::NEG_Z).abs() > 0.999);
    }

    #[test]
    fn the_size_of_the_image_is_even_never_tiny_and_debounced() {
        assert_eq!(target_size(Vec2::new(901.0, 601.0)), UVec2::new(900, 600));
        assert_eq!(target_size(Vec2::new(3.0, 0.0)), UVec2::new(16, 16));
        let have = UVec2::new(800, 600);
        assert_eq!(next_size(have, Vec2::new(800.0, 600.0), 5.0), None, "same");
        assert_eq!(
            next_size(have, Vec2::new(1000.0, 600.0), 0.05),
            None,
            "too soon"
        );
        assert_eq!(
            next_size(have, Vec2::new(1000.0, 600.0), 0.2),
            Some(UVec2::new(1000, 600))
        );
        assert!(next_size(have, Vec2::new(1000.0, 600.0), f64::INFINITY).is_some());
    }

    #[test]
    fn a_label_stays_in_the_canvas() {
        let size = Vec2::new(100.0, 20.0);
        let canvas = Vec2::new(800.0, 600.0);
        let at = |c: Vec2| clamp_label(c, size, canvas, &[]);
        assert_eq!(
            at(Vec2::new(400.0, 300.0)),
            Vec2::new(350.0, 290.0),
            "centred on its anchor"
        );
        let corner = at(Vec2::new(-50.0, -50.0));
        assert!(corner.x >= 0.0 && corner.y >= 0.0);
        let far = at(Vec2::new(5000.0, 5000.0));
        assert!(far.x + 100.0 <= 800.0 && far.y + 20.0 <= 600.0);
    }

    #[test]
    fn a_label_is_pushed_out_of_the_panels() {
        let size = Vec2::new(100.0, 20.0);
        let canvas = Vec2::new(800.0, 600.0);
        let header = Rect::from_corners(Vec2::ZERO, Vec2::new(800.0, 44.0));
        let legend = Rect::from_corners(Vec2::new(0.0, 540.0), Vec2::new(400.0, 600.0));
        let filters = Rect::from_corners(Vec2::new(588.0, 52.0), Vec2::new(784.0, 300.0));
        let panels = [header, legend, filters];
        let clear = |corner: Vec2| {
            let l = Rect::from_corners(corner, corner + size);
            panels.iter().all(|p| {
                l.min.x >= p.max.x || l.max.x <= p.min.x || l.min.y >= p.max.y || l.max.y <= p.min.y
            })
        };
        for anchor in [
            Vec2::new(300.0, 20.0),
            Vec2::new(200.0, 560.0),
            Vec2::new(650.0, 150.0),
            Vec2::new(10.0, 10.0),
        ] {
            let corner = clamp_label(anchor, size, canvas, &panels);
            assert!(clear(corner), "{anchor} ended at {corner}");
            assert!(corner.x >= 0.0 && corner.y >= 0.0);
            assert!(corner.x + size.x <= 800.0 && corner.y + size.y <= 600.0);
        }
        let free = clamp_label(Vec2::new(300.0, 300.0), size, canvas, &panels);
        assert_eq!(
            free,
            Vec2::new(250.0, 290.0),
            "a label in the clear does not move"
        );
    }

    #[test]
    fn a_point_behind_an_orthographic_camera_is_not_on_the_screen() {
        let pose = view_pose(View3D::XY, &CameraPose::default());
        let ahead = Vec3::new(0.0, 0.0, 0.0);
        let behind = pose.eye() + (pose.eye() - Vec3::from(pose.target)).normalize() * 2.0;
        assert!(project(&pose, Lens::Orthographic, SIZE, ahead).is_some());
        assert_eq!(project(&pose, Lens::Orthographic, SIZE, behind), None);
    }

    #[test]
    fn a_view_never_holds_a_distance_outside_the_camera_limits() {
        for distance in [100.0, 0.01, f32::NAN, f32::INFINITY] {
            let pose = CameraPose {
                distance,
                ..CameraPose::default()
            };
            let free = view_pose(View3D::Free, &pose);
            assert!(
                (MIN_DISTANCE..=MAX_DISTANCE).contains(&free.distance),
                "{distance} gave {}",
                free.distance
            );
        }
    }
}
