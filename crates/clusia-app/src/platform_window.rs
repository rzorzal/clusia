//! Talking to macOS about the window's app: the Dock and ⌘-Tab icon, and coming to the front.
//! Tests insert `Foregrounds`, which counts the requests instead.

use std::path::{Path, PathBuf};

use bevy::ecs::system::NonSendMarker;
use bevy::prelude::*;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
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

/// Registers the icon at startup.
pub struct PlatformPlugin;

impl Plugin for PlatformPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, set_icon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
