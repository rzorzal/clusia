//! Finds the suggested comments in an agent's answer.

use clusia_protocol::Suggestion;
use serde_json::Value;
use sha2::{Digest, Sha256};

const FENCE: &str = "```";
const INFO: &str = "clusia-suggestion";

/// The answer without its valid suggestion blocks, and those suggestions in order. A block
/// that is not closed, is not JSON or lacks a file, a body or valid lines stays in the text.
pub fn extract_suggestions(text: &str) -> (String, Vec<Suggestion>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == format!("{FENCE}{INFO}")
            && let Some(close) = lines[i + 1..].iter().position(|l| l.trim() == FENCE)
        {
            let body = lines[i + 1..i + 1 + close].join("\n");
            if let Some(suggestion) = parse_block(&body) {
                found.push(suggestion);
                i += close + 2;
                // The blank line that set the block apart goes with it.
                if lines.get(i).is_some_and(|l| l.trim().is_empty())
                    && kept.last().is_none_or(|l| l.trim().is_empty())
                {
                    i += 1;
                }
                continue;
            }
        }
        kept.push(lines[i]);
        i += 1;
    }
    (kept.join("\n").trim_end().to_string(), found)
}

fn parse_block(json: &str) -> Option<Suggestion> {
    let value: Value = serde_json::from_str(json).ok()?;
    let file = value.get("file")?.as_str()?.trim();
    let body = value.get("body")?.as_str()?.trim();
    if file.is_empty() || file.starts_with('/') || file.split('/').any(|p| p == "..") {
        return None;
    }
    if body.is_empty() {
        return None;
    }
    let number = |key: &str| -> Option<Option<u32>> {
        match value.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(v) => {
                let n = u32::try_from(v.as_u64()?).ok()?;
                (n > 0).then_some(Some(n))
            }
        }
    };
    let (line, start, end) = (number("line")?, number("start_line")?, number("end_line")?);
    let (line, start_line, end_line, span) = match (line, start, end) {
        (Some(l), None, None) => (Some(l), None, None, l.to_string()),
        (None, Some(s), Some(e)) if s <= e => (None, Some(s), Some(e), format!("{s}-{e}")),
        _ => return None,
    };
    let digest = Sha256::digest(format!("{file}|{span}|{body}").as_bytes());
    let id = digest[..6]
        .iter()
        .fold(String::from("sug-"), |acc, b| acc + &format!("{b:02x}"));
    Some(Suggestion {
        id,
        file: file.to_string(),
        line,
        start_line,
        end_line,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: &str) -> String {
        format!("```clusia-suggestion\n{json}\n```")
    }

    #[test]
    fn a_line_suggestion_is_taken_out_of_the_text() {
        let text = format!(
            "Looks fine.\n\n{}\n\nThat is all.",
            block(
                r#"{"file":"src/auth/store.rs","line":44,"body":"Check the expiry before refreshing."}"#
            )
        );
        let (rest, found) = extract_suggestions(&text);
        assert_eq!(rest, "Looks fine.\n\nThat is all.");
        assert_eq!(
            found,
            [Suggestion {
                id: "sug-8fd7376aa622".into(),
                file: "src/auth/store.rs".into(),
                line: Some(44),
                start_line: None,
                end_line: None,
                body: "Check the expiry before refreshing.".into(),
            }]
        );
    }

    #[test]
    fn the_recorded_suggestions_fixture_gives_a_line_and_a_range() {
        let events = include_str!("../tests/fixtures/suggestions.jsonl");
        let last = events.lines().last().unwrap();
        let answer = serde_json::from_str::<Value>(last).unwrap()["result"]
            .as_str()
            .unwrap()
            .to_string();
        let (rest, found) = extract_suggestions(&answer);
        assert_eq!(
            rest,
            "Two things stand out.\n\nAnd a longer one:\n\nThat is all."
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "sug-8fd7376aa622");
        assert_eq!(found[1].id, "sug-5b4eb2a551a9");
        assert_eq!(
            (found[1].line, found[1].start_line, found[1].end_line),
            (None, Some(10), Some(18))
        );
        assert_eq!(
            found[1].body,
            "This loop retries forever.\n\nCap the attempts."
        );
    }

    #[test]
    fn ids_follow_the_content_not_the_position() {
        let a = block(r#"{"file":"a.rs","line":1,"body":"x"}"#);
        let b = block(r#"{"file":"a.rs","line":2,"body":"x"}"#);
        let id = |t: &str| extract_suggestions(t).1[0].id.clone();
        assert_eq!(id(&a), id(&format!("Intro\n\n{a}")));
        assert_ne!(id(&a), id(&b));
        assert!(id(&a).starts_with("sug-") && id(&a).len() == 16);
    }

    #[test]
    fn invalid_blocks_stay_as_text() {
        let broken = "```clusia-suggestion\n{\"file\": \"src/a.rs\", \"line\": 3,\n```";
        let no_body = block(r#"{"file":"src/a.rs","line":3}"#);
        let text = format!("First:\n\n{broken}\n\nThen:\n\n{no_body}\n");
        let (rest, found) = extract_suggestions(&text);
        assert!(found.is_empty());
        assert_eq!(rest, text.trim_end());
    }

    #[test]
    fn the_recorded_invalid_fixture_changes_nothing() {
        let events = include_str!("../tests/fixtures/invalid_suggestion.jsonl");
        let last = events.lines().last().unwrap();
        let answer = serde_json::from_str::<Value>(last).unwrap()["result"]
            .as_str()
            .unwrap()
            .to_string();
        let (rest, found) = extract_suggestions(&answer);
        assert!(found.is_empty());
        assert_eq!(rest, answer.trim_end());
    }

    #[test]
    fn bad_places_and_lines_are_refused() {
        for json in [
            r#"{"file":"/etc/passwd","line":1,"body":"x"}"#,
            r#"{"file":"../outside.rs","line":1,"body":"x"}"#,
            r#"{"file":"a/../../b.rs","line":1,"body":"x"}"#,
            r#"{"file":"","line":1,"body":"x"}"#,
            r#"{"file":"a.rs","line":0,"body":"x"}"#,
            r#"{"file":"a.rs","line":-1,"body":"x"}"#,
            r#"{"file":"a.rs","line":"3","body":"x"}"#,
            r#"{"file":"a.rs","start_line":5,"end_line":2,"body":"x"}"#,
            r#"{"file":"a.rs","start_line":5,"body":"x"}"#,
            r#"{"file":"a.rs","line":3,"start_line":1,"end_line":3,"body":"x"}"#,
            r#"{"file":"a.rs","line":3,"body":"   "}"#,
            r#"{"file":"a.rs","body":"no line at all"}"#,
            r#"["not","an","object"]"#,
        ] {
            let text = block(json);
            let (rest, found) = extract_suggestions(&text);
            assert!(found.is_empty(), "{json}");
            assert_eq!(rest, text, "{json}");
        }
    }

    #[test]
    fn a_block_that_never_closes_is_plain_text() {
        let text = "Here:\n```clusia-suggestion\n{\"file\":\"a.rs\",\"line\":1,\"body\":\"x\"}";
        let (rest, found) = extract_suggestions(text);
        assert!(found.is_empty());
        assert_eq!(rest, text);
    }

    #[test]
    fn other_code_fences_are_left_alone() {
        let text = "```rust\nfn main() {}\n```\n\n```clusia-suggestion-extra\nx\n```";
        let (rest, found) = extract_suggestions(text);
        assert!(found.is_empty());
        assert_eq!(rest, text);
    }

    #[test]
    fn text_that_is_only_a_block_becomes_empty() {
        let (rest, found) = extract_suggestions(&block(r#"{"file":"a.rs","line":1,"body":"x"}"#));
        assert_eq!(rest, "");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_valid_block_after_an_invalid_one_is_still_found() {
        let text = format!(
            "{}\n\n{}",
            block(r#"{"file":"a.rs"}"#),
            block(r#"{"file":"b.rs","line":2,"body":"y"}"#)
        );
        let (rest, found) = extract_suggestions(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, "b.rs");
        assert_eq!(rest, block(r#"{"file":"a.rs"}"#));
    }
}
