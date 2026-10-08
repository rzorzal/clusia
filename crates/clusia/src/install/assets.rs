//! The files `clusia install` puts inside the bundle that are not binaries. They are compiled
//! in, so an install needs nothing but the executable and the freshly built binaries.

use clusia_core::config::SoundId;

/// Something copied into `Contents/Resources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asset {
    Icon,
    Sound(SoundId),
}

impl Asset {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Asset::Icon => include_bytes!("../../../../docs/assets/brand/Clusia.icns"),
            Asset::Sound(SoundId::Leaf) => include_bytes!("../../assets/sounds/leaf.aiff"),
            Asset::Sound(SoundId::Drop) => include_bytes!("../../assets/sounds/drop.aiff"),
            Asset::Sound(SoundId::Chime) => include_bytes!("../../assets/sounds/chime.aiff"),
            Asset::Sound(SoundId::Tick) => include_bytes!("../../assets/sounds/tick.aiff"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames in an AIFF and its sample rate, read from the `COMM` chunk.
    fn aiff_length(bytes: &[u8]) -> Option<f64> {
        if bytes.get(..4)? != b"FORM" || bytes.get(8..12)? != b"AIFF" {
            return None;
        }
        let comm = bytes.windows(4).position(|w| w == b"COMM")?;
        let body = &bytes[comm + 8..];
        let frames = u32::from_be_bytes(body.get(2..6)?.try_into().ok()?);
        let exponent = i32::from(u16::from_be_bytes(body.get(8..10)?.try_into().ok()?)) - 16383;
        let mantissa = u64::from_be_bytes(body.get(10..18)?.try_into().ok()?);
        let rate = mantissa as f64 / 2f64.powi(63 - exponent);
        Some(f64::from(frames) / rate)
    }

    #[test]
    fn every_sound_is_a_short_aiff() {
        for id in SoundId::ALL {
            let seconds = aiff_length(Asset::Sound(id).bytes())
                .unwrap_or_else(|| panic!("{} is not an AIFF", id.as_str()));
            assert!(
                (0.1..=0.6).contains(&seconds),
                "{} lasts {seconds} s",
                id.as_str()
            );
        }
    }

    #[test]
    fn the_sounds_are_distinct() {
        let all: Vec<_> = SoundId::ALL
            .iter()
            .map(|s| Asset::Sound(*s).bytes())
            .collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn the_icon_is_an_icns() {
        let icon = Asset::Icon.bytes();
        assert_eq!(&icon[..4], b"icns");
        assert!(icon.len() > 10_000, "all sizes are inside");
    }
}
