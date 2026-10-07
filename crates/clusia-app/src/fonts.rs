//! Inter (UI, upright and italic) and JetBrains Mono (code), all OFL and compiled into the binary
//! (spec §7.1).

use bevy::prelude::*;

pub const INTER: &[u8] = include_bytes!("../assets/fonts/InterVariable.ttf");
pub const INTER_ITALIC: &[u8] = include_bytes!("../assets/fonts/Inter-Italic[opsz,wght].ttf");
pub const JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono[wght].ttf");

/// Handles to the bundled faces. Inter has no synthetic slant, so emphasis uses its own italic.
/// `Default` (empty handles) is for headless tests.
#[derive(Resource, Debug, Clone, Default)]
pub struct UiFonts {
    pub sans: Handle<Font>,
    pub italic: Handle<Font>,
    pub mono: Handle<Font>,
}

pub fn load(fonts: &mut Assets<Font>) -> UiFonts {
    UiFonts {
        sans: fonts.add(Font::from_bytes(INTER.to_vec())),
        italic: fonts.add(Font::from_bytes(INTER_ITALIC.to_vec())),
        mono: fonts.add(Font::from_bytes(JETBRAINS_MONO.to_vec())),
    }
}

/// Adds the fonts to `Assets<Font>` (which the text plugin, or a test, must have registered).
pub struct FontsPlugin;

impl Plugin for FontsPlugin {
    fn build(&self, app: &mut App) {
        let fonts = load(&mut app.world_mut().resource_mut::<Assets<Font>>());
        app.insert_resource(fonts);
    }
}

/// Whether `font` (TrueType bytes) has a glyph for `ch`, read from its Unicode `cmap`
/// subtable (format 12, else format 4). Without one, the text shows a missing-glyph box.
#[cfg(test)]
pub(crate) fn covers(font: &[u8], ch: char) -> bool {
    let u16_at = |at: usize| u16::from_be_bytes([font[at], font[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_be_bytes(font[at..at + 4].try_into().unwrap()) as usize;
    let Some(cmap) = (0..u16_at(4))
        .map(|i| 12 + 16 * i)
        .find(|&r| &font[r..r + 4] == b"cmap")
        .map(|r| u32_at(r + 8))
    else {
        return false;
    };
    let subtables: Vec<usize> = (0..u16_at(cmap + 2))
        .map(|i| cmap + u32_at(cmap + 4 + 8 * i + 4))
        .collect();
    let code = ch as usize;
    if let Some(&t) = subtables.iter().find(|&&t| u16_at(t) == 12) {
        return (0..u32_at(t + 12)).any(|g| {
            let group = t + 16 + 12 * g;
            let (start, end, glyph) = (u32_at(group), u32_at(group + 4), u32_at(group + 8));
            (start..=end).contains(&code) && glyph + code - start != 0
        });
    }
    let Some(&t) = subtables.iter().find(|&&t| u16_at(t) == 4) else {
        return false;
    };
    let segments = u16_at(t + 6) / 2;
    let ends = t + 14;
    let starts = ends + 2 * segments + 2;
    let deltas = starts + 2 * segments;
    let ranges = deltas + 2 * segments;
    (0..segments).any(|i| {
        let (start, end) = (u16_at(starts + 2 * i), u16_at(ends + 2 * i));
        if !(start..=end).contains(&code) {
            return false;
        }
        let range = u16_at(ranges + 2 * i);
        let glyph = if range == 0 {
            code
        } else {
            u16_at(ranges + 2 * i + range + 2 * (code - start))
        };
        glyph != 0 && (glyph + u16_at(deltas + 2 * i)) % 0x10000 != 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_reads_the_character_map() {
        for font in [INTER, INTER_ITALIC, JETBRAINS_MONO] {
            assert!(covers(font, 'A') && covers(font, '±'));
            assert!(!covers(font, '\u{2623}'), "no biohazard sign");
        }
        assert!(covers(JETBRAINS_MONO, '◎') && !covers(INTER, '◎'));
        assert!(!covers(INTER, '☺') && !covers(INTER, '▣'));
    }

    #[test]
    fn fonts_are_embedded_and_distinct() {
        assert!(INTER.len() > 500_000 && JETBRAINS_MONO.len() > 200_000);
        assert!(INTER_ITALIC.len() > 500_000);
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::asset::AssetPlugin::default()))
            .init_asset::<Font>()
            .add_plugins(FontsPlugin);
        let fonts = app.world().resource::<UiFonts>().clone();
        assert_ne!(fonts.sans, fonts.mono);
        assert_ne!(fonts.sans, fonts.italic);
        assert_ne!(fonts.italic, fonts.mono);
        let assets = app.world().resource::<Assets<Font>>();
        assert!(assets.get(&fonts.sans).is_some() && assets.get(&fonts.mono).is_some());
        assert!(assets.get(&fonts.italic).is_some());
    }
}
