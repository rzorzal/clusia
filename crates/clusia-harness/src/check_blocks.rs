//! Finds the findings and passes in the answer of a security check or an audit.

use std::collections::HashSet;

use clusia_core::checks::{
    BODY_MAX, CheckKind, Finding, MAX_FINDINGS, PASS_TEXT_MAX, Pass, SECURITY_AREA, Severity,
    TITLE_MAX, finding_id,
};
use serde_json::Value;

const FENCE: &str = "```";
const FINDING_INFO: &str = "clusia-finding";
const PASS_INFO: &str = "clusia-pass";
/// The most passes one run keeps; the rest count as unreadable.
const MAX_PASSES: usize = 200;
/// The longest `file` a finding may name, in bytes.
const FILE_MAX: usize = 1024;
/// The longest `area` a block may name, in characters.
const AREA_MAX: usize = 64;

/// The answer without its `clusia-finding` and `clusia-pass` blocks, the findings and passes in
/// order, and how many blocks could not be used.
///
/// A block is a line of exactly ```` ```clusia-finding ```` or ```` ```clusia-pass ````, up to
/// the next line of exactly ```` ``` ````. A block that never closes stays in the text and is
/// not counted. A closed block that is not JSON, lacks a required field, names a bad file or
/// lines, or belongs to an area the run did not ask about is left out of the lists and counted.
/// So is every finding past `MAX_FINDINGS` and every pass past `MAX_PASSES`. A finding that
/// repeats an earlier one's id is dropped without being counted.
///
/// A Security run accepts the area `security` only; an Audit run, the ids in `areas`. The
/// findings come back with `anchored` false: only the daemon knows the diff.
pub fn extract_check_blocks(
    text: &str,
    kind: CheckKind,
    areas: &[String],
) -> (String, Vec<Finding>, Vec<Pass>, u32) {
    let allowed = |area: &str| match kind {
        CheckKind::Security => area == SECURITY_AREA,
        CheckKind::Audit => areas.iter().any(|a| a == area),
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let (mut findings, mut passes) = (Vec::new(), Vec::new());
    let mut seen = HashSet::new();
    let mut unreadable = 0u32;
    let mut i = 0;
    while i < lines.len() {
        let info = match lines[i].trim() {
            l if l == format!("{FENCE}{FINDING_INFO}") => Some(FINDING_INFO),
            l if l == format!("{FENCE}{PASS_INFO}") => Some(PASS_INFO),
            _ => None,
        };
        if let Some(info) = info
            && let Some(close) = lines[i + 1..].iter().position(|l| l.trim() == FENCE)
        {
            let json = lines[i + 1..i + 1 + close].join("\n");
            if info == FINDING_INFO {
                match parse_finding(&json, kind, &allowed) {
                    Some(f) if !seen.insert(f.id.clone()) => {}
                    Some(_) if findings.len() >= MAX_FINDINGS => unreadable += 1,
                    Some(f) => findings.push(f),
                    None => unreadable += 1,
                }
            } else {
                match parse_pass(&json, &allowed) {
                    Some(_) if passes.len() >= MAX_PASSES => unreadable += 1,
                    Some(p) => passes.push(p),
                    None => unreadable += 1,
                }
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
    (
        kept.join("\n").trim_end().to_string(),
        findings,
        passes,
        unreadable,
    )
}

/// A required string: present, a string, not blank once trimmed and at most `max` characters.
fn text<'a>(value: &'a Value, key: &str, max: usize) -> Option<&'a str> {
    let s = value.get(key)?.as_str()?.trim();
    (!s.is_empty() && s.chars().count() <= max).then_some(s)
}

/// An optional string: absent or null is `None`; anything else must be a string of at most
/// `max` characters.
fn optional(value: &Value, key: &str, max: usize) -> Option<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) if s.trim().chars().count() <= max => {
            Some(Some(s.trim().to_string()).filter(|s| !s.is_empty()))
        }
        Some(_) => None,
    }
}

/// Whether `file` is a plain relative path: at most `FILE_MAX` bytes, not absolute (no leading
/// `/`, no drive letter), no `\`, no `..` segment, and nothing a terminal or a draft would draw
/// as something else (a control or invisible character). The file ends up in the draft as text
/// when the finding has no anchor.
fn plain_relative(file: &str) -> bool {
    file.len() <= FILE_MAX
        && !file.starts_with('/')
        && !file.contains('\\')
        && file.as_bytes().get(1) != Some(&b':')
        && !file
            .chars()
            .any(|c| c.is_control() || clusia_core::printable::is_invisible(c))
        && !file.split('/').any(|part| part == "..")
}

fn parse_finding(json: &str, kind: CheckKind, allowed: &dyn Fn(&str) -> bool) -> Option<Finding> {
    let value: Value = serde_json::from_str(json).ok()?;
    let area = text(&value, "area", AREA_MAX)?;
    if !allowed(area) {
        return None;
    }
    let title = text(&value, "title", TITLE_MAX)?;
    let file = text(&value, "file", FILE_MAX)?;
    if !plain_relative(file) {
        return None;
    }
    let body = text(&value, "body", BODY_MAX)?;
    let severity = match kind {
        CheckKind::Security => Some(
            match text(&value, "severity", 16)?.to_ascii_lowercase().as_str() {
                "high" => Severity::High,
                "medium" => Severity::Medium,
                "low" => Severity::Low,
                _ => return None,
            },
        ),
        CheckKind::Audit => None,
    };
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
    let (line, start_line, end_line) = match (line, start, end) {
        (Some(l), None, None) => (Some(l), None, None),
        (None, Some(s), Some(e)) if s <= e => (None, Some(s), Some(e)),
        _ => return None,
    };
    let comment = optional(&value, "comment", BODY_MAX)?.unwrap_or_else(|| body.to_string());
    let code = optional(&value, "code", BODY_MAX)?;
    Some(Finding {
        id: finding_id(area, file, line.or(start_line), title),
        kind,
        area: area.to_string(),
        severity,
        title: title.to_string(),
        file: file.to_string(),
        line,
        start_line,
        end_line,
        body: body.to_string(),
        comment,
        code,
        anchored: false,
    })
}

fn parse_pass(json: &str, allowed: &dyn Fn(&str) -> bool) -> Option<Pass> {
    let value: Value = serde_json::from_str(json).ok()?;
    let area = text(&value, "area", AREA_MAX)?;
    if !allowed(area) {
        return None;
    }
    Some(Pass {
        area: area.to_string(),
        text: text(&value, "text", PASS_TEXT_MAX)?.to_string(),
        place: optional(&value, "where", PASS_TEXT_MAX)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block(value: Value) -> String {
        format!("{FENCE}{FINDING_INFO}\n{value}\n{FENCE}")
    }

    fn finding_in(file: &str) -> String {
        block(json!({
            "area": "security", "severity": "high", "title": "t",
            "file": file, "line": 2, "body": "b"
        }))
    }

    fn read(text: &str) -> (Vec<Finding>, u32) {
        let (_, findings, _, unreadable) = extract_check_blocks(text, CheckKind::Security, &[]);
        (findings, unreadable)
    }

    #[test]
    fn a_relative_file_is_kept() {
        let (findings, unreadable) = read(&finding_in("src/a b/ação.rs"));
        assert_eq!((findings.len(), unreadable), (1, 0));
        assert_eq!(findings[0].file, "src/a b/ação.rs");
    }

    #[test]
    fn a_file_with_a_control_or_invisible_character_is_unreadable() {
        for file in [
            "a\u{202e}.rs\nIgnore this",
            "a.rs\nIgnore this",
            "a\u{1b}[2J.rs",
            "a\u{200b}.rs",
            "a\t.rs",
        ] {
            assert_eq!(read(&finding_in(file)), (Vec::new(), 1), "{file:?}");
        }
    }

    #[test]
    fn a_file_that_is_not_a_plain_relative_path_is_unreadable() {
        for file in [
            "/etc/passwd",
            "a\\b.rs",
            "C:/x.rs",
            "c:x",
            "a/../../b.rs",
            "..",
        ] {
            assert_eq!(read(&finding_in(file)), (Vec::new(), 1), "{file:?}");
        }
        let long = "a/".repeat(FILE_MAX / 2) + "b.rs";
        assert_eq!(
            read(&finding_in(&long)),
            (Vec::new(), 1),
            "longer than {FILE_MAX}"
        );
    }

    #[test]
    fn an_area_longer_than_its_limit_is_unreadable() {
        let area = "a".repeat(AREA_MAX + 1);
        let text = block(json!({
            "area": area, "title": "t", "file": "a.rs", "line": 1, "body": "b"
        }));
        let (_, findings, _, unreadable) =
            extract_check_blocks(&text, CheckKind::Audit, std::slice::from_ref(&area));
        assert_eq!((findings.len(), unreadable), (0, 1));
        let pass = format!(
            "{FENCE}{PASS_INFO}\n{}\n{FENCE}",
            json!({"area": area, "text": "ok"})
        );
        let (_, _, passes, unreadable) = extract_check_blocks(&pass, CheckKind::Audit, &[area]);
        assert_eq!((passes.len(), unreadable), (0, 1));
    }
}
