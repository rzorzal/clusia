//! Finds the recipe and the near-tests list in the answer of a recipe turn.

use clusia_core::printable::is_invisible;
use serde_json::Value;

use crate::fenced::fenced_blocks;

pub const RECIPE_INFO: &str = "clusia-recipe";
pub const NEAR_TESTS_INFO: &str = "clusia-near-tests";
/// The most near-test names an answer keeps.
const NEAR_NAMES_MAX: usize = 200;
/// The longest near-test name, in characters.
const NEAR_NAME_MAX: usize = 300;

/// What a recipe turn's answer holds.
#[derive(Debug, Clone, PartialEq)]
pub struct RecipeBlocks {
    /// The answer without its blocks.
    pub text: String,
    /// The first `clusia-recipe` block whose body is a JSON object.
    pub recipe: Option<Value>,
    /// The names of the `clusia-near-tests` blocks, trimmed and unique, in order.
    pub near_tests: Vec<String>,
    /// The blocks that could not be used, or could be used only in part.
    pub unreadable: u32,
}

/// The recipe and the near tests of `text`.
///
/// A block is a line of exactly ```` ```clusia-recipe ```` or ```` ```clusia-near-tests ````,
/// up to the next line of exactly ```` ``` ````; one that never closes stays in the text and is
/// not counted. Only the first recipe that is a JSON object is kept; every other recipe block
/// counts as unreadable. A near-tests block is `{"tests": [..]}`; a name that is not a string,
/// is blank, is over 300 characters, holds a control or invisible character or comes past the
/// 200th is dropped, and a block that dropped one counts once as unreadable.
pub fn extract_recipe_blocks(text: &str) -> RecipeBlocks {
    let (kept, blocks) = fenced_blocks(text, &[RECIPE_INFO, NEAR_TESTS_INFO]);
    let mut recipe = None;
    let mut near_tests = Vec::new();
    let mut unreadable = 0u32;
    for (info, body) in blocks {
        if info == RECIPE_INFO {
            match serde_json::from_str::<Value>(&body) {
                Ok(value) if value.is_object() && recipe.is_none() => recipe = Some(value),
                _ => unreadable += 1,
            }
        } else {
            read_near(&body, &mut near_tests, &mut unreadable);
        }
    }
    RecipeBlocks {
        text: kept,
        recipe,
        near_tests,
        unreadable,
    }
}

fn read_near(body: &str, names: &mut Vec<String>, unreadable: &mut u32) {
    let items = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| match value {
            Value::Object(mut object) => object.remove("tests"),
            _ => None,
        });
    let Some(Value::Array(items)) = items else {
        *unreadable += 1;
        return;
    };
    let mut dropped = false;
    for item in items {
        let Some(name) = item.as_str().map(str::trim) else {
            dropped = true;
            continue;
        };
        let bad = name.is_empty()
            || name.chars().count() > NEAR_NAME_MAX
            || name.chars().any(|c| c.is_control() || is_invisible(c));
        if bad {
            dropped = true;
        } else if names.iter().any(|n| n == name) {
            continue;
        } else if names.len() >= NEAR_NAMES_MAX {
            dropped = true;
        } else {
            names.push(name.to_string());
        }
    }
    if dropped {
        *unreadable += 1;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::{near_tests_block, recipe_block};

    fn recipe() -> serde_json::Value {
        json!({"runner": "auto", "tests": {"all": "cargo test --workspace", "results": "libtest"}})
    }

    #[test]
    fn the_recipe_and_the_near_tests_are_read_and_the_prose_is_left() {
        let answer = format!(
            "Here is the recipe.\n\n{}\n\nDone.\n\n{}",
            recipe_block(recipe()),
            near_tests_block(json!({"tests": ["a::t", "b::t"]}))
        );
        let blocks = extract_recipe_blocks(&answer);
        assert_eq!(blocks.text, "Here is the recipe.\n\nDone.");
        assert_eq!(blocks.recipe, Some(recipe()));
        assert_eq!(blocks.near_tests, ["a::t", "b::t"]);
        assert_eq!(blocks.unreadable, 0);
    }

    #[test]
    fn the_first_recipe_object_wins_and_the_others_count_as_unreadable() {
        let other = json!({"runner": "local", "tests": {"all": "make test", "results": "plain"}});
        let two = format!(
            "{}\n{}",
            recipe_block(recipe()),
            recipe_block(other.clone())
        );
        let blocks = extract_recipe_blocks(&two);
        assert_eq!(blocks.recipe, Some(recipe()));
        assert_eq!(blocks.unreadable, 1);

        let broken_first = format!(
            "```clusia-recipe\nnot json\n```\n{}",
            recipe_block(other.clone())
        );
        let blocks = extract_recipe_blocks(&broken_first);
        assert_eq!(blocks.recipe, Some(other));
        assert_eq!(blocks.unreadable, 1);
    }

    #[test]
    fn a_recipe_that_is_not_an_object_is_unreadable() {
        let answer = "```clusia-recipe\n[1, 2]\n```\n```clusia-recipe\n\"x\"\n```";
        let blocks = extract_recipe_blocks(answer);
        assert_eq!((blocks.recipe, blocks.unreadable), (None, 2));
    }

    #[test]
    fn near_names_are_trimmed_and_unique_across_blocks() {
        let answer = format!(
            "{}\n{}",
            near_tests_block(json!({"tests": [" a ", "a", "b"]})),
            near_tests_block(json!({"tests": ["b", "c"]}))
        );
        let blocks = extract_recipe_blocks(&answer);
        assert_eq!(blocks.near_tests, ["a", "b", "c"]);
        assert_eq!(blocks.unreadable, 0, "a repeated name is not an error");
    }

    #[test]
    fn a_bad_name_is_dropped_and_its_block_counts_once() {
        let long = "n".repeat(301);
        let exact = "n".repeat(300);
        let answer = near_tests_block(json!({
            "tests": ["ok", "bad\nname", "bad\u{202e}name", long, exact, "", 3, "also ok"]
        }));
        let blocks = extract_recipe_blocks(&answer);
        assert_eq!(
            blocks.near_tests,
            vec!["ok".to_string(), exact, "also ok".to_string()]
        );
        assert_eq!(blocks.unreadable, 1);
    }

    #[test]
    fn at_most_two_hundred_names_are_kept() {
        let names: Vec<String> = (0..250).map(|n| format!("t{n}")).collect();
        let blocks = extract_recipe_blocks(&near_tests_block(json!({ "tests": names })));
        assert_eq!(blocks.near_tests.len(), 200);
        assert_eq!(blocks.near_tests[199], "t199");
        assert_eq!(blocks.unreadable, 1);
    }

    #[test]
    fn a_near_block_of_the_wrong_shape_is_unreadable() {
        for body in ["{\"tests\": \"a\"}", "[\"a\"]", "{\"other\": []}", "nope"] {
            let blocks = extract_recipe_blocks(&format!("```clusia-near-tests\n{body}\n```"));
            assert_eq!(
                (blocks.near_tests.len(), blocks.unreadable),
                (0, 1),
                "{body}"
            );
        }
    }

    #[test]
    fn a_block_that_never_closes_stays_in_the_text_and_is_not_counted() {
        let blocks = extract_recipe_blocks("Before\n```clusia-recipe\n{\"runner\": \"auto\"}");
        assert_eq!(blocks.recipe, None);
        assert_eq!(blocks.unreadable, 0);
        assert_eq!(
            blocks.text,
            "Before\n```clusia-recipe\n{\"runner\": \"auto\"}"
        );
    }

    #[test]
    fn other_info_strings_and_plain_answers_are_left_alone() {
        let answer = "```clusia-recipe-draft\n{}\n```\n```toml\nx = 1\n```";
        let blocks = extract_recipe_blocks(answer);
        assert_eq!(blocks.text, answer);
        assert_eq!((blocks.recipe, blocks.unreadable), (None, 0));
        let blocks = extract_recipe_blocks("No recipe is possible here.\n");
        assert_eq!(blocks.text, "No recipe is possible here.");
    }

    #[test]
    fn a_fence_inside_a_json_string_does_not_end_the_block() {
        let value = json!({"runner": "auto", "tests": {"all": "echo ```", "results": "plain"}});
        let blocks = extract_recipe_blocks(&recipe_block(value.clone()));
        assert_eq!(blocks.recipe, Some(value));
    }
}
