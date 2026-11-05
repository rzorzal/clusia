//! Finds the scenario, benchmark and suggestion blocks in the answer of a QA cockpit turn.

use clusia_core::cockpit::SUGGESTIONS_MAX;
use serde_json::Value;

pub use clusia_core::cockpit::{BENCH_INFO, SCENARIO_INFO, SUGGESTION_INFO};

use crate::fenced::fenced_blocks;

/// The most scenario blocks an answer keeps.
pub const SCENARIO_BLOCKS_MAX: usize = 12;
/// The most benchmark blocks an answer keeps.
pub const BENCH_BLOCKS_MAX: usize = 1;

/// What the answer of a scenario, benchmark or copilot turn holds.
#[derive(Debug, Clone, PartialEq)]
pub struct CockpitBlocks {
    /// The answer without its blocks.
    pub text: String,
    pub scenarios: Vec<Value>,
    pub benches: Vec<Value>,
    pub suggestions: Vec<Value>,
    /// Blocks that could not be used: not a JSON object, or past the cap.
    pub unreadable: u32,
}

/// The blocks of `text`, per kind in order. A body that is a JSON object is kept while its
/// kind's list is under its cap; every other body counts once as unreadable. A block that never
/// closes stays in the text.
pub fn extract_cockpit_blocks(text: &str) -> CockpitBlocks {
    let (text, blocks) = fenced_blocks(text, &[SCENARIO_INFO, BENCH_INFO, SUGGESTION_INFO]);
    let mut out = CockpitBlocks {
        text,
        scenarios: Vec::new(),
        benches: Vec::new(),
        suggestions: Vec::new(),
        unreadable: 0,
    };
    for (info, body) in blocks {
        let (list, cap) = match info {
            SCENARIO_INFO => (&mut out.scenarios, SCENARIO_BLOCKS_MAX),
            BENCH_INFO => (&mut out.benches, BENCH_BLOCKS_MAX),
            _ => (&mut out.suggestions, SUGGESTIONS_MAX),
        };
        match serde_json::from_str::<Value>(&body) {
            Ok(value) if value.is_object() && list.len() < cap => list.push(value),
            _ => out.unreadable += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::{bench_block, scenario_block, suggestion_block};

    fn scenario(title: &str) -> serde_json::Value {
        json!({"title": title, "run": "sh {script}"})
    }

    #[test]
    fn the_three_kinds_are_kept_in_order_and_bad_ones_counted() {
        let answer = format!(
            "Here is what I found.\n{}\n{}\n```clusia-scenario\n[1, 2]\n```\n```clusia-bench\nnot json\n```\n{}\n{}\nDone.",
            scenario_block(scenario("a")),
            scenario_block(scenario("b")),
            bench_block(json!({"function": "f"})),
            suggestion_block(json!({"kind": "scenario", "title": "t", "why": "w"})),
        );
        let blocks = extract_cockpit_blocks(&answer);
        assert_eq!(blocks.scenarios, vec![scenario("a"), scenario("b")]);
        assert_eq!(blocks.benches, vec![json!({"function": "f"})]);
        assert_eq!(
            blocks.suggestions,
            vec![json!({"kind": "scenario", "title": "t", "why": "w"})]
        );
        assert_eq!(blocks.unreadable, 2);
        assert!(!blocks.text.contains("clusia-"));
        assert!(blocks.text.contains("Here is what I found.") && blocks.text.contains("Done."));
        assert_eq!(
            (SCENARIO_INFO, BENCH_INFO, SUGGESTION_INFO),
            ("clusia-scenario", "clusia-bench", "clusia-suggestion")
        );
    }

    #[test]
    fn the_caps_drop_the_rest_as_unreadable() {
        let many: String = (0..14)
            .map(|i| scenario_block(scenario(&format!("s{i}"))))
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = extract_cockpit_blocks(&many);
        assert_eq!(blocks.scenarios.len(), SCENARIO_BLOCKS_MAX);
        assert_eq!(blocks.scenarios[11], scenario("s11"));
        assert_eq!(blocks.unreadable, 2);
        let benches = format!(
            "{}\n{}",
            bench_block(json!({"function": "a"})),
            bench_block(json!({"function": "b"}))
        );
        let blocks = extract_cockpit_blocks(&benches);
        assert_eq!(
            blocks.benches,
            vec![json!({"function": "a"})],
            "one benchmark per answer"
        );
        assert_eq!(blocks.unreadable, 1);
        let suggestions: String = (0..5)
            .map(|i| {
                suggestion_block(
                    json!({"kind": "bench", "title": format!("t{i}"), "why": "w", "function": "f"}),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = extract_cockpit_blocks(&suggestions);
        assert_eq!(blocks.suggestions.len(), 3);
        assert_eq!(blocks.unreadable, 2);
    }

    #[test]
    fn a_block_that_never_closes_stays_in_the_text() {
        let blocks = extract_cockpit_blocks("```clusia-scenario\n{\"title\": \"x\"}\n");
        assert!(blocks.scenarios.is_empty());
        assert_eq!(blocks.unreadable, 0);
        assert!(blocks.text.contains("clusia-scenario"));
        let none = extract_cockpit_blocks("Plain text only.");
        assert_eq!(none.text, "Plain text only.");
        assert_eq!(none.unreadable, 0);
    }
}
