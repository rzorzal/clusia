//! The ▶ next to the sound choice: plays a notification sound with `afplay`. Tests insert
//! `PlayedSounds`, which records the choice instead.

use std::path::{Path, PathBuf};

use bevy::prelude::*;
use clusia_core::config::SoundId;

/// When present, previews are recorded here instead of played (tests).
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct PlayedSounds(pub Vec<SoundId>);

/// The sound's file: inside the app bundle next to this executable
/// (`Contents/Resources`), else the copy in the source tree (running from `target/`).
pub fn sound_file(exe: &Path, id: SoundId, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let name = format!("{}.aiff", id.as_str());
    let bundled = exe.parent()?.parent()?.join("Resources").join(&name);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../clusia/assets/sounds")
        .join(&name);
    [bundled, source].into_iter().find(|p| exists(p))
}

/// Plays `id` in the background; a missing file or player is only logged.
pub fn play(id: SoundId) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(file) = sound_file(&exe, id, |p| p.is_file()) else {
        tracing::warn!(sound = id.as_str(), "no sound file to preview");
        return;
    };
    match std::process::Command::new("/usr/bin/afplay")
        .arg(file)
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(error = %e, "cannot run /usr/bin/afplay"),
    }
}

/// Records `id` when `PlayedSounds` exists, otherwise plays it.
pub fn preview(recorder: Option<ResMut<PlayedSounds>>, id: SoundId) {
    match recorder {
        Some(mut played) => played.0.push(id),
        None => play(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundle_wins_over_the_source_tree() {
        let exe = Path::new("/Applications/Clusia.app/Contents/MacOS/clusia-app");
        let found = sound_file(exe, SoundId::Leaf, |_| true).unwrap();
        assert_eq!(
            found,
            PathBuf::from("/Applications/Clusia.app/Contents/Resources/leaf.aiff")
        );
    }

    #[test]
    fn a_build_folder_falls_back_to_the_source_tree() {
        let exe = Path::new("/work/target/debug/clusia-app");
        let found = sound_file(exe, SoundId::Chime, |p| {
            p.to_string_lossy().contains("assets/sounds")
        })
        .unwrap();
        assert!(
            found.ends_with("clusia/assets/sounds/chime.aiff"),
            "{found:?}"
        );
        assert_eq!(sound_file(exe, SoundId::Chime, |_| false), None);
    }

    #[test]
    fn every_sound_exists_in_the_source_tree() {
        let exe = Path::new("/work/target/debug/clusia-app");
        for id in SoundId::ALL {
            assert!(
                sound_file(exe, id, |p| p.is_file()).is_some(),
                "{}",
                id.as_str()
            );
        }
    }
}
