//! Talking to macOS about the window's app: the Dock and ⌘-Tab icon, and coming to the front.
//! Tests insert `Foregrounds`, which counts the requests instead, and `ReduceMotionSource`, which
//! stands in for the system's Reduce Motion setting.

use std::path::{Path, PathBuf};

use bevy::ecs::system::NonSendMarker;
use bevy::prelude::*;
use bevy::window::WindowFocused;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage, NSWorkspace};
use objc2_foundation::NSString;

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

/// Registers the icon and the Reduce Motion follower at startup.
pub struct PlatformPlugin;

impl Plugin for PlatformPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReduceMotionSource>()
            .add_systems(Startup, set_icon)
            .add_systems(Update, follow_reduce_motion);
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
}
