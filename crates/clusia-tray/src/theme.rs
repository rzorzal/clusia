//! The tray's only literal colors (sRGB). Text and
//! surfaces use macOS dynamic colors so light/dark follow the system.

pub const GREEN: (f64, f64, f64) = (0.184, 0.620, 0.267);
pub const ORANGE: (f64, f64, f64) = (0.851, 0.341, 0.169);
/// #4CC35F: the brand green on dark surfaces.
pub const GREEN_DARK: (f64, f64, f64) = (0.298, 0.765, 0.373);
/// #F07A4A: the brand orange on dark surfaces.
pub const ORANGE_DARK: (f64, f64, f64) = (0.941, 0.478, 0.290);
/// Heatmap opacity by level (0 uses `fill` instead).
pub const HEAT_ALPHA: [f64; 5] = [0.0, 0.35, 0.55, 0.75, 1.0];

/// Base drawn over the vibrancy so any wallpaper reads the same (windowBackgroundColor alpha).
pub const BASE_ALPHA: f64 = 0.9;
/// Neutral fills for level-0 heatmap cells, panels and the toolbar group: (rgb, alpha).
pub const FILL_LIGHT: ((f64, f64, f64), f64) = ((0.0, 0.0, 0.0), 0.08);
pub const FILL_DARK: ((f64, f64, f64), f64) = ((1.0, 1.0, 1.0), 0.11);
/// Alpha applied to the secondary label for inert controls.
pub const DISABLED_ALPHA: f64 = 0.5;

/// Toolbar SF Symbol size in points.
pub const SYMBOL_POINT_SIZE: f64 = 15.0;

/// The neutral surface fill for the current appearance.
pub fn fill(dark: bool) -> ((f64, f64, f64), f64) {
    if dark { FILL_DARK } else { FILL_LIGHT }
}

/// The green for the current appearance.
pub fn green(dark: bool) -> (f64, f64, f64) {
    if dark { GREEN_DARK } else { GREEN }
}

/// The orange for the current appearance.
pub fn orange(dark: bool) -> (f64, f64, f64) {
    if dark { ORANGE_DARK } else { ORANGE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_accents_are_the_brighter_brand_values() {
        assert_eq!(green(false), GREEN);
        assert_eq!(orange(false), ORANGE);
        assert_eq!(green(true), GREEN_DARK);
        assert_eq!(orange(true), ORANGE_DARK);
        let luma = |(r, g, b): (f64, f64, f64)| 0.2126 * r + 0.7152 * g + 0.0722 * b;
        assert!(luma(GREEN_DARK) > luma(GREEN));
        assert!(luma(ORANGE_DARK) > luma(ORANGE));
    }
}
