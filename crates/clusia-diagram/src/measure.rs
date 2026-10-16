//! Text measurement for the layout, from advance tables so the numbers are the same in tests,
//! screenshots and the app. A table holds a face's advances in 1/1000 em at weights 400 and
//! 600 for printable ASCII and the symbols the diagrams use: `Fonts` uses Inter's, built in,
//! and `FaceMeasure` any face's, read from its file, times the text scale. JetBrains Mono is
//! 0.6 em per character. Kerning is ignored, so every width gets `SLACK`.

use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextStyle {
    Title,
    TitleMono,
    Sub,
    SubStrong,
    Row,
    RowMono,
    /// The label of an edge, in the UI face.
    EdgeLabel,
    /// The label of an edge that names code (a call), in the monospace face.
    EdgeLabelMono,
    GroupLabel,
    /// Entity–relationship cardinalities such as `1` and `0..N`.
    Card,
}

pub trait Measure {
    fn width(&self, text: &str, style: TextStyle) -> f32;
    fn line_height(&self, style: TextStyle) -> f32;
}

/// Size in px, weight and whether the face is the monospace one.
pub fn style_size(style: TextStyle) -> (f32, u16, bool) {
    match style {
        TextStyle::Title => (14.0, 600, false),
        TextStyle::TitleMono => (13.0, 600, true),
        TextStyle::Sub => (11.0, 400, false),
        TextStyle::SubStrong => (11.0, 600, false),
        TextStyle::Row => (11.0, 400, false),
        TextStyle::RowMono => (11.0, 400, true),
        TextStyle::EdgeLabel => (11.0, 400, false),
        TextStyle::EdgeLabelMono => (11.0, 400, true),
        TextStyle::GroupLabel => (12.0, 600, false),
        TextStyle::Card => (11.0, 600, false),
    }
}

/// The advance tables behind `Measure`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fonts;

/// Widths are this much over the table sum, so a missing kern never makes a line wrap.
const SLACK: f32 = 1.02;
const MONO_EM: f32 = 0.6;
const FALLBACK_EM: f32 = 0.55;
const WIDE_EM: f32 = 1.0;

/// Inter advances for U+0020..=U+007E, in 1/1000 em.
const INTER_400: [u16; 95] = [
    281, 288, 466, 633, 642, 982, 644, 300, 365, 365, 501, 662, 288, 460, 288, 360, 631, 407, 610,
    618, 646, 593, 620, 566, 619, 620, 288, 302, 662, 662, 662, 511, 966, 690, 654, 730, 722, 601,
    590, 746, 743, 269, 571, 672, 565, 903, 753, 765, 639, 765, 644, 642, 646, 744, 690, 985, 682,
    679, 629, 365, 360, 365, 471, 456, 323, 562, 612, 571, 612, 583, 370, 613, 591, 242, 242, 549,
    242, 876, 591, 600, 612, 612, 376, 528, 327, 591, 562, 818, 546, 562, 552, 426, 333, 426, 662,
];
const INTER_600: [u16; 95] = [
    252, 321, 523, 644, 650, 1004, 663, 326, 373, 373, 540, 673, 319, 465, 319, 379, 660, 423, 623,
    636, 666, 612, 640, 576, 640, 640, 319, 329, 673, 673, 673, 543, 999, 728, 659, 737, 722, 605,
    588, 749, 746, 277, 580, 703, 565, 922, 759, 769, 645, 773, 652, 650, 660, 736, 728, 1020, 720,
    713, 652, 373, 379, 373, 481, 469, 351, 574, 624, 583, 624, 591, 389, 625, 612, 262, 262, 569,
    262, 900, 612, 609, 624, 624, 397, 549, 353, 612, 587, 839, 569, 588, 566, 455, 359, 455, 673,
];
/// Inter advances for the symbols diagrams use, as (400, 600) pairs in 1/1000 em.
const INTER_SYMBOLS: [(char, u16, u16); 21] = [
    ('→', 954, 954),
    ('−', 662, 673),
    ('·', 288, 319),
    ('–', 500, 500),
    ('—', 1000, 1000),
    ('…', 864, 956),
    ('«', 583, 634),
    ('»', 583, 634),
    ('◀', 917, 940),
    ('▶', 917, 940),
    ('●', 914, 914),
    ('✓', 884, 883),
    ('×', 662, 673),
    ('↔', 1341, 1341),
    ('↑', 834, 881),
    ('↓', 834, 881),
    ('≤', 662, 673),
    ('≥', 662, 673),
    ('↩', 1144, 1154),
    ('▲', 1011, 1034),
    ('▼', 1011, 1034),
];

/// East Asian wide characters and emoji, drawn about one em wide.
fn is_wide(c: char) -> bool {
    matches!(u32::from(c),
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
}

/// The advances of one face, in 1/1000 em, at weights 400 and 600; 0 is a character the face
/// has no glyph for, measured as `FALLBACK_EM`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advances {
    regular: [u16; 95],
    strong: [u16; 95],
    /// (character, 400, 600) for the symbols of `INTER_SYMBOLS`, in that order.
    symbols: Vec<(char, u16, u16)>,
}

impl Advances {
    /// Inter's built-in tables.
    pub fn inter() -> Self {
        Self {
            regular: INTER_400,
            strong: INTER_600,
            symbols: INTER_SYMBOLS.to_vec(),
        }
    }

    /// The characters a table holds: printable ASCII, then the symbols diagrams use.
    pub fn sampled() -> impl Iterator<Item = char> {
        (' '..='~').chain(INTER_SYMBOLS.iter().map(|&(c, _, _)| c))
    }

    /// A table from `advance(character, strong)`: the advance in em at weight 600 when
    /// `strong`, else 400; `None` when the face has no glyph for it.
    pub fn from_face(advance: impl Fn(char, bool) -> Option<f32>) -> Self {
        let milli = |c: char, strong: bool| {
            advance(c, strong).map_or(0, |em| (em * 1000.0).round().clamp(1.0, 4000.0) as u16)
        };
        let row = |strong: bool| std::array::from_fn(|i| milli(char::from(b' ' + i as u8), strong));
        Self {
            regular: row(false),
            strong: row(true),
            symbols: INTER_SYMBOLS
                .iter()
                .map(|&(c, _, _)| (c, milli(c, false), milli(c, true)))
                .collect(),
        }
    }

    /// The advance of one character in em.
    fn advance(&self, c: char, weight: u16, mono: bool) -> f32 {
        if c.is_control() {
            return 0.0;
        }
        if is_wide(c) {
            return WIDE_EM;
        }
        if mono {
            return MONO_EM;
        }
        let strong = weight >= 600;
        let milli = match c {
            ' '..='~' => {
                let table = if strong { &self.strong } else { &self.regular };
                table[usize::from(c as u8 - b' ')]
            }
            _ => self
                .symbols
                .iter()
                .find(|(s, _, _)| *s == c)
                .map_or(0, |&(_, w4, w6)| if strong { w6 } else { w4 }),
        };
        if milli == 0 {
            FALLBACK_EM
        } else {
            f32::from(milli) / 1000.0
        }
    }

    /// The width of `text` in px at `px` size, before `SLACK`.
    fn raw_width(&self, text: &str, px: f32, weight: u16, mono: bool) -> f32 {
        text.chars()
            .map(|c| self.advance(c, weight, mono))
            .sum::<f32>()
            * px
    }
}

/// The line height of `style` at the base sizes of `style_size`.
fn base_line_height(style: TextStyle) -> f32 {
    match style {
        TextStyle::Title | TextStyle::TitleMono | TextStyle::GroupLabel => 16.0,
        _ => 14.0,
    }
}

/// Inter's tables at the base sizes.
fn raw_width(text: &str, px: f32, weight: u16, mono: bool) -> f32 {
    static INTER: std::sync::OnceLock<Advances> = std::sync::OnceLock::new();
    INTER
        .get_or_init(Advances::inter)
        .raw_width(text, px, weight, mono)
}

impl Measure for Fonts {
    fn width(&self, text: &str, style: TextStyle) -> f32 {
        let (px, weight, mono) = style_size(style);
        raw_width(text, px, weight, mono) * SLACK
    }

    fn line_height(&self, style: TextStyle) -> f32 {
        base_line_height(style)
    }
}

/// The measure of text drawn in one face at `scale` times the sizes of `style_size` (the
/// window's text size). Code stays JetBrains Mono, so only the scale reaches it.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceMeasure {
    advances: Arc<Advances>,
    scale: f32,
}

impl FaceMeasure {
    pub fn new(advances: Arc<Advances>, scale: f32) -> Self {
        Self { advances, scale }
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }
}

impl Default for FaceMeasure {
    fn default() -> Self {
        Self::new(Arc::new(Advances::inter()), 1.0)
    }
}

impl Measure for FaceMeasure {
    fn width(&self, text: &str, style: TextStyle) -> f32 {
        let (px, weight, mono) = style_size(style);
        self.advances.raw_width(text, px * self.scale, weight, mono) * SLACK
    }

    fn line_height(&self, style: TextStyle) -> f32 {
        base_line_height(style) * self.scale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Widths measured from the bundled fonts with hinting off.
    #[test]
    fn the_tables_match_the_bundled_fonts() {
        let cases = [
            ("TokenStore::refresh", 13.0, 600, false, 127.5),
            ("TokenStore::refresh", 13.0, 400, false, 123.2),
            ("OAuth token endpoint", 13.0, 600, false, 137.8),
            ("HTTP client", 13.0, 600, false, 73.1),
            ("Refresh call, step by step", 13.0, 400, false, 155.7),
            ("+9 −2 · 1 draft comment", 11.0, 600, false, 126.4),
            ("exchange()", 11.0, 400, true, 66.0),
            ("refresh_lock: Mutex<()> · new", 11.0, 400, true, 191.4),
        ];
        for (text, px, weight, mono, expected) in cases {
            let got = raw_width(text, px, weight, mono);
            assert!(
                (got - expected).abs() <= expected * 0.02,
                "{text:?}: {got} vs {expected}"
            );
        }
    }

    #[test]
    fn measure_adds_slack_and_knows_the_styles() {
        let f = Fonts;
        let raw = raw_width("exchange()", 11.0, 400, true);
        assert!((f.width("exchange()", TextStyle::EdgeLabelMono) - raw * SLACK).abs() < 1e-4);
        assert_eq!(f.line_height(TextStyle::Title), 16.0);
        assert_eq!(f.line_height(TextStyle::Row), 14.0);
        assert_eq!(style_size(TextStyle::TitleMono), (13.0, 600, true));
        assert_eq!(style_size(TextStyle::Title), (14.0, 600, false));
        assert_eq!(style_size(TextStyle::EdgeLabel), (11.0, 400, false));
        assert_eq!(style_size(TextStyle::EdgeLabelMono), (11.0, 400, true));
        assert_eq!(f.width("", TextStyle::Title), 0.0);
    }

    #[test]
    fn a_face_measure_follows_its_advances_and_scale() {
        let inter = FaceMeasure::new(Arc::new(Advances::inter()), 1.0);
        for style in [TextStyle::Title, TextStyle::Row, TextStyle::RowMono] {
            let text = "Refresh call → step · 3";
            assert_eq!(inter.width(text, style), Fonts.width(text, style));
            assert_eq!(inter.line_height(style), Fonts.line_height(style));
        }
        let large = FaceMeasure::new(Arc::new(Advances::inter()), 1.5);
        let w = Fonts.width("TokenStore", TextStyle::Title);
        assert!((large.width("TokenStore", TextStyle::Title) - w * 1.5).abs() < 1e-3);
        assert!(
            (large.width("exchange()", TextStyle::RowMono)
                - Fonts.width("exchange()", TextStyle::RowMono) * 1.5)
                .abs()
                < 1e-3
        );
        assert_eq!(large.line_height(TextStyle::Title), 24.0);
        assert_eq!(large.line_height(TextStyle::Row), 21.0);
        assert_eq!(large.scale(), 1.5);
    }

    #[test]
    fn advances_are_filled_from_a_face_and_unknown_glyphs_fall_back() {
        // A face where every glyph is half an em, except that it has no `→` and a wide `W`.
        let face = Advances::from_face(|c, strong| match c {
            '→' => None,
            'W' if strong => Some(0.9),
            _ => Some(0.5),
        });
        let m = FaceMeasure::new(Arc::new(face), 1.0);
        let unit = |t: &str, s| m.width(t, s) / style_size(s).0 / SLACK;
        assert!((unit("ab", TextStyle::Row) - 1.0).abs() < 1e-4);
        assert!((unit("W", TextStyle::Title) - 0.9).abs() < 1e-4);
        assert!((unit("W", TextStyle::Row) - 0.5).abs() < 1e-4);
        assert!((unit("→", TextStyle::Row) - FALLBACK_EM).abs() < 1e-4);
        assert!(Advances::sampled().any(|c| c == '→'));
        assert_eq!(Advances::sampled().count(), 95 + INTER_SYMBOLS.len());
    }

    #[test]
    fn unknown_wide_and_control_characters_have_rules() {
        let f = Fonts;
        let unit = |c: &str| f.width(c, TextStyle::EdgeLabelMono) / 11.0;
        assert!((unit("日") - SLACK).abs() < 1e-4, "wide is one em");
        assert!(
            (unit("é") - MONO_EM * SLACK).abs() < 1e-4,
            "mono is 0.6 em for any glyph"
        );
        assert_eq!(f.width("\u{1b}", TextStyle::Title), 0.0);
        let sans = f.width("é", TextStyle::Title) / 14.0;
        assert!(
            (sans - FALLBACK_EM * SLACK).abs() < 1e-4,
            "unknown sans glyph"
        );
        assert!(f.width("→", TextStyle::Title) > f.width("-", TextStyle::Title));
    }
}
