//! Twemoji graphics and the emoji data behind the picker, the `:shortcode:` lookup and the
//! text splitter that puts a picture where a comment has an emoji.
//!
//! The 72×72 PNGs are compiled into the binary (`table::PNGS`) and decoded the first time a
//! file is drawn, so nothing is read from disk or fetched at run time.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::image::{CompressedImageFormats, ImageSampler, ImageType};
use bevy::prelude::*;

mod table;

pub use table::EMOJI;

/// The emojibase groups, in emojibase order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    SmileysEmotion,
    PeopleBody,
    Component,
    AnimalsNature,
    FoodDrink,
    TravelPlaces,
    Activities,
    Objects,
    Symbols,
    Flags,
}

impl Group {
    /// The groups a picker lists, in order. `Component` (bare skin-tone swatches and hair
    /// pieces) is not something to pick on its own.
    pub const PICKER: [Group; 9] = [
        Group::SmileysEmotion,
        Group::PeopleBody,
        Group::AnimalsNature,
        Group::FoodDrink,
        Group::TravelPlaces,
        Group::Activities,
        Group::Objects,
        Group::Symbols,
        Group::Flags,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::SmileysEmotion => "Smileys & emotion",
            Group::PeopleBody => "People & body",
            Group::Component => "Components",
            Group::AnimalsNature => "Animals & nature",
            Group::FoodDrink => "Food & drink",
            Group::TravelPlaces => "Travel & places",
            Group::Activities => "Activities",
            Group::Objects => "Objects",
            Group::Symbols => "Symbols",
            Group::Flags => "Flags",
        }
    }
}

/// One emoji. `file` is its Twemoji file name (`1f44d.png`), `emoji` the fully qualified
/// Unicode text, and `shortcodes` the names GitHub accepts between colons.
#[derive(Debug, PartialEq, Eq)]
pub struct EmojiEntry {
    pub emoji: &'static str,
    pub file: &'static str,
    pub label: &'static str,
    pub group: Group,
    pub tags: &'static [&'static str],
    pub shortcodes: &'static [&'static str],
}

/// A run of text, or the Twemoji file for one emoji.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece<'a> {
    Text(&'a str),
    Emoji(&'static str),
}

const ZWJ: char = '\u{200D}';
const VS16: char = '\u{FE0F}';
const KEYCAP: char = '\u{20E3}';

fn png_index(file: &str) -> Option<usize> {
    table::PNGS
        .binary_search_by(|(name, _)| (*name).cmp(file))
        .ok()
}

fn stem(cluster: &str, keep_vs16: bool) -> String {
    let mut out = String::new();
    for c in cluster.chars().filter(|&c| keep_vs16 || c != VS16) {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&format!("{:x}", c as u32));
    }
    out.push_str(".png");
    out
}

/// The Twemoji file for one emoji (grapheme cluster). Twemoji drops U+FE0F from names unless
/// the sequence has a ZWJ; a few ZWJ sequences drop it anyway, so the second try strips it
/// everywhere.
pub fn twemoji_file(cluster: &str) -> Option<&'static str> {
    if cluster.is_empty() {
        return None;
    }
    let first = stem(cluster, cluster.contains(ZWJ));
    if let Some(i) = png_index(&first) {
        return Some(table::PNGS[i].0);
    }
    png_index(&stem(cluster, false)).map(|i| table::PNGS[i].0)
}

/// The emoji GitHub writes as `:name:`.
pub fn by_shortcode(name: &str) -> Option<&'static EmojiEntry> {
    EMOJI.iter().find(|e| e.shortcodes.contains(&name))
}

/// Every emoji of one group, in emojibase order.
pub fn by_group(group: Group) -> impl Iterator<Item = &'static EmojiEntry> {
    EMOJI.iter().filter(move |e| e.group == group)
}

/// Emoji whose label, tags or shortcodes contain every word of `query` (case-insensitive),
/// best matches first: an exact shortcode, then a label or shortcode that starts with the
/// query, then the rest in emojibase order. Components are never offered; an empty query
/// matches nothing.
pub fn search(query: &str) -> Vec<&'static EmojiEntry> {
    let query = query.trim().to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, &'static EmojiEntry)> = EMOJI
        .iter()
        .filter(|e| e.group != Group::Component)
        .filter(|e| {
            words.iter().all(|w| {
                e.label.contains(w)
                    || e.tags.iter().any(|t| t.contains(w))
                    || e.shortcodes.iter().any(|s| s.contains(w))
            })
        })
        .map(|e| {
            let rank = if e.shortcodes.contains(&query.as_str()) {
                0
            } else if e.label.starts_with(&query)
                || e.shortcodes.iter().any(|s| s.starts_with(&query))
            {
                1
            } else {
                2
            };
            (rank, e)
        })
        .collect();
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().map(|(_, e)| e).collect()
}

fn is_skin_tone(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_tag(c: char) -> bool {
    ('\u{E0020}'..='\u{E007F}').contains(&c)
}

fn continues(c: char) -> bool {
    c == VS16 || c == ZWJ || c == KEYCAP || is_skin_tone(c) || is_tag(c)
}

/// A single character below U+1F000 is an emoji when it is one by default (✅, ⭐); the
/// text-style ones (©, ™, ❤) stay text unless U+FE0F follows.
fn text_style(single: char) -> bool {
    table::TEXT_STYLE.binary_search(&single).is_ok()
}

/// The longest emoji starting at `chars[k]`: the index just past it and its file.
fn emoji_at(text: &str, chars: &[(usize, char)], k: usize) -> Option<(usize, &'static str)> {
    let first = chars[k].1;
    if first.is_ascii() {
        let keycap_base = first.is_ascii_digit() || first == '#' || first == '*';
        let next = chars.get(k + 1).map(|c| c.1);
        if !(keycap_base && matches!(next, Some(VS16 | KEYCAP))) {
            return None;
        }
    }
    let mut end = k + 1;
    while let Some(&(_, next)) = chars.get(end) {
        let after_zwj = chars[end - 1].1 == ZWJ;
        let flag_pair = end == k + 1 && is_regional_indicator(first) && is_regional_indicator(next);
        if continues(next) || after_zwj || flag_pair {
            end += 1;
        } else {
            break;
        }
    }
    let start = chars[k].0;
    (k + 1..=end).rev().find_map(|e| {
        let stop = chars.get(e).map_or(text.len(), |c| c.0);
        let cluster = &text[start..stop];
        let file = twemoji_file(cluster)?;
        let lone_text_style = e == k + 1 && text_style(first);
        (!lone_text_style).then_some((e, file))
    })
}

/// Cuts `text` into runs of plain text and emoji, taking the longest emoji at each point
/// (a family or a flag is one piece, not its parts).
pub fn split_emoji(text: &str) -> Vec<Piece<'_>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut plain_from = 0;
    let mut k = 0;
    while k < chars.len() {
        match emoji_at(text, &chars, k) {
            Some((end, file)) => {
                let start = chars[k].0;
                if plain_from < start {
                    out.push(Piece::Text(&text[plain_from..start]));
                }
                out.push(Piece::Emoji(file));
                plain_from = chars.get(end).map_or(text.len(), |c| c.0);
                k = end;
            }
            None => k += 1,
        }
    }
    if plain_from < text.len() {
        out.push(Piece::Text(&text[plain_from..]));
    }
    out
}

/// Decoded Twemoji images, made the first time each file is asked for.
#[derive(Resource, Default)]
pub struct EmojiImages {
    by_file: HashMap<&'static str, Handle<Image>>,
}

impl EmojiImages {
    /// The image for a Twemoji file name. An unknown name, or bytes that do not decode, give
    /// the default (empty) handle and are not remembered.
    pub fn get(&mut self, file: &str, images: &mut Assets<Image>) -> Handle<Image> {
        let Some(i) = png_index(file) else {
            return Handle::default();
        };
        let (name, bytes) = table::PNGS[i];
        if let Some(handle) = self.by_file.get(name) {
            return handle.clone();
        }
        let decoded = Image::from_buffer(
            bytes,
            ImageType::Extension("png"),
            CompressedImageFormats::NONE,
            true,
            ImageSampler::linear(),
            RenderAssetUsages::RENDER_WORLD,
        );
        let Ok(image) = decoded else {
            return Handle::default();
        };
        let handle = images.add(image);
        self.by_file.insert(name, handle.clone());
        handle
    }
}

pub struct EmojiPlugin;

impl Plugin for EmojiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EmojiImages>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(text: &str) -> Vec<Piece<'_>> {
        split_emoji(text)
    }

    #[test]
    fn twemoji_names_follow_the_fe0f_rule() {
        assert_eq!(twemoji_file("❤\u{FE0F}"), Some("2764.png"));
        assert_eq!(
            twemoji_file("❤\u{FE0F}\u{200D}🔥"),
            Some("2764-fe0f-200d-1f525.png")
        );
        assert_eq!(twemoji_file("👍🏽"), Some("1f44d-1f3fd.png"));
        assert_eq!(twemoji_file("🇧🇷"), Some("1f1e7-1f1f7.png"));
        assert_eq!(twemoji_file("1\u{FE0F}\u{20E3}"), Some("31-20e3.png"));
        assert_eq!(
            twemoji_file("👁\u{FE0F}\u{200D}🗨\u{FE0F}"),
            Some("1f441-200d-1f5e8.png")
        );
        assert_eq!(twemoji_file("x"), None);
        assert_eq!(twemoji_file(""), None);
    }

    #[test]
    fn every_entry_has_its_image_and_the_table_is_sorted() {
        assert!(EMOJI.len() > 1800);
        for e in EMOJI {
            assert!(png_index(e.file).is_some(), "{} {}", e.label, e.file);
            assert_eq!(twemoji_file(e.emoji), Some(e.file), "{}", e.label);
        }
        let names: Vec<&str> = table::PNGS.iter().map(|(n, _)| *n).collect();
        assert!(names.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(names.len(), 4009);
        assert!(table::PNGS.iter().all(|(_, b)| b.starts_with(b"\x89PNG")));
    }

    #[test]
    fn shortcodes_find_emoji() {
        assert_eq!(by_shortcode("tada").map(|e| e.emoji), Some("🎉"));
        assert_eq!(by_shortcode("+1"), by_shortcode("thumbsup"));
        assert!(by_shortcode("thumbsup").is_some());
        assert_eq!(by_shortcode("not_an_emoji"), None);
    }

    #[test]
    fn search_ranks_and_skips_components() {
        assert_eq!(search("tada").first().map(|e| e.emoji), Some("🎉"));
        assert!(search("THUMBS").iter().any(|e| e.emoji.starts_with('👍')));
        let faces = search("grinning face");
        assert!(!faces.is_empty());
        assert!(
            faces
                .iter()
                .all(|e| e.label.contains("face") || e.tags.contains(&"face"))
        );
        assert!(
            search("skin tone")
                .iter()
                .all(|e| e.group != Group::Component)
        );
        assert!(search("").is_empty() && search("   ").is_empty());
        assert!(search("zzzzqqq").is_empty());
    }

    #[test]
    fn groups_list_in_order_without_components() {
        assert_eq!(Group::PICKER.len(), 9);
        assert!(!Group::PICKER.contains(&Group::Component));
        assert_eq!(Group::PICKER[0].label(), "Smileys & emotion");
        assert_eq!(Group::PICKER[8].label(), "Flags");
        assert!(by_group(Group::Flags).any(|e| e.emoji == "🇧🇷"));
        assert!(Group::PICKER.iter().all(|g| by_group(*g).next().is_some()));
    }

    #[test]
    fn split_puts_a_picture_where_the_emoji_is() {
        assert_eq!(
            pieces("ship it 🚀 now"),
            vec![
                Piece::Text("ship it "),
                Piece::Emoji("1f680.png"),
                Piece::Text(" now")
            ]
        );
        assert_eq!(
            pieces("✅🚀"),
            vec![Piece::Emoji("2705.png"), Piece::Emoji("1f680.png")]
        );
        assert_eq!(pieces("plain"), vec![Piece::Text("plain")]);
        assert!(pieces("").is_empty());
    }

    #[test]
    fn split_takes_the_longest_sequence() {
        assert_eq!(pieces("👨\u{200D}👩\u{200D}👧\u{200D}👦").len(), 1);
        assert_eq!(pieces("👍🏽"), vec![Piece::Emoji("1f44d-1f3fd.png")]);
        assert_eq!(pieces("🇧🇷🇯🇵").len(), 2);
        assert_eq!(pieces("a1\u{FE0F}\u{20E3}b").len(), 3);
        assert_eq!(
            pieces("👩\u{200D}"),
            vec![Piece::Emoji("1f469.png"), Piece::Text("\u{200D}")]
        );
    }

    #[test]
    fn split_leaves_text_style_symbols_and_plain_characters_alone() {
        assert_eq!(
            pieces("© 2026 ™ ação — é 1 2 3 # *"),
            vec![Piece::Text("© 2026 ™ ação — é 1 2 3 # *")]
        );
        assert_eq!(pieces("❤"), vec![Piece::Text("❤")]);
        assert_eq!(pieces("❤\u{FE0F}"), vec![Piece::Emoji("2764.png")]);
        assert_eq!(pieces("©\u{FE0F}"), vec![Piece::Emoji("a9.png")]);
    }

    #[test]
    fn images_decode_lazily_and_once() {
        let mut images = Assets::<Image>::default();
        let mut emoji = EmojiImages::default();
        assert_eq!(images.len(), 0);
        let a = emoji.get("1f680.png", &mut images);
        let b = emoji.get("1f680.png", &mut images);
        assert_eq!(a, b);
        assert_eq!(images.len(), 1);
        let image = images.get(&a).expect("decoded");
        assert_eq!((image.width(), image.height()), (72, 72));
        assert_eq!(emoji.get("nope.png", &mut images), Handle::default());
        assert_eq!(images.len(), 1);
    }
}
