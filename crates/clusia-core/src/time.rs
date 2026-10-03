//! Minimal UTC date arithmetic (no timezone database): enough for GitHub timestamps and the activity heatmap.

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let (m, d) = (i64::from(m), i64::from(d));
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number from [`days_from_civil`].
pub fn civil_from_days(day: i64) -> (i64, u32, u32) {
    let z = day + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` → Unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = s.get(19..)?;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// The local day number of `unix`, given the local offset from UTC in seconds.
pub fn day_number(unix: i64, offset_secs: i64) -> i64 {
    (unix + offset_secs).div_euclid(86_400)
}

/// `YYYY-MM-DD` for a day number.
pub fn format_day(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn civil_round_trips() {
        for day in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day);
        }
    }

    #[test]
    fn parses_github_timestamps() {
        assert_eq!(parse_rfc3339("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(parse_rfc3339("2000-01-01T00:00:00.123Z"), Some(946_684_800));
        assert_eq!(
            parse_rfc3339("2000-01-01T02:00:00+02:00"),
            Some(946_684_800)
        );
        assert_eq!(
            parse_rfc3339("1999-12-31T21:30:00-02:30"),
            Some(946_684_800)
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for bad in [
            "",
            "2000-01-01",
            "2000-13-01T00:00:00Z",
            "2000-01-01T25:00:00Z",
            "2000-01-01T00:00:00",
            "2000-01-01T00:00:00.Z",
            "2000-01-01T00:00:00+0200",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn local_day_and_format() {
        let ts = 946_684_800; // 2000-01-01T00:00:00Z
        assert_eq!(format_day(day_number(ts, 0)), "2000-01-01");
        assert_eq!(format_day(day_number(ts, -3 * 3600)), "1999-12-31");
    }
}
