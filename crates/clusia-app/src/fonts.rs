//! Inter (UI) and JetBrains Mono (code), both OFL and compiled into the binary (spec §7.1).

use bevy::prelude::*;

pub const INTER: &[u8] = include_bytes!("../assets/fonts/InterVariable.ttf");
pub const JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono[wght].ttf");

/// Handles to the two bundled faces. `Default` (empty handles) is for headless tests.
#[derive(Resource, Debug, Clone, Default)]
pub struct UiFonts {
    pub sans: Handle<Font>,
    pub mono: Handle<Font>,
}

pub fn load(fonts: &mut Assets<Font>) -> UiFonts {
    UiFonts {
        sans: fonts.add(Font::from_bytes(INTER.to_vec())),
        mono: fonts.add(Font::from_bytes(JETBRAINS_MONO.to_vec())),
    }
}

/// Adds both fonts to `Assets<Font>` (which the text plugin, or a test, must have registered).
pub struct FontsPlugin;

impl Plugin for FontsPlugin {
    fn build(&self, app: &mut App) {
        let fonts = load(&mut app.world_mut().resource_mut::<Assets<Font>>());
        app.insert_resource(fonts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_are_embedded_and_distinct() {
        assert!(INTER.len() > 500_000 && JETBRAINS_MONO.len() > 200_000);
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::asset::AssetPlugin::default()))
            .init_asset::<Font>()
            .add_plugins(FontsPlugin);
        let fonts = app.world().resource::<UiFonts>().clone();
        assert_ne!(fonts.sans, fonts.mono);
        let assets = app.world().resource::<Assets<Font>>();
        assert!(assets.get(&fonts.sans).is_some() && assets.get(&fonts.mono).is_some());
    }
}
