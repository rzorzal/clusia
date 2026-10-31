//! Theme tokens (spec §7.1; values from `docs/mockups/README.md`). The only place with literal
//! colors: screens name a `Swatch` and the kit resolves it against the current `Tokens`.

use bevy::prelude::*;
use bevy::text::RemSize;
use bevy::window::{PrimaryWindow, WindowTheme, WindowThemeChanged};
use clusia_core::config::Theme as ThemeChoice;
use clusia_core::{Density, TextSize};
use clusia_highlight::Class;

use crate::bridge::Model;

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
    /// A primary button while pressed.
    GreenPress,
    OnGreen,
    GreenSoft,
    /// The fill of a chosen or emphasised card, row or chip: a faint green close to the surface it
    /// sits on, so muted text stays readable on it in both themes.
    GreenTint,
    /// A softer added-line tint (diff and code previews).
    AddedBg,
    /// The removed-line tint.
    RemovedBg,
    /// The changed part of a paired line (Split view).
    AddedStrong,
    RemovedStrong,
    /// The dimmed layer behind a modal.
    Scrim,
    /// Syntax highlighting.
    Code(Class),
    Orange,
    OrangeSoft,
    /// A danger button while pressed.
    OrangePress,
    Hover,
    Selected,
    /// Highlight of selected code text.
    TextSelect,
    /// The knob of a switch.
    Knob,
    /// Heatmap level 0–4 (higher values clamp to 4).
    Heat(u8),
    /// Theme previews: fixed light (`false`) or dark (`true`) colors, whatever the current theme.
    PreviewBg(bool),
    PreviewInk(bool),
    PreviewGreen(bool),
    /// The dots of the diagram canvas.
    CanvasDot,
    /// The lines and arrowheads of a diagram.
    Edge,
    /// `Edge` at 60 % over a surface: a reply arrow, a removed line.
    EdgeSoft,
    /// The fill of a changed box (softer than `GreenSoft`).
    ChangedBg,
    /// The fill of a box at risk.
    RiskBg,
    /// The text of a box that is gone in the version being compared.
    GhostInk,
    /// The fill of a box that is gone in the version being compared.
    GhostFill,
    /// The outline of a box that changed since the version being compared.
    Amber,
    AmberSoft,
    /// The fill of a setup step that runs on the reviewer's machine, and the ink of its text
    /// and its tag.
    HostRow,
    HostInk,
    /// The ink of the `network` tag and its tint.
    NetBlue,
    NetBlueSoft,
    /// The 3D scene's background.
    SceneBg,
    Axis,
    Grid,
    Cloud,
    /// Word family `i` (0..12; larger values clamp to the last).
    Family(u8),
    /// The places that belong to no use case.
    Elsewhere,
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
    pub green_press: Color,
    pub on_green: Color,
    pub green_soft: Color,
    pub green_tint: Color,
    pub added_bg: Color,
    pub removed_bg: Color,
    pub added_strong: Color,
    pub removed_strong: Color,
    pub scrim: Color,
    /// Syntax colors, indexed by `Class::index` (the order of `Class::ALL`).
    pub code: [Color; 14],
    pub orange: Color,
    pub orange_soft: Color,
    pub orange_press: Color,
    pub hover: Color,
    pub selected: Color,
    pub text_select: Color,
    pub knob: Color,
    pub heat: [Color; 5],
    pub canvas_dot: Color,
    pub edge: Color,
    pub edge_soft: Color,
    pub changed_bg: Color,
    pub risk_bg: Color,
    pub ghost_ink: Color,
    pub ghost_fill: Color,
    pub amber: Color,
    pub amber_soft: Color,
    pub host_row: Color,
    pub host_ink: Color,
    pub net_blue: Color,
    pub net_blue_soft: Color,
    /// The keyword view's floor: the colour its 3D image is cleared with.
    pub scene_bg: Color,
    /// The cube's three axes, and its floor grid.
    pub axis: Color,
    pub grid: Color,
    /// A use case's cloud: the axis colour at 7 %.
    pub cloud: Color,
    /// Places that belong to no use case.
    pub elsewhere: Color,
    /// The twelve word-family colours, each at least 3:1 against `scene_bg`.
    pub family: [Color; 12],
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
            Swatch::GreenPress => self.green_press,
            Swatch::OnGreen => self.on_green,
            Swatch::GreenSoft => self.green_soft,
            Swatch::GreenTint => self.green_tint,
            Swatch::AddedBg => self.added_bg,
            Swatch::RemovedBg => self.removed_bg,
            Swatch::AddedStrong => self.added_strong,
            Swatch::RemovedStrong => self.removed_strong,
            Swatch::Scrim => self.scrim,
            Swatch::Code(class) => self.code[class.index()],
            Swatch::Orange => self.orange,
            Swatch::OrangeSoft => self.orange_soft,
            Swatch::OrangePress => self.orange_press,
            Swatch::Hover => self.hover,
            Swatch::Selected => self.selected,
            Swatch::TextSelect => self.text_select,
            Swatch::Knob => self.knob,
            Swatch::PreviewBg(dark) => {
                if dark {
                    DARK.bg
                } else {
                    LIGHT.bg
                }
            }
            Swatch::PreviewInk(dark) => {
                if dark {
                    DARK.fg
                } else {
                    LIGHT.fg
                }
            }
            Swatch::PreviewGreen(dark) => {
                if dark {
                    DARK.green
                } else {
                    LIGHT.green
                }
            }
            Swatch::Heat(level) => self.heat[usize::from(level.min(4))],
            Swatch::CanvasDot => self.canvas_dot,
            Swatch::Edge => self.edge,
            Swatch::EdgeSoft => self.edge_soft,
            Swatch::ChangedBg => self.changed_bg,
            Swatch::RiskBg => self.risk_bg,
            Swatch::GhostInk => self.ghost_ink,
            Swatch::GhostFill => self.ghost_fill,
            Swatch::Amber => self.amber,
            Swatch::AmberSoft => self.amber_soft,
            Swatch::HostRow => self.host_row,
            Swatch::HostInk => self.host_ink,
            Swatch::NetBlue => self.net_blue,
            Swatch::NetBlueSoft => self.net_blue_soft,
            Swatch::SceneBg => self.scene_bg,
            Swatch::Axis => self.axis,
            Swatch::Grid => self.grid,
            Swatch::Cloud => self.cloud,
            Swatch::Family(i) => self.family[usize::from(i).min(11)],
            Swatch::Elsewhere => self.elsewhere,
        }
    }
}

/// The colour of the keyword view's ambient light, the same in both themes.
pub const SCENE_LIGHT: Color = Color::WHITE;

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
    green_press: Color::srgb_u8(0x1F, 0x78, 0x30),
    on_green: Color::srgb_u8(0xFF, 0xFF, 0xFF),
    green_soft: Color::srgba_u8(0x2F, 0x9E, 0x44, 0x24),
    green_tint: Color::srgba_u8(0x2F, 0x9E, 0x44, 0x24),
    added_bg: Color::srgb_u8(0xE9, 0xF6, 0xEB),
    removed_bg: Color::srgb_u8(0xFC, 0xEE, 0xE8),
    added_strong: Color::srgba_u8(0x2F, 0x9E, 0x44, 0x42),
    removed_strong: Color::srgba_u8(0xD9, 0x57, 0x2B, 0x42),
    scrim: Color::srgba_u8(0xF6, 0xF7, 0xF5, 0x8C),
    code: [
        Color::srgb_u8(0x7C, 0x3A, 0xAD), // keyword
        Color::srgb_u8(0x0A, 0x6C, 0x5A), // string
        Color::srgb_u8(0x74, 0x7D, 0x76), // comment
        Color::srgb_u8(0x8B, 0x52, 0x00), // type
        Color::srgb_u8(0x26, 0x59, 0xB8), // function
        Color::srgb_u8(0x9B, 0x47, 0x1B), // number
        Color::srgb_u8(0x9B, 0x47, 0x1B), // constant
        Color::srgb_u8(0x59, 0x62, 0x5B), // operator
        Color::srgb_u8(0x59, 0x62, 0x5B), // punctuation
        Color::srgb_u8(0x1D, 0x67, 0x81), // property
        Color::srgb_u8(0x26, 0x59, 0xB8), // tag
        Color::srgb_u8(0x8B, 0x52, 0x00), // attribute
        Color::srgb_u8(0x1B, 0x1F, 0x1C), // variable
        Color::srgb_u8(0x1B, 0x1F, 0x1C), // plain
    ],
    orange: Color::srgb_u8(0xD9, 0x57, 0x2B),
    orange_soft: Color::srgba_u8(0xD9, 0x57, 0x2B, 0x1F),
    orange_press: Color::srgba_u8(0xD9, 0x57, 0x2B, 0x38),
    hover: Color::srgb_u8(0xF1, 0xF4, 0xF1),
    selected: Color::srgb_u8(0xE9, 0xF6, 0xEB),
    // Opaque like the tints around it: over added and removed rows it must still read.
    text_select: Color::srgb_u8(0xBF, 0xD6, 0xF7),
    knob: Color::srgb_u8(0xFF, 0xFF, 0xFF),
    heat: [
        Color::srgb_u8(0xEC, 0xEE, 0xEC),
        Color::srgb_u8(0xC4, 0xE3, 0xC9),
        Color::srgb_u8(0x8F, 0xCB, 0x98),
        Color::srgb_u8(0x55, 0xB0, 0x63),
        Color::srgb_u8(0x2F, 0x9E, 0x44),
    ],
    canvas_dot: Color::srgb_u8(0xDD, 0xE2, 0xDD),
    edge: Color::srgb_u8(0xA4, 0xAC, 0xA6),
    edge_soft: Color::srgb_u8(0xC8, 0xCD, 0xCA),
    // Opaque: the UI blends in linear light, so a 10 % tint over white came out almost white.
    changed_bg: Color::srgb_u8(0xEA, 0xF5, 0xEC),
    risk_bg: Color::srgb_u8(0xFB, 0xEE, 0xE9),
    ghost_ink: Color::srgb_u8(0xA9, 0xB0, 0xAB),
    ghost_fill: Color::srgba_u8(0x8A, 0x93, 0x8C, 0x1A),
    amber: Color::srgb_u8(0xB7, 0x79, 0x1F),
    amber_soft: Color::srgba_u8(0xB7, 0x79, 0x1F, 0x1A),
    host_row: Color::srgba_u8(0xE0, 0x9A, 0x1F, 0x2A),
    host_ink: Color::srgb_u8(0x7A, 0x4A, 0x08),
    net_blue: Color::srgb_u8(0x1F, 0x4F, 0xB0),
    net_blue_soft: Color::srgba_u8(0x2B, 0x6C, 0xD9, 0x1A),
    scene_bg: Color::srgb_u8(0xFB, 0xFB, 0xF9),
    axis: Color::srgb_u8(0x9A, 0x9D, 0x96),
    grid: Color::srgb_u8(0xE8, 0xE8, 0xE3),
    cloud: Color::srgba_u8(0x9A, 0x9D, 0x96, 0x12),
    elsewhere: Color::srgb_u8(0x8A, 0x93, 0x8C),
    family: [
        Color::srgb_u8(0x2F, 0x9E, 0x44),
        Color::srgb_u8(0x70, 0x48, 0xE8),
        Color::srgb_u8(0x1C, 0x7E, 0xD6),
        Color::srgb_u8(0xD9, 0x48, 0x0F),
        Color::srgb_u8(0xC2, 0x25, 0x5C),
        Color::srgb_u8(0x0C, 0x85, 0x99),
        Color::srgb_u8(0xA3, 0x5F, 0x00),
        Color::srgb_u8(0x42, 0x63, 0xEB),
        Color::srgb_u8(0x8A, 0x5A, 0x2B),
        Color::srgb_u8(0x5F, 0x7A, 0x00),
        Color::srgb_u8(0x9C, 0x36, 0xB5),
        Color::srgb_u8(0x49, 0x50, 0x57),
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
    green_press: Color::srgb_u8(0x47, 0xB5, 0x59),
    on_green: Color::srgb_u8(0x0C, 0x1A, 0x0F),
    green_soft: Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x29),
    green_tint: Color::srgb_u8(0x1E, 0x2B, 0x22),
    added_bg: Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x0C),
    removed_bg: Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x0C),
    added_strong: Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x18),
    removed_strong: Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x18),
    scrim: Color::srgba_u8(0x08, 0x0A, 0x09, 0x99),
    code: [
        Color::srgb_u8(0xD3, 0xA9, 0xEE), // keyword
        Color::srgb_u8(0x7F, 0xD1, 0xAE), // string
        Color::srgb_u8(0x90, 0x99, 0x92), // comment
        Color::srgb_u8(0xE5, 0xB5, 0x67), // type
        Color::srgb_u8(0x97, 0xB9, 0xFF), // function
        Color::srgb_u8(0xF3, 0xA8, 0x7A), // number
        Color::srgb_u8(0xF3, 0xA8, 0x7A), // constant
        Color::srgb_u8(0xB3, 0xBB, 0xB5), // operator
        Color::srgb_u8(0xB3, 0xBB, 0xB5), // punctuation
        Color::srgb_u8(0x7F, 0xC8, 0xDB), // property
        Color::srgb_u8(0x97, 0xB9, 0xFF), // tag
        Color::srgb_u8(0xE5, 0xB5, 0x67), // attribute
        Color::srgb_u8(0xE9, 0xEC, 0xE9), // variable
        Color::srgb_u8(0xE9, 0xEC, 0xE9), // plain
    ],
    orange: Color::srgb_u8(0xF0, 0x7A, 0x4A),
    orange_soft: Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x29),
    orange_press: Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x45),
    hover: Color::srgb_u8(0x22, 0x27, 0x24),
    selected: Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x1F),
    text_select: Color::srgb_u8(0x2B, 0x4A, 0x7A),
    knob: Color::srgb_u8(0xE9, 0xEC, 0xE9),
    heat: [
        Color::srgb_u8(0x2E, 0x30, 0x2E),
        Color::srgb_u8(0x1F, 0x4A, 0x27),
        Color::srgb_u8(0x2A, 0x7A, 0x3A),
        Color::srgb_u8(0x3A, 0xA3, 0x4E),
        Color::srgb_u8(0x4C, 0xC3, 0x5F),
    ],
    canvas_dot: Color::srgb_u8(0x2A, 0x30, 0x2C),
    edge: Color::srgb_u8(0x59, 0x62, 0x5B),
    edge_soft: Color::srgb_u8(0x40, 0x47, 0x42),
    changed_bg: Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x1F),
    risk_bg: Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x1F),
    ghost_ink: Color::srgb_u8(0x59, 0x62, 0x5B),
    ghost_fill: Color::srgba_u8(0x6B, 0x74, 0x6D, 0x24),
    amber: Color::srgb_u8(0xE0, 0xA4, 0x3A),
    amber_soft: Color::srgba_u8(0xE0, 0xA4, 0x3A, 0x1F),
    host_row: Color::srgba_u8(0xE0, 0xA4, 0x3A, 0x1C),
    host_ink: Color::srgb_u8(0xF8, 0xD6, 0x96),
    net_blue: Color::srgb_u8(0xCB, 0xE0, 0xFD),
    net_blue_soft: Color::srgba_u8(0x6F, 0xA8, 0xF5, 0x2E),
    scene_bg: Color::srgb_u8(0x12, 0x14, 0x12),
    axis: Color::srgb_u8(0x6C, 0x70, 0x6A),
    grid: Color::srgb_u8(0x23, 0x26, 0x23),
    cloud: Color::srgba_u8(0x6C, 0x70, 0x6A, 0x12),
    elsewhere: Color::srgb_u8(0x6B, 0x74, 0x6D),
    family: [
        Color::srgb_u8(0x4C, 0xC3, 0x5F),
        Color::srgb_u8(0x97, 0x75, 0xFA),
        Color::srgb_u8(0x4D, 0xAB, 0xF7),
        Color::srgb_u8(0xF0, 0x8C, 0x5A),
        Color::srgb_u8(0xF0, 0x65, 0x95),
        Color::srgb_u8(0x22, 0xB8, 0xCF),
        Color::srgb_u8(0xF0, 0xA6, 0x3A),
        Color::srgb_u8(0x74, 0x8F, 0xFC),
        Color::srgb_u8(0xC9, 0x94, 0x5A),
        Color::srgb_u8(0xA9, 0xC2, 0x3C),
        Color::srgb_u8(0xCC, 0x5D, 0xE8),
        Color::srgb_u8(0xAD, 0xB5, 0xBD),
    ],
};

/// The look in effect. Changes only when the result differs, so `restyle` runs only then.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub tokens: Tokens,
    /// Points, for code (diff, previews), at the medium text size.
    pub code_size: f32,
    pub compact: bool,
    /// The factor on every text size (`RemSize`): text sizes are in rem, so changing it resizes
    /// all text without rebuilding a screen.
    pub text_scale: f32,
}

impl Theme {
    pub fn new(dark: bool, code_size: u8, density: Density) -> Self {
        Self {
            dark,
            tokens: if dark { DARK } else { LIGHT },
            code_size: f32::from(code_size),
            compact: density == Density::Compact,
            text_scale: 1.0,
        }
    }

    pub fn with_text_size(self, size: TextSize) -> Self {
        Self {
            text_scale: size.scale(),
            ..self
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

/// macOS is in dark mode (including Auto at night).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemDark(pub bool);

pub struct ThemePlugin;

impl Plugin for ThemePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SystemDark>().add_systems(
            PreUpdate,
            (follow_system, update_theme, follow_text_scale).chain(),
        );
    }
}

/// Seeds from the window's theme once winit reports it, then follows `WindowThemeChanged`.
fn follow_system(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut changes: MessageReader<WindowThemeChanged>,
    mut system: ResMut<SystemDark>,
    mut seeded: Local<bool>,
) {
    let mut dark = None;
    if !*seeded
        && let Ok(w) = windows.single()
        && let Some(theme) = w.window_theme
    {
        dark = Some(theme == WindowTheme::Dark);
        *seeded = true;
    }
    for change in changes.read() {
        dark = Some(change.theme == WindowTheme::Dark);
        *seeded = true;
    }
    if let Some(dark) = dark
        && system.0 != dark
    {
        system.0 = dark;
    }
}

fn update_theme(
    model: Res<Model>,
    system: Res<SystemDark>,
    mut theme: ResMut<Theme>,
    clear: Option<ResMut<ClearColor>>,
) {
    if !(model.is_changed() || system.is_changed()) {
        return;
    }
    let a = &model.snapshot.config.appearance;
    let next =
        Theme::new(is_dark(a.theme, system.0), a.code_size, a.density).with_text_size(a.text_size);
    if *theme != next {
        *theme = next;
        if let Some(mut clear) = clear {
            clear.0 = next.tokens.bg;
        }
    }
}

/// Keeps `RemSize` on the theme's text scale. Text relaid by the change is all there is to do.
fn follow_text_scale(theme: Res<Theme>, rem: Option<ResMut<RemSize>>) {
    let Some(mut rem) = rem else { return };
    if (theme.is_changed() || rem.is_added()) && rem.0 != theme.text_scale {
        rem.0 = theme.text_scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_ignore_the_current_theme() {
        assert_eq!(LIGHT.get(Swatch::PreviewBg(true)), DARK.bg);
        assert_eq!(DARK.get(Swatch::PreviewBg(false)), LIGHT.bg);
        assert_eq!(DARK.get(Swatch::PreviewGreen(false)), LIGHT.green);
        assert_eq!(LIGHT.get(Swatch::PreviewInk(true)), DARK.fg);
    }

    #[test]
    fn selected_code_shows_over_every_row_fill_in_both_themes() {
        for theme in [LIGHT, DARK] {
            let select = theme.get(Swatch::TextSelect);
            assert_eq!(select.alpha(), 1.0, "opaque, drawn under the text");
            for fill in [
                Swatch::Bg,
                Swatch::Chrome,
                Swatch::Surface,
                Swatch::AddedBg,
                Swatch::RemovedBg,
                Swatch::Selected,
                Swatch::Hover,
            ] {
                assert_ne!(select, theme.get(fill), "{fill:?}");
            }
        }
    }

    #[test]
    fn added_lines_are_softer_than_green_soft() {
        assert_eq!(LIGHT.added_bg, Color::srgb_u8(0xE9, 0xF6, 0xEB));
        assert_eq!(DARK.added_bg, Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x0C));
        assert_eq!(DARK.get(Swatch::AddedBg), DARK.added_bg);
    }

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
    fn the_diagram_tokens_follow_the_mockups() {
        let get = |t: &Tokens, s| t.get(s);
        assert_eq!(
            get(&LIGHT, Swatch::CanvasDot),
            Color::srgb_u8(0xDD, 0xE2, 0xDD)
        );
        assert_eq!(
            get(&DARK, Swatch::CanvasDot),
            Color::srgb_u8(0x2A, 0x30, 0x2C)
        );
        assert_eq!(get(&LIGHT, Swatch::Edge), Color::srgb_u8(0xA4, 0xAC, 0xA6));
        assert_eq!(get(&DARK, Swatch::Edge), Color::srgb_u8(0x59, 0x62, 0x5B));
        assert_eq!(
            get(&LIGHT, Swatch::ChangedBg),
            Color::srgb_u8(0xEA, 0xF5, 0xEC)
        );
        assert_eq!(
            get(&DARK, Swatch::ChangedBg),
            Color::srgba_u8(0x4C, 0xC3, 0x5F, 0x1F)
        );
        assert_eq!(
            get(&LIGHT, Swatch::RiskBg),
            Color::srgb_u8(0xFB, 0xEE, 0xE9)
        );
        assert_eq!(
            get(&DARK, Swatch::RiskBg),
            Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x1F)
        );
        assert_eq!(get(&LIGHT, Swatch::Amber), Color::srgb_u8(0xB7, 0x79, 0x1F));
        assert_eq!(get(&DARK, Swatch::Amber), Color::srgb_u8(0xE0, 0xA4, 0x3A));
        assert_eq!(
            get(&LIGHT, Swatch::AmberSoft),
            Color::srgba_u8(0xB7, 0x79, 0x1F, 0x1A)
        );
        assert_eq!(
            get(&DARK, Swatch::AmberSoft),
            Color::srgba_u8(0xE0, 0xA4, 0x3A, 0x1F)
        );
        for tokens in [LIGHT, DARK] {
            assert_ne!(
                tokens.changed_bg, tokens.green_soft,
                "softer than the diff's tint"
            );
            assert_ne!(tokens.edge_soft, tokens.edge);
            assert_ne!(tokens.ghost_ink, tokens.fg);
        }
    }

    #[test]
    fn the_amber_is_readable_on_its_own_tint_over_the_surface() {
        // Relative luminance contrast of a title ink over `surface` with `soft` laid on it.
        fn lum(c: Color) -> f32 {
            let c = c.to_linear();
            0.2126 * c.red + 0.7152 * c.green + 0.0722 * c.blue
        }
        fn over(top: Color, below: Color) -> Color {
            let (t, b) = (top.to_srgba(), below.to_srgba());
            Color::srgb(
                t.red * t.alpha + b.red * (1.0 - t.alpha),
                t.green * t.alpha + b.green * (1.0 - t.alpha),
                t.blue * t.alpha + b.blue * (1.0 - t.alpha),
            )
        }
        for tokens in [LIGHT, DARK] {
            let ground = over(tokens.amber_soft, tokens.surface);
            let (a, b) = (lum(tokens.amber), lum(ground));
            let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(ratio >= 2.5, "amber on its tint: {ratio}");
        }
    }

    #[test]
    fn the_host_and_network_swatches_have_their_values() {
        let get = |t: &Tokens, s| t.get(s);
        assert_eq!(
            get(&LIGHT, Swatch::HostRow),
            Color::srgba_u8(0xE0, 0x9A, 0x1F, 0x2A)
        );
        assert_eq!(
            get(&LIGHT, Swatch::HostInk),
            Color::srgb_u8(0x7A, 0x4A, 0x08)
        );
        assert_eq!(
            get(&LIGHT, Swatch::NetBlue),
            Color::srgb_u8(0x1F, 0x4F, 0xB0)
        );
        assert_eq!(
            get(&LIGHT, Swatch::NetBlueSoft),
            Color::srgba_u8(0x2B, 0x6C, 0xD9, 0x1A)
        );
        assert_eq!(
            get(&DARK, Swatch::HostRow),
            Color::srgba_u8(0xE0, 0xA4, 0x3A, 0x1C)
        );
        assert_eq!(
            get(&DARK, Swatch::HostInk),
            Color::srgb_u8(0xF8, 0xD6, 0x96)
        );
        assert_eq!(
            get(&DARK, Swatch::NetBlue),
            Color::srgb_u8(0xCB, 0xE0, 0xFD)
        );
        assert_eq!(
            get(&DARK, Swatch::NetBlueSoft),
            Color::srgba_u8(0x6F, 0xA8, 0xF5, 0x2E)
        );
    }

    #[test]
    fn host_rows_and_network_tags_are_readable_in_both_themes() {
        for (name, tokens) in [("light", LIGHT), ("dark", DARK)] {
            let surface = tokens.surface.to_linear();
            let host = over(surface, tokens.host_row);
            let amber_tag_on_host = over(host, tokens.amber_soft);
            let amber_tag_on_surface = over(surface, tokens.amber_soft);
            let net = over(surface, tokens.net_blue_soft);
            let net_on_host = over(host, tokens.net_blue_soft);
            let ink = tokens.host_ink.to_linear();
            let blue = tokens.net_blue.to_linear();
            for (what, ratio) in [
                ("host ink on the host row", contrast(ink, host)),
                (
                    "host ink on the amber tag over a host row",
                    contrast(ink, amber_tag_on_host),
                ),
                (
                    "host ink on the amber tag over the surface",
                    contrast(ink, amber_tag_on_surface),
                ),
                ("network blue on its tag", contrast(blue, net)),
                (
                    "network blue on its tag over a host row",
                    contrast(blue, net_on_host),
                ),
            ] {
                assert!(ratio >= 4.5, "{name}: {what}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn muted_and_normal_text_are_readable_on_the_green_tint_in_both_themes() {
        for (name, tokens) in [("light", LIGHT), ("dark", DARK)] {
            for (under, base) in [("surface", tokens.surface), ("page", tokens.bg)] {
                let tint = over(base.to_linear(), tokens.green_tint);
                for (what, ink) in [("text", tokens.fg), ("muted text", tokens.muted)] {
                    let ratio = contrast(ink.to_linear(), tint);
                    assert!(
                        ratio >= 4.5,
                        "{name}: {what} on the tint over the {under}: {ratio:.2}"
                    );
                }
            }
        }
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
        assert_eq!(t.text_scale, 1.0, "medium until the config says otherwise");
        let large = t.with_text_size(TextSize::Large);
        assert_eq!(large.text_scale, TextSize::Large.scale());
        assert_eq!(
            large.code_size, 16.0,
            "the code size is scaled by the text, not here"
        );
        assert_eq!(Theme::new(false, 13, Density::Comfortable).tokens, LIGHT);
    }

    #[test]
    fn diff_and_modal_tokens_match_the_mockups() {
        assert_eq!(
            LIGHT.get(Swatch::RemovedBg),
            Color::srgb_u8(0xFC, 0xEE, 0xE8)
        );
        assert_eq!(DARK.removed_bg, Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x0C));
        assert_eq!(
            LIGHT.get(Swatch::AddedStrong),
            Color::srgba_u8(0x2F, 0x9E, 0x44, 0x42)
        );
        assert_eq!(
            DARK.get(Swatch::RemovedStrong),
            Color::srgba_u8(0xF0, 0x7A, 0x4A, 0x18)
        );
        assert_eq!(
            LIGHT.get(Swatch::Scrim),
            Color::srgba_u8(0xF6, 0xF7, 0xF5, 0x8C)
        );
        assert_eq!(
            DARK.get(Swatch::Scrim),
            Color::srgba_u8(0x08, 0x0A, 0x09, 0x99)
        );
    }

    #[test]
    fn syntax_colors_cover_every_class() {
        for tokens in [LIGHT, DARK] {
            assert_eq!(tokens.get(Swatch::Code(Class::Plain)), tokens.fg);
            // Comments have their own ink (readable on the diff tints); UI faint text stays put.
            assert_ne!(tokens.get(Swatch::Code(Class::Comment)), tokens.faint);
            for class in Class::ALL {
                assert_ne!(tokens.get(Swatch::Code(class)), tokens.surface, "{class:?}");
            }
        }
        assert_eq!(
            LIGHT.get(Swatch::Code(Class::String)),
            Color::srgb_u8(0x0A, 0x6C, 0x5A),
            "the table follows Class::ALL"
        );
        assert_eq!(
            DARK.get(Swatch::Code(Class::Keyword)),
            Color::srgb_u8(0xD3, 0xA9, 0xEE)
        );
        assert_eq!(LIGHT.faint, Color::srgb_u8(0x8A, 0x93, 0x8C));
        assert_eq!(DARK.faint, Color::srgb_u8(0x6B, 0x74, 0x6D));
    }

    /// `top` over `bottom`, in linear light: what the GPU does on the sRGB window target.
    fn over(bottom: LinearRgba, top: Color) -> LinearRgba {
        let t = top.to_linear();
        let mix = |b: f32, c: f32| b * (1.0 - t.alpha) + c * t.alpha;
        LinearRgba::rgb(
            mix(bottom.red, t.red),
            mix(bottom.green, t.green),
            mix(bottom.blue, t.blue),
        )
    }

    /// WCAG contrast ratio of an opaque ink on an opaque background.
    fn contrast(ink: LinearRgba, bg: LinearRgba) -> f32 {
        let lum = |c: LinearRgba| 0.2126 * c.red + 0.7152 * c.green + 0.0722 * c.blue;
        let (a, b) = (lum(ink), lum(bg));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn code_stays_readable_on_diff_tints_and_marks() {
        for (name, tokens) in [("light", LIGHT), ("dark", DARK)] {
            let surface = tokens.surface.to_linear();
            for (row, mark) in [
                (tokens.added_bg, tokens.added_strong),
                (tokens.removed_bg, tokens.removed_strong),
            ] {
                let tinted = over(surface, row);
                for bg in [surface, tinted, over(tinted, mark)] {
                    for class in Class::ALL {
                        let need = if class == Class::Comment { 3.0 } else { 4.5 };
                        let ratio = contrast(tokens.get(Swatch::Code(class)).to_linear(), bg);
                        assert!(ratio >= need, "{name} {class:?} on {bg:?}: {ratio:.2}");
                    }
                }
            }
        }
    }

    #[test]
    fn diff_tints_still_show() {
        for (name, tokens) in [("light", LIGHT), ("dark", DARK)] {
            let surface = tokens.surface.to_linear();
            for (row, mark) in [
                (tokens.added_bg, tokens.added_strong),
                (tokens.removed_bg, tokens.removed_strong),
            ] {
                let tinted = over(surface, row);
                let marked = over(tinted, mark);
                let apart = |a: LinearRgba, b: LinearRgba| {
                    let (a, b) = (Color::from(a).to_srgba(), Color::from(b).to_srgba());
                    (a.red - b.red).abs() + (a.green - b.green).abs() + (a.blue - b.blue).abs()
                };
                assert!(apart(surface, tinted) > 0.08, "{name}: the row tint shows");
                assert!(
                    apart(tinted, marked) > 0.08,
                    "{name}: the mark shows on the row"
                );
            }
        }
    }

    #[test]
    fn the_scene_tokens_are_the_mockups() {
        assert_eq!(LIGHT.scene_bg, Color::srgb_u8(0xFB, 0xFB, 0xF9));
        assert_eq!(DARK.scene_bg, Color::srgb_u8(0x12, 0x14, 0x12));
        assert_eq!(LIGHT.axis, Color::srgb_u8(0x9A, 0x9D, 0x96));
        assert_eq!(DARK.axis, Color::srgb_u8(0x6C, 0x70, 0x6A));
        assert_eq!(LIGHT.grid, Color::srgb_u8(0xE8, 0xE8, 0xE3));
        assert_eq!(DARK.grid, Color::srgb_u8(0x23, 0x26, 0x23));
        assert_eq!(LIGHT.get(Swatch::SceneBg), LIGHT.scene_bg);
        assert_eq!(DARK.get(Swatch::Axis), DARK.axis);
        assert_eq!(DARK.get(Swatch::Grid), DARK.grid);
        assert_eq!(LIGHT.get(Swatch::Cloud), LIGHT.cloud);
        assert_eq!(LIGHT.get(Swatch::Family(0)), LIGHT.family[0]);
        assert_eq!(LIGHT.get(Swatch::Family(11)), LIGHT.family[11]);
        assert_eq!(LIGHT.get(Swatch::Family(40)), LIGHT.family[11], "clamped");
        assert_eq!(DARK.get(Swatch::Elsewhere), DARK.elsewhere);
        for tokens in [LIGHT, DARK] {
            let a = tokens.cloud.to_srgba().alpha;
            assert!(
                (a - 0.07).abs() < 0.005,
                "a cloud is the axis colour at 7 %: {a}"
            );
        }
    }

    #[test]
    fn the_twelve_family_colours_read_on_the_scene_background_in_both_themes() {
        for (name, tokens) in [("light", LIGHT), ("dark", DARK)] {
            let bg = tokens.scene_bg.to_linear();
            for (i, c) in tokens.family.iter().enumerate() {
                let ratio = contrast(c.to_linear(), bg);
                assert!(ratio >= 3.0, "{name} family {i}: {ratio:.2}");
            }
            for (what, c) in [("ring", tokens.green), ("elsewhere", tokens.elsewhere)] {
                let ratio = contrast(c.to_linear(), bg);
                assert!(ratio >= 3.0, "{name} {what}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn the_twelve_family_colours_differ() {
        for tokens in [LIGHT, DARK] {
            for i in 0..12 {
                for j in (i + 1)..12 {
                    assert_ne!(tokens.family[i], tokens.family[j], "{i} and {j}");
                }
            }
        }
    }
}
