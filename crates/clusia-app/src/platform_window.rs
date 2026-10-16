//! Talking to macOS about the window's app: the Dock and ⌘-Tab icon, and coming to the front.
//! Tests insert `Foregrounds`, which counts the requests instead, and `ReduceMotionSource`, which
//! stands in for the system's Reduce Motion setting.

use std::path::{Path, PathBuf};

use bevy::ecs::system::NonSendMarker;
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, WindowFocused, WindowPosition};
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSImage, NSScreen, NSWorkspace};
use objc2_foundation::{NSNumber, NSString};

use crate::window_frame::{Display, Rect, Remembered};

/// When present, requests to come to the front are counted here instead of made (tests).
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct Foregrounds(pub u32);

/// `Contents/Resources/Clusia.icns` of the bundle this executable lives in, else the icon in
/// the source tree (running from `target/`).
pub fn icon_file(exe: &Path, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let bundled = exe.parent()?.parent()?.join("Resources/Clusia.icns");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/assets/brand/Clusia.icns");
    [bundled, source].into_iter().find(|p| exists(p))
}

/// Shows the Clúsia icon in the Dock and ⌘-Tab: this process is not the bundle's main
/// executable, so macOS would otherwise show a generic one.
fn set_icon(_main_thread: NonSendMarker) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(file) = std::env::current_exe()
        .ok()
        .and_then(|exe| icon_file(&exe, |p| p.is_file()))
    else {
        tracing::warn!("no Clúsia icon file found");
        return;
    };
    let image = NSImage::initWithContentsOfFile(
        NSImage::alloc(),
        &NSString::from_str(&file.to_string_lossy()),
    );
    match image {
        // SAFETY: the image is a valid NSImage and this runs on the main thread (`mtm`).
        Some(image) => unsafe {
            NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
        },
        None => tracing::warn!(file = %file.display(), "cannot read the icon"),
    }
}

/// Makes this app the active one, so the window comes in front of whatever the user is in.
fn activate(_main_thread: NonSendMarker) {
    if let Some(mtm) = MainThreadMarker::new() {
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    }
}

/// Counts the request when `Foregrounds` exists, otherwise brings the app forward.
pub fn bring_forward(recorder: Option<&mut Foregrounds>, main_thread: NonSendMarker) {
    match recorder {
        Some(count) => count.0 += 1,
        None => activate(main_thread),
    }
}

/// How this process becomes a regular app (Dock icon, app menu, ⌘-Tab). The bundle is an agent
/// app (`LSUIElement`) for the tray's sake, so the window process opts in at startup. Tests and
/// demo runs insert a stand-in.
#[derive(Resource, Clone, Copy)]
pub struct DockSource(pub fn());

impl Default for DockSource {
    fn default() -> Self {
        Self(join_dock)
    }
}

fn join_dock() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    quit_through_the_window(&app);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

/// Points the app menu's Quit (⌘Q) at the key window's close, so it takes the window's own close
/// path, which holds the window while a comment is being written. Left to itself, `terminate:`
/// would end the process past that check.
fn quit_through_the_window(app: &NSApplication) {
    let Some(app_menu) = app
        .mainMenu()
        .and_then(|bar| bar.itemAtIndex(0))
        .and_then(|item| item.submenu())
    else {
        return;
    };
    for item in app_menu.itemArray() {
        if item.keyEquivalent().to_string() == "q" {
            // SAFETY: `performClose:` takes the sender and is understood by NSWindow, which the
            // responder chain reaches with a nil target.
            unsafe {
                item.setTarget(None);
                item.setAction(Some(objc2::sel!(performClose:)));
            }
        }
    }
}

/// Makes the window process a regular app once, at startup.
fn show_in_dock(source: Res<DockSource>, _main_thread: NonSendMarker) {
    (source.0)();
}

/// Where the system's Reduce Motion setting is read from.
#[derive(Resource, Clone, Copy)]
pub struct ReduceMotionSource(pub fn() -> bool);

impl Default for ReduceMotionSource {
    fn default() -> Self {
        Self(system_reduce_motion)
    }
}

fn system_reduce_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

/// Reads Reduce Motion into `Motion` at startup and whenever a window gains focus (the setting
/// is changed in System Settings, so the app is in the background when it changes).
fn follow_reduce_motion(
    source: Res<ReduceMotionSource>,
    mut motion: ResMut<crate::ui::spinner::Motion>,
    mut focused: MessageReader<WindowFocused>,
    mut first: Local<bool>,
) {
    let regained = focused.read().any(|f| f.focused);
    if !*first || regained {
        *first = true;
        let reduced = (source.0)();
        if motion.reduced != reduced {
            motion.reduced = reduced;
        }
    }
}

/// The connected displays, the primary one first. Empty off the main thread.
pub fn displays() -> Vec<Display> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Vec::new();
    };
    let screens = NSScreen::screens(mtm);
    // Cocoa measures from the bottom-left of the primary display; the frames here from its top-left.
    let Some(primary) = screens.iter().next() else {
        return Vec::new();
    };
    let desktop_height = primary.frame().size.height;
    screens
        .iter()
        .map(|screen| {
            let visible = screen.visibleFrame();
            let frame = screen.frame();
            let id = screen
                .deviceDescription()
                .objectForKey(&NSString::from_str("NSScreenNumber"))
                .and_then(|n| n.downcast::<NSNumber>().ok())
                .map(|n| n.unsignedIntValue().to_string())
                .unwrap_or_else(|| {
                    format!(
                        "{}x{}@{},{}",
                        frame.size.width, frame.size.height, frame.origin.x, frame.origin.y
                    )
                });
            Display {
                id,
                visible: Rect::new(
                    visible.origin.x,
                    desktop_height - (visible.origin.y + visible.size.height),
                    visible.size.width,
                    visible.size.height,
                ),
            }
        })
        .collect()
}

/// Where the displays are read from.
#[derive(Resource, Clone, Copy)]
pub struct DisplaySource(pub fn() -> Vec<Display>);

impl Default for DisplaySource {
    fn default() -> Self {
        Self(displays)
    }
}

/// How long the window must hold still before its frame is written down.
const FRAME_SETTLE_SECS: f32 = 0.5;

/// Remembers where the user leaves the window. Without this resource (demos, screenshots,
/// tests) nothing is written.
#[derive(Resource)]
pub struct FrameMemory {
    file: std::path::PathBuf,
    remembered: Remembered,
    /// A frame not yet written, and the time it was first seen.
    pending: Option<(Rect, f32)>,
    /// The frame in the file now: a window that stays put is not written again.
    last_saved: Option<Rect>,
}

impl FrameMemory {
    pub fn new(file: std::path::PathBuf) -> Self {
        Self {
            remembered: Remembered::load(&file),
            file,
            pending: None,
            last_saved: None,
        }
    }

    /// The remembered frames this window starts from.
    pub fn remembered(&self) -> &Remembered {
        &self.remembered
    }
}

/// The frame of a window at physical `position` with logical `size`, in points.
pub fn frame_of(position: IVec2, size: Vec2, scale: f32) -> Rect {
    let scale = f64::from(scale);
    Rect::new(
        f64::from(position.x) / scale,
        f64::from(position.y) / scale,
        f64::from(size.x),
        f64::from(size.y),
    )
}

/// Writes the window's frame down once it has stopped moving or resizing.
fn remember_frame(
    _main_thread: NonSendMarker,
    time: Res<Time>,
    source: Res<DisplaySource>,
    windows: Query<&Window, With<PrimaryWindow>>,
    memory: Option<ResMut<FrameMemory>>,
) {
    let (Some(mut memory), Ok(window)) = (memory, windows.single()) else {
        return;
    };
    let now = time.elapsed_secs();
    if let WindowPosition::At(at) = window.position {
        let size = Vec2::new(window.resolution.width(), window.resolution.height());
        let frame = frame_of(at, size, window.resolution.scale_factor());
        if memory.last_saved == Some(frame) {
            memory.pending = None;
        } else if memory.pending.map(|(f, _)| f) != Some(frame) {
            memory.pending = Some((frame, now));
        }
    }
    let Some((frame, since)) = memory.pending else {
        return;
    };
    if now - since < FRAME_SETTLE_SECS {
        return;
    }
    memory.pending = None;
    memory.last_saved = Some(frame);
    let connected = (source.0)();
    memory.remembered.record(frame, &connected);
    if let Err(e) = memory.remembered.save(&memory.file) {
        tracing::warn!(file = %memory.file.display(), "cannot save the window frame: {e}");
    }
}

/// The frame the window should take once it exists, set once (`None`: leave it where the system
/// put it).
#[derive(Resource, Debug)]
pub struct PlaceWindow(pub Option<Rect>);

/// Moves the window to its remembered frame. The position is in device pixels, so it is
/// scaled by the window's own scale factor: that is the one the system reads it back with.
fn place_window(
    mut place: ResMut<PlaceWindow>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    let (Some(frame), Ok(mut window)) = (place.0, windows.single_mut()) else {
        return;
    };
    place.0 = None;
    let scale = f64::from(window.resolution.scale_factor());
    window.position = WindowPosition::At(IVec2::new(
        (frame.x * scale).round() as i32,
        (frame.y * scale).round() as i32,
    ));
}

/// Registers the icon, the Reduce Motion follower and the window frame memory.
pub struct PlatformPlugin;

impl Plugin for PlatformPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReduceMotionSource>()
            .init_resource::<DisplaySource>()
            .init_resource::<DockSource>()
            .add_systems(Startup, (show_in_dock, set_icon).chain())
            .add_systems(
                Update,
                (
                    follow_reduce_motion,
                    place_window.run_if(resource_exists::<PlaceWindow>),
                    remember_frame,
                )
                    .chain(),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::spinner::Motion;

    fn motion_app(source: fn() -> bool) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<WindowFocused>()
            .init_resource::<Motion>()
            .insert_resource(ReduceMotionSource(source))
            .add_systems(Update, follow_reduce_motion);
        app
    }

    fn focus(app: &mut App, focused: bool) {
        let window = app.world_mut().spawn_empty().id();
        app.world_mut()
            .write_message(WindowFocused { window, focused });
    }

    #[test]
    fn reduce_motion_is_read_at_startup() {
        let mut app = motion_app(|| true);
        app.update();
        assert!(app.world().resource::<Motion>().reduced);
        let mut app = motion_app(|| false);
        app.update();
        assert!(!app.world().resource::<Motion>().reduced);
    }

    #[test]
    fn reduce_motion_is_read_again_when_a_window_gains_focus_only() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static ON: AtomicBool = AtomicBool::new(false);
        let mut app = motion_app(|| ON.load(Ordering::SeqCst));
        app.update();
        ON.store(true, Ordering::SeqCst);
        app.update();
        assert!(
            !app.world().resource::<Motion>().reduced,
            "nothing asked yet"
        );
        focus(&mut app, false);
        app.update();
        assert!(
            !app.world().resource::<Motion>().reduced,
            "losing focus is not a reason"
        );
        focus(&mut app, true);
        app.update();
        assert!(app.world().resource::<Motion>().reduced);
    }

    static DOCK_JOINS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    #[test]
    fn the_window_joins_the_dock_once_at_startup() {
        let mut app = App::new();
        app.insert_resource(DockSource(|| {
            DOCK_JOINS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }))
        .add_systems(Startup, show_in_dock);
        app.update();
        app.update();
        assert_eq!(DOCK_JOINS.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn the_bundle_icon_wins() {
        let exe = Path::new("/Applications/Clusia.app/Contents/MacOS/clusia-app");
        assert_eq!(
            icon_file(exe, |_| true),
            Some(PathBuf::from(
                "/Applications/Clusia.app/Contents/Resources/Clusia.icns"
            ))
        );
    }

    #[test]
    fn a_build_folder_uses_the_icon_in_the_source_tree() {
        let exe = Path::new("/work/target/debug/clusia-app");
        let found = icon_file(exe, |p| p.to_string_lossy().contains("docs/assets/brand")).unwrap();
        assert!(found.ends_with("docs/assets/brand/Clusia.icns"));
        assert_eq!(icon_file(exe, |_| false), None);
    }

    #[test]
    fn the_source_tree_icon_exists() {
        let exe = Path::new("/work/target/debug/clusia-app");
        assert!(icon_file(exe, |p| p.is_file()).is_some());
    }

    fn laptop() -> Vec<Display> {
        vec![Display {
            id: "laptop".into(),
            visible: Rect::new(0.0, 25.0, 1512.0, 920.0),
        }]
    }

    fn frame_app(file: &Path, window: Window) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(300),
            ))
            .insert_resource(DisplaySource(laptop))
            .insert_resource(FrameMemory::new(file.to_path_buf()))
            .add_systems(Update, remember_frame);
        let e = app.world_mut().spawn((window, PrimaryWindow)).id();
        (app, e)
    }

    fn window_at(x: i32, y: i32, w: u32, h: u32) -> Window {
        Window {
            position: WindowPosition::At(IVec2::new(x, y)),
            resolution: (w, h).into(),
            ..default()
        }
    }

    #[test]
    fn a_frame_is_in_points_from_a_physical_position() {
        assert_eq!(
            frame_of(IVec2::new(200, 100), Vec2::new(1280.0, 800.0), 2.0),
            Rect::new(100.0, 50.0, 1280.0, 800.0)
        );
    }

    #[test]
    fn the_frame_is_written_once_the_window_holds_still() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("window.json");
        let (mut app, window) = frame_app(&file, window_at(80, 100, 1400, 900));
        app.update();
        app.update();
        assert!(!file.exists(), "still moving, nothing written");
        app.world_mut().get_mut::<Window>(window).unwrap().position =
            WindowPosition::At(IVec2::new(90, 30));
        for _ in 0..4 {
            app.update();
        }
        let saved = Remembered::load(&file);
        assert_eq!(
            saved.placement(&laptop()),
            Some(Rect::new(90.0, 30.0, 1400.0, 900.0)),
            "the last position, not the first"
        );
    }

    #[test]
    fn a_window_that_stays_put_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("window.json");
        let (mut app, _) = frame_app(&file, window_at(80, 100, 1400, 900));
        for _ in 0..4 {
            app.update();
        }
        assert!(file.exists());
        std::fs::remove_file(&file).unwrap();
        for _ in 0..12 {
            app.update();
        }
        assert!(!file.exists(), "the same frame is not written again");
    }

    #[test]
    fn the_saved_window_is_placed_with_the_scale_of_the_window_itself() {
        let (mut app, window) = frame_app(
            Path::new("/nonexistent/window.json"),
            Window {
                resolution: bevy::window::WindowResolution::new(1400, 900)
                    .with_scale_factor_override(2.0),
                ..default()
            },
        );
        app.insert_resource(PlaceWindow(Some(Rect::new(100.0, 50.0, 1400.0, 900.0))))
            .add_systems(Update, place_window);
        app.update();
        let w = app.world().get::<Window>(window).unwrap();
        assert_eq!(w.position, WindowPosition::At(IVec2::new(200, 100)));
        assert!(app.world().resource::<PlaceWindow>().0.is_none(), "once");
    }

    #[test]
    fn without_a_frame_memory_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("window.json");
        let (mut app, _) = frame_app(&file, window_at(80, 100, 1400, 900));
        app.world_mut().remove_resource::<FrameMemory>();
        for _ in 0..6 {
            app.update();
        }
        assert!(!file.exists());
    }
}
