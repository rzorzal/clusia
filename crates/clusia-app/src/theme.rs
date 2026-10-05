//! Theme tokens (spec §7.1; values from `docs/mockups/README.md`). The only place with literal
//! colors: screens name a `Swatch` and the kit resolves it against the current `Tokens`.

use bevy::prelude::*;
use clusia_core::Density;
use clusia_core::config::Theme as ThemeChoice;

/// A named color role. Screens use these, never `Color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Swatch {
    Clear,
    Bg,
    Chrome,
    Surface,
    /// The selected segment of a segmented control.
    Raised,
    Line,
    Fg,
    Muted,
    Faint,
    Green,
    GreenHover,
    OnGreen,
    GreenSoft,
    Orange,
    OrangeSoft,
    Hover,
    Selected,
    /// The knob of a switch.
    Knob,
    /// Heatmap level 0–4 (higher values clamp to 4).
    Heat(u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub bg: Color,
    pub chrome: Color,
    pub surface: Color,
    pub raised: Color,
    pub line: Color,
    pub fg: Color,
    pub muted: Color,
    pub faint: Color,
    pub green: Color,
    pub green_hover: Color,
    pub on_green: Color,
    pub green_soft: Color,
    pub orange: Color,
    pub orange_soft: Color,
    pub hover: Color,
    pub selected: Color,
    pub knob: Color,
    pub heat: [Color; 5],
}

impl Tokens {
    pub fn get(&self, s: Swatch) -> Color {
        match s {
            Swatch::Clear => Color::NONE,
            Swatch::Bg => self.bg,
            Swatch::Chrome => self.chrome,
            Swatch::Surface => self.surface,
            Swatch::Raised => self.raised,
            Swatch::Line => self.line,
            Swatch::Fg => self.fg,
            Swatch::Muted => self.muted,
            Swatch::Faint => self.faint,
            Swatch::Green => self.green,
            Swatch::GreenHover => self.green_hover,
            Swatch::OnGreen => self.on_green,
            Swatch::GreenSoft => self.green_soft,
            Swatch::Orange => self.orange,
            Swatch::OrangeSoft => self.orange_soft,
            Swatch::Hover => self.hover,
            Swatch::Selected => self.selected,
            Swatch::Knob => self.knob,
            Swatch::Heat(level) => self.heat[usize::from(level.min(4))],
        }
    }
}

pub const LIGHT: Tokens = Tokens {
    bg: Color::srgb_u8(0xF6, 0xF7, 0xF5),
    chrome: Color::srgb_u8(0xEE, 0xF1, 0xEE),
    surface: Color::srgb_u8(0xFF, 0xFF, 0xFF),
    raised: Color::srgb_u8(0xFF, 0xFF, 0xFF),
    line: Color::srgb_u8(0xE2, 0xE6, 0xE2),
    fg: Color::srgb_u8(0x1B, 0x1F, 0x1C),
    muted: Color::srgb_u8(0x5D, 0x66, 0x5F),
    faint: Color::srgb_u8(0x8A, 0x93, 0x8C),
    green: Color::srgb_u8(0x2F, 0x9E, 0x44),
    green_hover: Color::srgb_u8(0x2A, 0x8D, 0x3D),
    on_green: Color::srgb_u8(0xFF, 0xFF, 0xFF),
    green_soft: Color::srgba_u8(0x2F, 0x9E, 0x44, 0x24),
    orange: Color::srgb_u8(0xD9, 0x57, 0x2B),
    orange_soft: Color::srgba_u8(0xD9, 0x57, 0x2B, 0x1F),
    hover: Color::srgb_u8(0xF1, 0xF4, 0xF1),
    selected: Color::srgb_u8(0xE9, 0xF6, 0xEB),
    knob: Color::srgb_u8(0xFF, 0xFF, 0xFF),
    heat: [
        Color::srgb_u8(0xEC, 0xEE, 0xEC),
        Color::srgb_u8(0xC4, 0xE3, 0xC9),
        Color::srgb_u8(0x8F, 0xCB, 0x98),
        Color::srgb_u8(0x55, 0xB0, 0x63),
        Color::srgb_u8(0x2F, 0x9E, 0x44),
    ],
};

pub const DARK: Tokens = Tokens {
    bg: Color::srgb_u8(0x12, 0x15, 0x14),
    chrome: Color::srgb_u8(0x18, 0x1C, 0x1A),
    surface: Color::srgb_u8(0x1A, 0x1E, 0x1C),
    raised: Color::srgb_u8(0x2A, 0x30, 0x2C),
    line: Color::srgb_u8(0x2A, 0x30, 0x2C),
    fg: Color::srgb_u8(0xE9, 0xEC, 0xE9),
    muted: Color::srgb_u8(0x9A, 0xA4, 0x9C),
    faint: Color::srgb_u8(0x6B, 0x74, 0x6D),
    green: Color::srgb_u8(0x4C, 0xC3, 0x5F),
    green_hover: Color::srgb_u8(0x5C, 0xCF, 0x6E),
    on_green: Color::srgb_u8(0x0C, 0x1A, 0x0F),
    green_soft: Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x29),
    orange: Color::srgb_u8(0xF0, 0x7A, 0x4A),
    orange_soft: Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x29),
    hover: Color::srgb_u8(0x22, 0x27, 0x24),
    selected: Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x1F),
    knob: Color::srgb_u8(0xE9, 0xEC, 0xE9),
    heat: [
        Color::srgb_u8(0x2E, 0x30, 0x2E),
        Color::srgb_u8(0x1F, 0x4A, 0x27),
        Color::srgb_u8(0x2A, 0x7A, 0x3A),
        Color::srgb_u8(0x3A, 0xA3, 0x4E),
        Color::srgb_u8(0x4C, 0xC3, 0x5F),
    ],
};

/// The look in effect. Changes only when the result differs, so `restyle` runs only then.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub tokens: Tokens,
    /// Points, for code (diff, previews).
    pub code_size: f32,
    pub compact: bool,
}

impl Theme {
    pub fn new(dark: bool, code_size: u8, density: Density) -> Self {
        Self {
            dark,
            tokens: if dark { DARK } else { LIGHT },
            code_size: f32::from(code_size),
            compact: density == Density::Compact,
        }
    }
}

pub fn is_dark(choice: ThemeChoice, system_dark: bool) -> bool {
    match choice {
        ThemeChoice::Light => false,
        ThemeChoice::Dark => true,
        ThemeChoice::System => system_dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_the_mockups() {
        assert_eq!(LIGHT.bg, Color::srgb_u8(0xF6, 0xF7, 0xF5));
        assert_eq!(LIGHT.fg, Color::srgb_u8(0x1B, 0x1F, 0x1C));
        assert_eq!(LIGHT.green, Color::srgb_u8(0x2F, 0x9E, 0x44));
        assert_eq!(DARK.bg, Color::srgb_u8(0x12, 0x15, 0x14));
        assert_eq!(DARK.green, Color::srgb_u8(0x4C, 0xC3, 0x5F));
        assert_eq!(DARK.orange, Color::srgb_u8(0xF0, 0x7A, 0x4A));
        assert_eq!(LIGHT.get(Swatch::Orange), Color::srgb_u8(0xD9, 0x57, 0x2B));
        assert_eq!(DARK.get(Swatch::Clear), Color::NONE);
        assert_eq!(LIGHT.get(Swatch::Heat(0)), LIGHT.heat[0]);
        assert_eq!(
            LIGHT.get(Swatch::Heat(9)),
            LIGHT.heat[4],
            "levels above 4 clamp"
        );
    }

    #[test]
    fn choosing_dark() {
        assert!(!is_dark(ThemeChoice::Light, true));
        assert!(is_dark(ThemeChoice::Dark, false));
        assert!(is_dark(ThemeChoice::System, true));
        assert!(!is_dark(ThemeChoice::System, false));
    }

    #[test]
    fn theme_carries_sizes() {
        let t = Theme::new(true, 16, Density::Compact);
        assert!(t.dark && t.compact);
        assert_eq!(t.tokens, DARK);
        assert_eq!(t.code_size, 16.0);
        assert_eq!(Theme::new(false, 13, Density::Comfortable).tokens, LIGHT);
    }
}
