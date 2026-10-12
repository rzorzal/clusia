//! Finds the verdicts in the answer of a revalidation run.

use clusia_core::printable::printable_lines;
use clusia_core::revalidation::{MAX_BLOCKS, REASON_MAX, TEXT_MAX, Verdict};
use clusia_core::{Anchor, Side};
use serde_json::Value;

use crate::check_blocks::{FILE_MAX, optional, plain_relative, text};

const FENCE: &str = "```";
const INFO: &str = "clusia-revalidation";

/// What the agent said about one comment. A moved anchor has no `commit` yet: only the daemon
/// knows the new head, and it checks the anchor against the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawVerdict {
    pub item: String,
    pub verdict: Verdict,
}

/// The answer without its `clusia-revalidation` blocks, the verdicts, and how many blocks could
/// not be used.
///
/// A block is a line of exactly ```` ```clusia-revalidation ````, up to the next line of exactly
/// ```` ``` ````. A block that never closes stays in the text and is not counted. A closed block
/// that is not JSON, names an item that was not asked, lacks a required field, or names a path
/// that is absolute or climbs with `..` is counted and left out; so is every block past
/// `MAX_BLOCKS`. A later valid block for an item replaces an earlier one. The `text` and the
/// `reason` come back with the characters that draw nothing or reorder what is drawn shown as
/// escapes; the length caps apply to what the agent wrote, before those escapes, so a stored
/// `text` or `reason` can run past them.
pub fn extract_revalidation_blocks(text: &str, asked: &[String]) -> (String, Vec<RawVerdict>, u32) {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut verdicts: Vec<RawVerdict> = Vec::new();
    let (mut blocks, mut unreadable) = (0usize, 0u32);
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == format!("{FENCE}{INFO}")
            && let Some(close) = lines[i + 1..].iter().position(|l| l.trim() == FENCE)
        {
            let json = lines[i + 1..i + 1 + close].join("\n");
            blocks += 1;
            match parse(&json, asked) {
                Some(_) if blocks > MAX_BLOCKS => unreadable += 1,
                Some(v) => {
                    verdicts.retain(|old| old.item != v.item);
                    verdicts.push(v);
                }
                None => unreadable += 1,
            }
            i += close + 2;
            // The blank line that set the block apart goes with it.
            if lines.get(i).is_some_and(|l| l.trim().is_empty())
                && kept.last().is_none_or(|l| l.trim().is_empty())
            {
                i += 1;
            }
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }
    (kept.join("\n").trim_end().to_string(), verdicts, unreadable)
}

fn number(value: &Value, key: &str) -> Option<Option<u32>> {
    match value.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => {
            let n = u32::try_from(v.as_u64()?).ok()?;
            (n > 0).then_some(Some(n))
        }
    }
}

fn anchor(value: &Value) -> Option<Anchor> {
    let path = text(value, "path", FILE_MAX)?;
    if !plain_relative(path) {
        return None;
    }
    let side = match value.get("side") {
        None | Some(Value::Null) => Side::Right,
        Some(Value::String(s)) if s == "right" => Side::Right,
        Some(Value::String(s)) if s == "left" => Side::Left,
        Some(_) => return None,
    };
    let (line, start, end) = (
        number(value, "line")?,
        number(value, "start_line")?,
        number(value, "end_line")?,
    );
    let (line, start_line) = match (line, start, end) {
        (Some(l), None, None) => (l, None),
        (None, Some(s), Some(e)) if s <= e => (e, Some(s)),
        _ => return None,
    };
    Some(Anchor {
        path: path.to_string(),
        line,
        start_line,
        side,
        commit: String::new(),
    })
}

fn parse(json: &str, asked: &[String]) -> Option<RawVerdict> {
    let value: Value = serde_json::from_str(json).ok()?;
    let item = text(&value, "item", 64)?;
    if !asked.iter().any(|id| id == item) {
        return None;
    }
    let reason = || text(&value, "reason", REASON_MAX).map(printable_lines);
    let verdict = match text(&value, "verdict", 16)? {
        "kept" => Verdict::Kept,
        "moved" => Verdict::Moved {
            anchor: anchor(&value)?,
            reason: reason()?,
        },
        "obsolete" => Verdict::Obsolete { reason: reason()? },
        "reword" => Verdict::Reword {
            text: printable_lines(text(&value, "text", TEXT_MAX)?),
            reason: optional(&value, "reason", REASON_MAX)?.map(|r| printable_lines(&r)),
        },
        _ => return None,
    };
    Some(RawVerdict {
        item: item.to_string(),
        verdict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block(value: Value) -> String {
        format!("{FENCE}{INFO}\n{value}\n{FENCE}")
    }

    fn asked() -> Vec<String> {
        vec!["i1".into(), "i2".into(), "i3".into()]
    }

    fn read(text: &str) -> (Vec<RawVerdict>, u32) {
        let (_, verdicts, unreadable) = extract_revalidation_blocks(text, &asked());
        (verdicts, unreadable)
    }

    #[test]
    fn the_four_verdicts_are_read_and_the_blocks_leave_the_text() {
        let text = format!(
            "Here is what I found.\n\n{}\n\n{}\n\n{}\n\n{}\n\nThat is all.",
            block(json!({"item": "i1", "verdict": "kept"})),
            block(json!({
                "item": "i2", "verdict": "moved", "path": "src/cache.rs",
                "line": 44, "reason": "The function gained 4 lines above."
            })),
            block(
                json!({"item": "i3", "verdict": "obsolete", "reason": "The retry loop is gone."})
            ),
            block(
                json!({"item": "i1", "verdict": "reword", "text": "Why 60 seconds?", "reason": "The limit changed."})
            ),
        );
        let (rest, verdicts, unreadable) = extract_revalidation_blocks(&text, &asked());
        assert_eq!(rest, "Here is what I found.\n\nThat is all.");
        assert_eq!(unreadable, 0);
        assert_eq!(
            verdicts,
            [
                RawVerdict {
                    item: "i2".into(),
                    verdict: Verdict::Moved {
                        anchor: Anchor {
                            path: "src/cache.rs".into(),
                            line: 44,
                            start_line: None,
                            side: Side::Right,
                            commit: String::new(),
                        },
                        reason: "The function gained 4 lines above.".into(),
                    },
                },
                RawVerdict {
                    item: "i3".into(),
                    verdict: Verdict::Obsolete {
                        reason: "The retry loop is gone.".into()
                    },
                },
                RawVerdict {
                    item: "i1".into(),
                    verdict: Verdict::Reword {
                        text: "Why 60 seconds?".into(),
                        reason: Some("The limit changed.".into()),
                    },
                },
            ],
            "the later block for i1 replaced the earlier one"
        );
    }

    #[test]
    fn a_range_and_the_left_side_are_read() {
        let (verdicts, unreadable) = read(&block(json!({
            "item": "i1", "verdict": "moved", "path": "a.rs",
            "start_line": 3, "end_line": 5, "side": "left", "reason": "r"
        })));
        assert_eq!(unreadable, 0);
        let Verdict::Moved { anchor, .. } = &verdicts[0].verdict else {
            panic!("moved")
        };
        assert_eq!(
            (anchor.line, anchor.start_line, anchor.side),
            (5, Some(3), Side::Left)
        );
    }

    #[test]
    fn a_block_that_cannot_be_used_is_counted_and_dropped() {
        let bad = [
            "{not json".to_string(),
            block_body(json!({"item": "i9", "verdict": "kept"})),
            block_body(json!({"verdict": "kept"})),
            block_body(json!({"item": "i1", "verdict": "maybe"})),
            block_body(json!({"item": "i1", "verdict": "moved", "path": "a.rs", "line": 1})),
            block_body(json!({"item": "i1", "verdict": "moved", "reason": "r", "line": 1})),
            block_body(json!({"item": "i1", "verdict": "moved", "path": "a.rs", "reason": "r"})),
            block_body(
                json!({"item": "i1", "verdict": "moved", "path": "a.rs", "line": 0, "reason": "r"}),
            ),
            block_body(
                json!({"item": "i1", "verdict": "moved", "path": "a.rs", "start_line": 5, "end_line": 3, "reason": "r"}),
            ),
            block_body(
                json!({"item": "i1", "verdict": "moved", "path": "a.rs", "line": 1, "side": "up", "reason": "r"}),
            ),
            block_body(json!({"item": "i1", "verdict": "obsolete"})),
            block_body(json!({"item": "i1", "verdict": "reword"})),
            block_body(json!({"item": "i1", "verdict": "reword", "text": "  "})),
            block_body(
                json!({"item": "i1", "verdict": "obsolete", "reason": "x".repeat(REASON_MAX + 1)}),
            ),
            block_body(
                json!({"item": "i1", "verdict": "reword", "text": "x".repeat(TEXT_MAX + 1)}),
            ),
        ];
        for body in bad {
            let text = format!("{FENCE}{INFO}\n{body}\n{FENCE}");
            assert_eq!(read(&text), (Vec::new(), 1), "{body}");
        }
    }

    fn block_body(value: Value) -> String {
        value.to_string()
    }

    #[test]
    fn a_path_that_is_not_a_plain_relative_one_is_unreadable() {
        for path in [
            "/etc/passwd",
            "a/../../b.rs",
            "..",
            "C:/x.rs",
            "a\\b.rs",
            "a\u{202e}.rs",
        ] {
            let text = block(json!({
                "item": "i1", "verdict": "moved", "path": path, "line": 1, "reason": "r"
            }));
            assert_eq!(read(&text), (Vec::new(), 1), "{path:?}");
        }
    }

    #[test]
    fn reword_text_is_printable() {
        let text = block(json!({
            "item": "i1", "verdict": "reword",
            "text": "Line one\n\tindented \u{1b}[2K gone \u{202e}reversed",
            "reason": "Why\u{200b}now"
        }));
        let (verdicts, unreadable) = read(&text);
        assert_eq!(unreadable, 0);
        assert_eq!(
            verdicts[0].verdict,
            Verdict::Reword {
                text: "Line one\n\tindented \\u{1b}[2K gone \\u{202e}reversed".into(),
                reason: Some("Why\\u{200b}now".into()),
            }
        );
        let moved = block(json!({
            "item": "i1", "verdict": "obsolete", "reason": "gone\u{1b}[31m"
        }));
        let (verdicts, _) = read(&moved);
        assert_eq!(
            verdicts[0].verdict,
            Verdict::Obsolete {
                reason: "gone\\u{1b}[31m".into()
            }
        );
    }

    #[test]
    fn a_block_that_never_closes_stays_in_the_text() {
        let text = format!("Before\n{FENCE}{INFO}\n{{\"item\":\"i1\",\"verdict\":\"kept\"}}");
        let (rest, verdicts, unreadable) = extract_revalidation_blocks(&text, &asked());
        assert_eq!((verdicts.len(), unreadable), (0, 0));
        assert_eq!(rest, text);
    }

    #[test]
    fn other_fences_are_left_alone() {
        let text = "```rust\nlet x = 1;\n```\n```clusia-revalidation extra\n{}\n```";
        let (rest, verdicts, unreadable) = extract_revalidation_blocks(text, &asked());
        assert_eq!((verdicts.len(), unreadable), (0, 0));
        assert_eq!(rest, text);
    }

    #[test]
    fn blocks_past_the_cap_are_unreadable() {
        let ids: Vec<String> = (0..MAX_BLOCKS + 3).map(|n| format!("i{n}")).collect();
        let text: String = ids
            .iter()
            .map(|id| block(json!({"item": id, "verdict": "kept"})) + "\n")
            .collect();
        let (_, verdicts, unreadable) = extract_revalidation_blocks(&text, &ids);
        assert_eq!((verdicts.len(), unreadable), (MAX_BLOCKS, 3));
    }
}
