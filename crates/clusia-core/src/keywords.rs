//! The keyword map of a pull request: the words an agent picked, the places Clúsia found them in
//! the repository, and where the agent put those places on three axes. Pure: no I/O.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::diagram::{
    CAPTION_MAX, Cleaner, CodeAnchor, History, LABEL_MAX, NOTE_MAX, PATH_MAX, count_of, fail,
    shown, valid_id,
};
use crate::printable::{is_invisible, plain_relative};

pub const MAX_FAMILIES: usize = 12;
/// The most words one family holds.
pub const MAX_WORDS: usize = 8;
pub const MAX_USE_CASES: usize = 8;
pub const WORD_MIN: usize = 2;
pub const WORD_MAX: usize = 64;
pub const MAX_REGIONS: usize = 300;
pub const MAX_PLACEMENT_FILES: usize = 200;
/// The most bytes the placement prompt holds.
pub const PLACEMENT_PROMPT_MAX: usize = 60_000;
pub const MAX_HITS_PER_WORD: u32 = 2_000;
pub const MAX_HITS: u32 = 20_000;
pub const SEARCH_BUDGET_SECS: u64 = 10;
pub const MAX_FILE_BYTES: u64 = 1_048_576;
pub const MAX_LINE_BYTES: usize = 4_096;
/// Places of one word in one file above this many bundle into one point.
pub const BUNDLE_OVER: usize = 12;
pub const MAX_POINTS: usize = 400;
/// The most bytes a stored keyword map may take as JSON.
pub const MAX_MAP_BYTES: usize = 3_145_728;
pub const KEYWORDS_KEY: &str = "keywords";
pub const KEYWORDS_TITLE: &str = "Where the words live";
/// The use case of every place that no region of the placement contains.
pub const ELSEWHERE_NAME: &str = "Elsewhere";
/// The info string of the fenced block that holds a keyword plan.
pub const KEYWORD_PLAN_INFO: &str = "clusia-keyword-plan";
/// The info string of the fenced block that holds a keyword placement.
pub const KEYWORD_PLACEMENT_INFO: &str = "clusia-keyword-placement";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Axis {
    pub name: String,
    pub low: String,
    pub high: String,
}

/// The line of the pull request a family grew from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seed {
    pub word: String,
    pub path: String,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Family {
    pub id: String,
    pub label: String,
    pub seed: Seed,
    pub words: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<History>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UseCase {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<History>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeywordPlan {
    pub title: String,
    #[serde(default)]
    pub caption: String,
    pub axes: [Axis; 3],
    pub families: Vec<Family>,
    pub use_cases: Vec<UseCase>,
}

impl KeywordPlan {
    /// Every word of every family, families in order and each family's words in order: the order
    /// of `KeywordMap::words` and of its `overflow`.
    pub fn all_words(&self) -> Vec<String> {
        self.families
            .iter()
            .flat_map(|f| f.words.iter().cloned())
            .collect()
    }
}

/// Part of a file (or all of it) that belongs to one use case, and where it sits on the axes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
    pub use_case: String,
    pub at: [f32; 3],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeywordPlacement {
    pub regions: Vec<Region>,
}

/// Path → new-side line numbers the pull request adds.
pub type AddedLines = BTreeMap<String, BTreeSet<u32>>;

pub const DECLARED: u8 = 1;
pub const CHANGED: u8 = 2;
pub const SAME_NAME: u8 = 4;
/// The `region` of a place that no region contains.
pub const NO_REGION: u16 = u16::MAX;

/// One place; stored as the array `[path, line, word, region, flags]`, each an index into the
/// tables of the map but the line and the flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub path: u32,
    pub line: u32,
    pub word: u16,
    pub region: u16,
    pub flags: u8,
}

impl Place {
    pub fn declared(&self) -> bool {
        self.flags & DECLARED != 0
    }

    pub fn changed(&self) -> bool {
        self.flags & CHANGED != 0
    }

    pub fn same_name(&self) -> bool {
        self.flags & SAME_NAME != 0
    }
}

impl Serialize for Place {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.path, self.line, self.word, self.region, self.flags).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Place {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (path, line, word, region, flags) =
            <(u32, u32, u16, u16, u8)>::deserialize(deserializer)?;
        Ok(Place {
            path,
            line,
            word,
            region,
            flags,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapWord {
    pub text: String,
    pub family: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeywordMap {
    pub plan: KeywordPlan,
    /// The validated placement.
    pub regions: Vec<Region>,
    pub paths: Vec<String>,
    pub words: Vec<MapWord>,
    /// Sorted by (path, line, word).
    pub places: Vec<Place>,
    /// Per word: the hits found beyond the caps, counted and not listed.
    pub overflow: Vec<u32>,
    /// The search ran out of its time.
    #[serde(default)]
    pub search_cut: bool,
}

impl KeywordMap {
    /// The index into `plan.use_cases` of the use case of `place`; `None` is Elsewhere.
    pub fn use_case_of(&self, place: &Place) -> Option<usize> {
        let region = self.regions.get(usize::from(place.region))?;
        self.plan
            .use_cases
            .iter()
            .position(|u| u.id == region.use_case)
    }

    /// The index into `plan.families` of the family of the word of `place`.
    pub fn family_of(&self, place: &Place) -> usize {
        self.words
            .get(usize::from(place.word))
            .map_or(0, |w| usize::from(w.family))
    }

    /// The line the place at `index` stands for, as the code modal and a comment read it.
    pub fn anchor(&self, index: usize) -> Option<CodeAnchor> {
        let place = self.places.get(index)?;
        let path = self.paths.get(usize::try_from(place.path).ok()?)?;
        Some(CodeAnchor {
            path: path.clone(),
            start_line: place.line,
            end_line: place.line,
        })
    }

    /// The places found plus the ones counted past the caps.
    pub fn total_places(&self) -> u32 {
        let found = u32::try_from(self.places.len()).unwrap_or(u32::MAX);
        self.overflow
            .iter()
            .fold(found, |sum, n| sum.saturating_add(*n))
    }
}

/// The node name of the place at `index` in the protocol's `node` field.
pub fn place_node(index: usize) -> String {
    format!("p{index}")
}

/// The index in a node name made by `place_node`, and nothing else.
pub fn parse_place_node(node: &str) -> Option<usize> {
    let digits = node.strip_prefix('p')?;
    let plain = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if plain { digits.parse().ok() } else { None }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckedPlan {
    pub plan: KeywordPlan,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckedPlacement {
    pub placement: KeywordPlacement,
    pub notes: Vec<String>,
}

/// Whether `word` is 2 to 64 characters with no whitespace, control or invisible character.
fn valid_word(word: &str) -> bool {
    (WORD_MIN..=WORD_MAX).contains(&word.chars().count())
        && !word
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || is_invisible(c))
}

/// Checks the answer of the first keyword turn against the lines the pull request adds.
///
/// A family whose seed is not an added line, whose seed word is not a valid word or already
/// belongs to an earlier family, and every family past `MAX_FAMILIES` is dropped with a note;
/// words that are not valid, repeat one of an earlier family or are past `MAX_WORDS` are dropped
/// too, and the seed word always stays first. Broken ids, a missing use case and too many use
/// cases are errors to tell the agent. Text over its cap is cut.
pub fn validate_keyword_plan(
    raw: KeywordPlan,
    added: &AddedLines,
) -> Result<CheckedPlan, Vec<String>> {
    let mut errors = Vec::new();
    let mut cleaner = Cleaner::default();

    let mut axes = Vec::new();
    for axis in &raw.axes {
        let name = cleaner.text(&axis.name, LABEL_MAX);
        if name.is_empty() {
            fail(&mut errors, "An axis has no name.".into());
        }
        axes.push(Axis {
            name,
            low: cleaner.text(&axis.low, LABEL_MAX),
            high: cleaner.text(&axis.high, LABEL_MAX),
        });
    }
    let Ok(axes) = <[Axis; 3]>::try_from(axes) else {
        return Err(vec!["The plan needs three axes.".into()]);
    };

    if raw.use_cases.is_empty() {
        fail(&mut errors, "The plan has no use cases.".into());
    }
    if raw.use_cases.len() > MAX_USE_CASES {
        fail(
            &mut errors,
            format!(
                "There are {} use cases; the most is {MAX_USE_CASES}.",
                raw.use_cases.len()
            ),
        );
    }
    let mut use_case_ids = HashSet::new();
    let mut use_cases = Vec::new();
    for use_case in raw.use_cases.into_iter().take(MAX_USE_CASES) {
        if !valid_id(&use_case.id) {
            fail(
                &mut errors,
                format!(
                    "The use case id \"{}\" is not allowed: use 1 to 64 letters, digits, \"_\", \".\" or \"-\".",
                    shown(&use_case.id)
                ),
            );
        } else if !use_case_ids.insert(use_case.id.clone()) {
            fail(
                &mut errors,
                format!("Two use cases share the id \"{}\".", shown(&use_case.id)),
            );
        }
        let name = cleaner.text(&use_case.name, LABEL_MAX);
        if name.is_empty() {
            fail(
                &mut errors,
                format!("The use case \"{}\" has no name.", shown(&use_case.id)),
            );
        }
        use_cases.push(UseCase {
            name,
            note: cleaner.optional(use_case.note.as_deref(), NOTE_MAX),
            ..use_case
        });
    }

    let mut family_ids = HashSet::new();
    let mut taken: HashSet<String> = HashSet::new();
    let mut families = Vec::new();
    let (mut past_cap, mut bad_seed, mut seed_word, mut bad_words, mut extra_words) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    for family in raw.families {
        if !valid_id(&family.id) {
            fail(
                &mut errors,
                format!(
                    "The family id \"{}\" is not allowed: use 1 to 64 letters, digits, \"_\", \".\" or \"-\".",
                    shown(&family.id)
                ),
            );
            continue;
        }
        if !family_ids.insert(family.id.clone()) {
            fail(
                &mut errors,
                format!("Two families share the id \"{}\".", shown(&family.id)),
            );
            continue;
        }
        if families.len() >= MAX_FAMILIES {
            past_cap += 1;
            continue;
        }
        let seed = &family.seed;
        let on_added_line = plain_relative(&seed.path, PATH_MAX)
            && added
                .get(&seed.path)
                .is_some_and(|l| l.contains(&seed.line));
        if !on_added_line {
            bad_seed += 1;
            continue;
        }
        if !valid_word(&seed.word) || taken.contains(&seed.word) {
            seed_word += 1;
            continue;
        }
        let mut words = vec![seed.word.clone()];
        let mut mine: HashSet<String> = HashSet::from([seed.word.clone()]);
        for word in &family.words {
            if *word == seed.word {
                continue;
            }
            if !valid_word(word) || taken.contains(word) || mine.contains(word) {
                bad_words += 1;
            } else if words.len() >= MAX_WORDS {
                extra_words += 1;
            } else {
                mine.insert(word.clone());
                words.push(word.clone());
            }
        }
        taken.extend(words.iter().cloned());
        let label = cleaner.text(&family.label, LABEL_MAX);
        families.push(Family {
            label: if label.is_empty() {
                seed.word.clone()
            } else {
                label
            },
            words,
            ..family
        });
    }

    let title = cleaner.text(&raw.title, LABEL_MAX);
    let caption = cleaner.text(&raw.caption, CAPTION_MAX);
    if !errors.is_empty() {
        return Err(errors);
    }
    if families.is_empty() {
        return Err(vec!["No word of this change could be traced.".into()]);
    }

    let mut notes = Vec::new();
    for (n, one, many, why) in [
        (
            bad_seed,
            "family",
            "families",
            "its seed is not a line this change adds",
        ),
        (
            seed_word,
            "family",
            "families",
            "its seed word is not a word or belongs to an earlier family",
        ),
        (
            past_cap,
            "family",
            "families",
            "more than the 12 a plan holds",
        ),
        (
            bad_words,
            "word",
            "words",
            "not a word of 2 to 64 characters, or already in a family",
        ),
        (
            extra_words,
            "word",
            "words",
            "more than the 8 a family holds",
        ),
    ] {
        if n > 0 {
            notes.push(format!("{} dropped: {why}", count_of(n, one, many)));
        }
    }
    if cleaner.cut > 0 {
        notes.push(format!(
            "{} shortened",
            count_of(cleaner.cut, "text", "texts")
        ));
    }
    Ok(CheckedPlan {
        plan: KeywordPlan {
            title: if title.is_empty() {
                KEYWORDS_TITLE.into()
            } else {
                title
            },
            caption,
            axes,
            families,
            use_cases,
        },
        notes,
    })
}

/// Checks the answer of the second keyword turn: `files` are the paths the search found words
/// in. A region whose file was not searched, whose use case is not in `plan`, whose position is
/// not a number or whose lines are missing, zero or reversed is dropped with a note, and so is
/// every region past `MAX_REGIONS`; positions are clamped to 0 to 1. No region at all is valid:
/// every place is then Elsewhere.
pub fn validate_placement(
    raw: KeywordPlacement,
    plan: &KeywordPlan,
    files: &BTreeSet<String>,
) -> Result<CheckedPlacement, Vec<String>> {
    let known: HashSet<&str> = plan.use_cases.iter().map(|u| u.id.as_str()).collect();
    let mut regions = Vec::new();
    let (mut unsearched, mut unknown, mut not_number, mut lines, mut past_cap, mut clamped) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    for mut region in raw.regions {
        if !files.contains(&region.path) {
            unsearched += 1;
        } else if !known.contains(region.use_case.as_str()) {
            unknown += 1;
        } else if region.at.iter().any(|v| !v.is_finite()) {
            not_number += 1;
        } else if !match (region.start_line, region.end_line) {
            (None, None) => true,
            (Some(start), Some(end)) => start >= 1 && start <= end,
            _ => false,
        } {
            lines += 1;
        } else if regions.len() >= MAX_REGIONS {
            past_cap += 1;
        } else {
            if region.at.iter().any(|v| !(0.0..=1.0).contains(v)) {
                clamped += 1;
            }
            region.at = region.at.map(|v| v.clamp(0.0, 1.0));
            regions.push(region);
        }
    }
    let mut notes = Vec::new();
    for (n, why) in [
        (unsearched, "its file was not one of the files listed"),
        (unknown, "its use case is not in the plan"),
        (not_number, "a position is not a number"),
        (
            lines,
            "its lines are missing one end, start at 0 or run backwards",
        ),
        (past_cap, "more than the 300 a placement holds"),
    ] {
        if n > 0 {
            notes.push(format!(
                "{} dropped: {why}",
                count_of(n, "region", "regions")
            ));
        }
    }
    if clamped > 0 {
        notes.push(format!(
            "{} clamped into 0 to 1",
            count_of(clamped, "position", "positions")
        ));
    }
    Ok(CheckedPlacement {
        placement: KeywordPlacement { regions },
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axes() -> [Axis; 3] {
        ["Layer", "Client", "Storage"].map(|n| Axis {
            name: n.into(),
            low: "low".into(),
            high: "high".into(),
        })
    }

    fn family(id: &str, seed: &str, path: &str, line: u32, words: &[&str]) -> Family {
        Family {
            id: id.into(),
            label: format!("The {id} family"),
            seed: Seed {
                word: seed.into(),
                path: path.into(),
                line,
            },
            words: words.iter().map(|w| w.to_string()).collect(),
            history: None,
        }
    }

    fn use_case(id: &str) -> UseCase {
        UseCase {
            id: id.into(),
            name: format!("Use case {id}"),
            note: None,
            history: None,
        }
    }

    fn plan() -> KeywordPlan {
        KeywordPlan {
            title: "Token refresh".into(),
            caption: "Where the refresh words live.".into(),
            axes: axes(),
            families: vec![family(
                "retry",
                "retry",
                "src/auth/refresh.rs",
                40,
                &["retry", "retry_count", "RetryPolicy"],
            )],
            use_cases: vec![use_case("auth"), use_case("http")],
        }
    }

    fn added() -> AddedLines {
        let mut added = AddedLines::new();
        added.insert("src/auth/refresh.rs".into(), (40..=60).collect());
        added.insert("src/lib.rs".into(), BTreeSet::from([1, 2]));
        added
    }

    fn files(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn a_place_is_stored_as_a_five_element_array() {
        let place = Place {
            path: 3,
            line: 44,
            word: 1,
            region: NO_REGION,
            flags: DECLARED | CHANGED,
        };
        let json = serde_json::to_string(&place).unwrap();
        assert_eq!(json, "[3,44,1,65535,3]");
        assert_eq!(serde_json::from_str::<Place>(&json).unwrap(), place);
        assert!(place.declared() && place.changed() && !place.same_name());
        assert!(serde_json::from_str::<Place>("[1,2,3]").is_err());
        assert!(serde_json::from_str::<Place>("[1,2,3,4,300]").is_err());
    }

    fn map() -> KeywordMap {
        let mut plan = plan();
        plan.families.push(family(
            "token",
            "token",
            "src/auth/refresh.rs",
            41,
            &["token"],
        ));
        KeywordMap {
            plan,
            regions: vec![Region {
                path: "src/http.rs".into(),
                start_line: None,
                end_line: None,
                use_case: "http".into(),
                at: [0.5; 3],
            }],
            paths: vec!["src/auth/refresh.rs".into(), "src/http.rs".into()],
            words: vec![
                MapWord {
                    text: "retry".into(),
                    family: 0,
                },
                MapWord {
                    text: "token".into(),
                    family: 1,
                },
            ],
            places: vec![
                Place {
                    path: 0,
                    line: 40,
                    word: 0,
                    region: NO_REGION,
                    flags: CHANGED,
                },
                Place {
                    path: 1,
                    line: 9,
                    word: 1,
                    region: 0,
                    flags: 0,
                },
            ],
            overflow: vec![5, 0],
            search_cut: false,
        }
    }

    #[test]
    fn a_map_answers_for_its_places() {
        let map = map();
        assert_eq!(map.use_case_of(&map.places[0]), None);
        assert_eq!(map.use_case_of(&map.places[1]), Some(1));
        assert_eq!(map.family_of(&map.places[0]), 0);
        assert_eq!(map.family_of(&map.places[1]), 1);
        assert_eq!(
            map.anchor(1),
            Some(CodeAnchor {
                path: "src/http.rs".into(),
                start_line: 9,
                end_line: 9
            })
        );
        assert_eq!(map.anchor(2), None);
        assert_eq!(map.total_places(), 7);
        let mut huge = map.clone();
        huge.overflow = vec![u32::MAX, 9];
        assert_eq!(huge.total_places(), u32::MAX);
    }

    #[test]
    fn a_map_survives_json_with_places_as_tuples() {
        let map = map();
        let json = serde_json::to_string(&map).unwrap();
        assert!(json.contains("\"places\":[[0,40,0,65535,2],[1,9,1,0,0]]"));
        assert_eq!(serde_json::from_str::<KeywordMap>(&json).unwrap(), map);
        let old = json.replace(",\"search_cut\":false", "");
        assert!(!serde_json::from_str::<KeywordMap>(&old).unwrap().search_cut);
    }

    #[test]
    fn place_nodes_round_trip_and_nothing_else_parses() {
        assert_eq!(place_node(0), "p0");
        assert_eq!(place_node(137), "p137");
        assert_eq!(parse_place_node("p137"), Some(137));
        assert_eq!(parse_place_node("p0"), Some(0));
        for bad in [
            "",
            "p",
            "137",
            "p07",
            "p-1",
            "p+1",
            "p1x",
            "P1",
            "p 1",
            "p99999999999999999999",
        ] {
            assert_eq!(parse_place_node(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn all_words_are_the_families_in_order() {
        let map = map();
        assert_eq!(
            map.plan.all_words(),
            ["retry", "retry_count", "RetryPolicy", "token"]
        );
    }

    #[test]
    fn a_good_plan_passes_unchanged() {
        let checked = validate_keyword_plan(plan(), &added()).unwrap();
        assert_eq!(checked.plan, plan());
        assert!(checked.notes.is_empty());
    }

    #[test]
    fn a_family_whose_seed_is_not_an_added_line_is_dropped_with_a_note() {
        let mut raw = plan();
        raw.families.push(family(
            "old",
            "legacy",
            "src/auth/refresh.rs",
            10,
            &["legacy"],
        ));
        raw.families
            .push(family("gone", "gone", "src/other.rs", 40, &["gone"]));
        let checked = validate_keyword_plan(raw, &added()).unwrap();
        assert_eq!(checked.plan.families.len(), 1);
        assert_eq!(
            checked.notes,
            ["2 families dropped: its seed is not a line this change adds"]
        );
    }

    #[test]
    fn a_plan_with_no_traceable_family_is_an_error() {
        let mut raw = plan();
        raw.families[0].seed.line = 3;
        assert_eq!(
            validate_keyword_plan(raw, &added()).unwrap_err(),
            ["No word of this change could be traced."]
        );
        let mut traversal = plan();
        traversal.families[0].seed.path = "../x".into();
        let mut added = added();
        added.insert("../x".into(), BTreeSet::from([40]));
        assert!(validate_keyword_plan(traversal, &added).is_err());
    }

    #[test]
    fn forty_families_are_capped_at_twelve() {
        let mut raw = plan();
        raw.families.clear();
        let mut added = AddedLines::new();
        added.insert("src/a.rs".into(), (1..=40).collect());
        for n in 1..=40u32 {
            let word = format!("word{n}");
            raw.families
                .push(family(&format!("f{n}"), &word, "src/a.rs", n, &[&word]));
        }
        let checked = validate_keyword_plan(raw, &added).unwrap();
        assert_eq!(checked.plan.families.len(), MAX_FAMILIES);
        assert_eq!(checked.plan.families[11].id, "f12");
        assert_eq!(
            checked.notes,
            ["28 families dropped: more than the 12 a plan holds"]
        );
    }

    #[test]
    fn words_are_checked_deduplicated_and_capped() {
        let mut raw = plan();
        raw.families[0].words = vec![
            "retry".into(),
            "retry_count".into(),
            "retry count".into(),
            "-e".into(),
            "x".into(),
            "a\u{1b}[2Jb".into(),
            "tab\there".into(),
            "-e".into(),
            "w3".into(),
            "w4".into(),
            "w5".into(),
            "w6".into(),
            "w7".into(),
            "w8".into(),
            "w9".into(),
        ];
        raw.families.push(family(
            "second",
            "retry_count",
            "src/auth/refresh.rs",
            41,
            &["retry_count"],
        ));
        raw.families.push(family(
            "third",
            "third",
            "src/auth/refresh.rs",
            42,
            &["third", "w3"],
        ));
        let checked = validate_keyword_plan(raw, &added()).unwrap();
        let first = &checked.plan.families[0];
        assert_eq!(first.words[0], "retry");
        assert_eq!(first.words.len(), MAX_WORDS);
        assert!(first.words.contains(&"-e".to_string()));
        assert!(!first.words.iter().any(|w| w.contains(' ') || w.len() < 2));
        assert_eq!(
            checked.plan.families.len(),
            2,
            "the second family's seed word is taken"
        );
        assert_eq!(checked.plan.families[1].words, ["third"]);
        assert!(
            checked
                .notes
                .iter()
                .any(|n| n.contains("its seed word is not a word or belongs to an earlier family"))
        );
        assert!(
            checked.notes.contains(
                &"6 words dropped: not a word of 2 to 64 characters, or already in a family"
                    .to_string()
            )
        );
        assert!(
            checked
                .notes
                .contains(&"2 words dropped: more than the 8 a family holds".to_string())
        );
    }

    #[test]
    fn a_word_of_65_characters_or_with_an_invisible_character_is_not_a_word() {
        let mut raw = plan();
        raw.families[0].words = vec![
            "w".repeat(WORD_MAX),
            "w".repeat(WORD_MAX + 1),
            "zero\u{200b}width".into(),
        ];
        let checked = validate_keyword_plan(raw, &added()).unwrap();
        assert_eq!(checked.plan.families[0].words.len(), 2);
    }

    #[test]
    fn texts_are_trimmed_cut_and_made_printable() {
        let mut raw = plan();
        raw.axes[0].name = "  Layer\u{1b}[2J\u{202e}  ".into();
        raw.caption = "c".repeat(300);
        raw.families[0].label = "   ".into();
        raw.use_cases[0].note = Some("n".repeat(500));
        let checked = validate_keyword_plan(raw, &added()).unwrap();
        let plan = checked.plan;
        assert_eq!(plan.axes[0].name, "Layer\\u{1b}[2J\\u{202e}");
        assert_eq!(plan.caption.chars().count(), CAPTION_MAX);
        assert!(plan.caption.ends_with('…'));
        assert_eq!(
            plan.families[0].label, "retry",
            "an empty label falls back to the seed word"
        );
        assert_eq!(
            plan.use_cases[0].note.as_ref().unwrap().chars().count(),
            NOTE_MAX
        );
        assert_eq!(checked.notes, ["2 texts shortened"]);
    }

    #[test]
    fn broken_ids_and_use_case_counts_are_errors() {
        let mut raw = plan();
        raw.families[0].id = "a b".into();
        raw.use_cases.push(use_case("auth"));
        raw.axes[1].name = "".into();
        let errors = validate_keyword_plan(raw, &added()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("The family id \"a b\" is not allowed"))
        );
        assert!(errors.contains(&"Two use cases share the id \"auth\".".to_string()));
        assert!(errors.contains(&"An axis has no name.".to_string()));

        let mut none = plan();
        none.use_cases.clear();
        assert_eq!(
            validate_keyword_plan(none, &added()).unwrap_err(),
            ["The plan has no use cases."]
        );
        let mut many = plan();
        many.use_cases = (0..9).map(|n| use_case(&format!("u{n}"))).collect();
        assert_eq!(
            validate_keyword_plan(many, &added()).unwrap_err(),
            ["There are 9 use cases; the most is 8."]
        );
        let mut twins = plan();
        twins.families.push(family(
            "retry",
            "token",
            "src/auth/refresh.rs",
            41,
            &["token"],
        ));
        assert_eq!(
            validate_keyword_plan(twins, &added()).unwrap_err(),
            ["Two families share the id \"retry\"."]
        );
    }

    #[test]
    fn history_survives_validation() {
        let mut raw = plan();
        raw.families[0].history = Some(History::Changed);
        raw.use_cases[1].history = Some(History::Removed);
        let checked = validate_keyword_plan(raw, &added()).unwrap();
        assert_eq!(checked.plan.families[0].history, Some(History::Changed));
        assert_eq!(checked.plan.use_cases[1].history, Some(History::Removed));
    }

    fn region(path: &str, use_case: &str, lines: Option<(u32, u32)>, at: [f32; 3]) -> Region {
        Region {
            path: path.into(),
            start_line: lines.map(|l| l.0),
            end_line: lines.map(|l| l.1),
            use_case: use_case.into(),
            at,
        }
    }

    #[test]
    fn a_placement_keeps_good_regions_and_clamps_positions() {
        let raw = KeywordPlacement {
            regions: vec![
                region("src/a.rs", "auth", None, [0.1, 0.2, 0.3]),
                region("src/a.rs", "http", Some((10, 20)), [2.0, -1.0, 0.5]),
            ],
        };
        let checked = validate_placement(raw, &plan(), &files(&["src/a.rs"])).unwrap();
        assert_eq!(checked.placement.regions.len(), 2);
        assert_eq!(checked.placement.regions[1].at, [1.0, 0.0, 0.5]);
        assert_eq!(checked.notes, ["1 position clamped into 0 to 1"]);
    }

    #[test]
    fn a_placement_drops_what_cannot_be_used_and_says_so() {
        let raw = KeywordPlacement {
            regions: vec![
                region("src/a.rs", "auth", None, [0.5; 3]),
                region("src/missing.rs", "auth", None, [0.5; 3]),
                region("src/a.rs", "nope", None, [0.5; 3]),
                region("src/a.rs", "auth", None, [f32::NAN, 2.0, -1.0]),
                region("src/a.rs", "auth", None, [0.5, f32::INFINITY, 0.5]),
                region("src/a.rs", "auth", Some((20, 10)), [0.5; 3]),
                region("src/a.rs", "auth", Some((0, 3)), [0.5; 3]),
                Region {
                    end_line: None,
                    ..region("src/a.rs", "auth", Some((4, 9)), [0.5; 3])
                },
            ],
        };
        let checked = validate_placement(raw, &plan(), &files(&["src/a.rs"])).unwrap();
        assert_eq!(checked.placement.regions.len(), 1);
        assert_eq!(
            checked.notes,
            [
                "1 region dropped: its file was not one of the files listed",
                "1 region dropped: its use case is not in the plan",
                "2 regions dropped: a position is not a number",
                "3 regions dropped: its lines are missing one end, start at 0 or run backwards",
            ]
        );
    }

    #[test]
    fn a_placement_is_capped_and_may_be_empty() {
        let raw = KeywordPlacement {
            regions: (0..MAX_REGIONS + 5)
                .map(|_| region("src/a.rs", "auth", None, [0.5; 3]))
                .collect(),
        };
        let checked = validate_placement(raw, &plan(), &files(&["src/a.rs"])).unwrap();
        assert_eq!(checked.placement.regions.len(), MAX_REGIONS);
        assert_eq!(
            checked.notes,
            ["5 regions dropped: more than the 300 a placement holds"]
        );
        let none =
            validate_placement(KeywordPlacement { regions: vec![] }, &plan(), &files(&[])).unwrap();
        assert!(none.placement.regions.is_empty() && none.notes.is_empty());
    }
}
