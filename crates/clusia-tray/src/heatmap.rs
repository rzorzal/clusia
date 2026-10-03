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

#[cfg(test)]
mod tests {
    use super::*;

    fn days(counts: &[u32]) -> Vec<DayCount> {
        counts
            .iter()
            .map(|&count| DayCount {
                date: "2026-10-01".into(),
                count,
            })
            .collect()
    }

    #[test]
    fn levels_scale_to_the_busiest_day() {
        assert_eq!(levels(&[]), Vec::<u8>::new());
        assert_eq!(levels(&days(&[0, 0])), vec![0, 0]);
        assert_eq!(levels(&days(&[1, 2, 3, 4])), vec![1, 2, 3, 4]);
        assert_eq!(levels(&days(&[1, 100])), vec![1, 4], "any activity shows");
        assert_eq!(levels(&days(&[0, 7])), vec![0, 4]);
    }
}
