//! The tray's only literal colors (sRGB). Text and
//! surfaces use macOS dynamic colors so light/dark follow the system.

pub const GREEN: (f64, f64, f64) = (0.184, 0.620, 0.267);
pub const ORANGE: (f64, f64, f64) = (0.851, 0.341, 0.169);
/// Heatmap opacity by level (0 uses the system's quaternary label color instead).
pub const HEAT_ALPHA: [f64; 5] = [0.0, 0.35, 0.55, 0.75, 1.0];
