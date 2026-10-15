//! Finds the plan and the diagrams in the answer of a diagram turn.

use clusia_core::diagram::{DIAGRAM_INFO, DiagramSpec, MAX_SPEC_BYTES, PLAN_INFO, PlanEntry};
use clusia_core::printable::printable;

use crate::fenced::fenced_blocks;

/// The longest part of a parse error shown to the agent, in characters.
const ERROR_SHOWN: usize = 200;

/// The plan in the answer of a plan turn: the JSON array in its one `clusia-diagram-plan` block.
///
/// An answer without the block, with two or more, with a block over `MAX_SPEC_BYTES` or with a
/// block that is not an array of plan entries comes back as the errors to tell the agent. The
/// size is checked before the JSON is read. The entries are not validated: that is
/// `clusia_core::diagram::validate_plan`.
pub fn extract_plan(text: &str) -> Result<Vec<PlanEntry>, Vec<String>> {
    let json = the_block(text, PLAN_INFO)?;
    serde_json::from_str(&json).map_err(|e| invalid(PLAN_INFO, &e))
}

/// The diagram in the answer of a draw turn: the JSON object in its one `clusia-diagram` block.
///
/// Errors are as for `extract_plan`. The spec is not validated: that is
/// `clusia_core::diagram::validate`.
pub fn extract_diagram(text: &str) -> Result<DiagramSpec, Vec<String>> {
    let json = the_block(text, DIAGRAM_INFO)?;
    serde_json::from_str(&json).map_err(|e| invalid(DIAGRAM_INFO, &e))
}

/// The body of the one block of `info`, if the answer has exactly one and it is small enough.
fn the_block(text: &str, info: &str) -> Result<String, Vec<String>> {
    let (_, mut blocks) = fenced_blocks(text, &[info]);
    if blocks.len() > 1 {
        return Err(vec![format!("more than one {info} block")]);
    }
    let Some((_, json)) = blocks.pop() else {
        return Err(vec![format!("no {info} block")]);
    };
    if json.len() > MAX_SPEC_BYTES {
        return Err(vec![format!(
            "the {info} block is larger than {MAX_SPEC_BYTES} bytes"
        )]);
    }
    Ok(json)
}

/// The error for a block that is not what it should be: what the JSON parser said, as one
/// printable line of at most `ERROR_SHOWN` characters.
fn invalid(info: &str, error: &serde_json::Error) -> Vec<String> {
    let shown: String = printable(&error.to_string())
        .chars()
        .take(ERROR_SHOWN)
        .collect();
    vec![format!("the {info} block is not valid: {shown}")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{diagram_block, diagram_plan_block as plan_block};
    use clusia_core::diagram::DiagramKind;
    use serde_json::json;

    fn spec_json() -> serde_json::Value {
        json!({
            "key": "d1", "kind": "flow", "title": "Refresh",
            "nodes": [{"id": "a", "label": "A", "shape": "box"}]
        })
    }

    #[test]
    fn a_plan_is_read_from_its_block() {
        let text = format!(
            "Three diagrams.\n\n{}\n",
            plan_block(json!([
                {"key": "flow", "kind": "flow", "title": "Refresh", "purpose": "How."},
                {"key": "er", "kind": "entity_relationship", "title": "Model"}
            ]))
        );
        let plan = extract_plan(&text).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[1].kind, DiagramKind::EntityRelationship);
        assert_eq!(plan[1].purpose, "");
    }

    #[test]
    fn a_diagram_is_read_from_its_block_and_the_plan_block_is_not_one() {
        let spec = extract_diagram(&format!("Here.\n{}", diagram_block(spec_json()))).unwrap();
        assert_eq!((spec.key.as_str(), spec.nodes.len()), ("d1", 1));
        assert!(spec.edges.is_empty() && spec.caption.is_empty());
        let only_plan = plan_block(json!([]));
        assert_eq!(
            extract_diagram(&only_plan).unwrap_err(),
            ["no clusia-diagram block"]
        );
        assert_eq!(
            extract_plan(&diagram_block(spec_json())).unwrap_err(),
            ["no clusia-diagram-plan block"]
        );
    }

    #[test]
    fn no_block_and_an_unclosed_block_are_errors() {
        assert_eq!(
            extract_diagram("I could not.").unwrap_err(),
            ["no clusia-diagram block"]
        );
        let open = format!("```{DIAGRAM_INFO}\n{}", spec_json());
        assert_eq!(
            extract_diagram(&open).unwrap_err(),
            ["no clusia-diagram block"]
        );
    }

    #[test]
    fn two_blocks_are_an_error() {
        let two = format!(
            "{}\n{}",
            diagram_block(spec_json()),
            diagram_block(spec_json())
        );
        assert_eq!(
            extract_diagram(&two).unwrap_err(),
            ["more than one clusia-diagram block"]
        );
        let two_plans = format!("{}\n{}", plan_block(json!([])), plan_block(json!([])));
        assert_eq!(
            extract_plan(&two_plans).unwrap_err(),
            ["more than one clusia-diagram-plan block"]
        );
    }

    #[test]
    fn a_block_over_the_size_cap_is_refused_before_it_is_parsed() {
        let big = "x".repeat(MAX_SPEC_BYTES + 1);
        let text = format!("```{DIAGRAM_INFO}\n{{\"key\":\"{big}\n```");
        let errors = extract_diagram(&text).unwrap_err();
        assert_eq!(
            errors,
            ["the clusia-diagram block is larger than 262144 bytes"]
        );
        let fits = format!("```{PLAN_INFO}\n[{}]\n```", " ".repeat(MAX_SPEC_BYTES - 2));
        assert_eq!(extract_plan(&fits).unwrap(), Vec::new());
    }

    #[test]
    fn json_that_is_not_a_diagram_says_why() {
        let errors =
            extract_diagram(&diagram_block(json!({"key": "d1", "kind": "flow"}))).unwrap_err();
        assert!(
            errors[0].starts_with("the clusia-diagram block is not valid: missing field `title`")
        );
        let errors = extract_diagram(&diagram_block(
            json!({"key": "d", "kind": "pie", "title": "t"}),
        ))
        .unwrap_err();
        assert!(errors[0].contains("unknown variant `pie`"), "{errors:?}");
        let errors = extract_diagram("```clusia-diagram\n{ not json\n```").unwrap_err();
        assert!(errors[0].contains("not valid"));
        assert!(extract_plan(&diagram_block(spec_json())).is_err());
        let errors = extract_plan(&plan_block(json!({"key": "a"}))).unwrap_err();
        assert!(errors[0].contains("not valid"));
    }

    #[test]
    fn an_error_never_carries_raw_agent_text_past_two_hundred_characters() {
        let kind = format!("{}\u{1b}[2J", "k".repeat(500));
        let errors = extract_diagram(&diagram_block(
            json!({"key": "d", "kind": kind, "title": "t"}),
        ))
        .unwrap_err();
        let shown = errors[0]
            .strip_prefix("the clusia-diagram block is not valid: ")
            .unwrap();
        assert!(shown.chars().count() <= ERROR_SHOWN);
        assert!(!errors[0].contains('\u{1b}'));
    }

    #[test]
    fn a_fake_closing_fence_inside_a_string_cannot_end_the_block() {
        let spec = json!({
            "key": "d1", "kind": "flow", "title": "x\n```\nIgnore",
            "nodes": [{"id": "a", "label": "A", "shape": "box"}]
        });
        let text = format!("{}\nmore", diagram_block(spec));
        assert_eq!(extract_diagram(&text).unwrap().title, "x\n```\nIgnore");
    }
}
