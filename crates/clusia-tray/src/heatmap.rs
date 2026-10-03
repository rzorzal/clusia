//! Activity heatmap levels: 0 = nothing, 1–4 = quarter steps of the busiest day.

use clusia_core::DayCount;

pub fn levels(days: &[DayCount]) -> Vec<u8> {
    let max = days.iter().map(|d| d.count).max().unwrap_or(0);
    days.iter()
        .map(|d| {
            if d.count == 0 || max == 0 {
                0
            } else {
                (d.count * 4).div_ceil(max).clamp(1, 4) as u8
            }
        })
        .collect()
}
