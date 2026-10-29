//! The `--json` report of jest and vitest.

use std::path::Path;

use clusia_core::tests::{TestCaseResult, TestStatus};
use serde_json::Value;

use super::{ResultParser, clean, keep};

pub(super) struct Jest;

fn is_report(value: &Value) -> bool {
    value.get("testResults").is_some_and(Value::is_array)
}

/// The report in `stdout`: all of it, its last line that parses, or the span from the first
/// `{` to the last `}`.
fn report(stdout: &str) -> Option<Value> {
    let text = stdout.trim();
    let parse = |t: &str| serde_json::from_str::<Value>(t).ok().filter(is_report);
    parse(text)
        .or_else(|| text.lines().rev().find_map(|l| parse(l.trim())))
        .or_else(|| {
            let (first, last) = (text.find('{')?, text.rfind('}')?);
            // A `}` before the first `{` would make the slice backwards.
            (first < last).then(|| parse(&text[first..=last]))?
        })
}

/// `name` relative to the worktree when it is inside it.
fn relative(name: &str, worktree: &Path) -> String {
    let path = Path::new(name);
    let roots = [Some(worktree.to_path_buf()), worktree.canonicalize().ok()];
    roots
        .iter()
        .flatten()
        .find_map(|root| path.strip_prefix(root).ok())
        .map_or_else(|| name.to_string(), |p| p.display().to_string())
}

fn status_of(word: &str) -> TestStatus {
    match word {
        "passed" => TestStatus::Passed,
        "failed" => TestStatus::Failed,
        _ => TestStatus::Skipped,
    }
}

impl ResultParser for Jest {
    fn line(&mut self, _line: &str) -> Vec<TestCaseResult> {
        Vec::new()
    }

    fn finish(&mut self, worktree: &Path, stdout: &str) -> Vec<TestCaseResult> {
        let Some(report) = report(stdout) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        for suite in report["testResults"].as_array().into_iter().flatten() {
            let file = relative(suite["name"].as_str().unwrap_or_default(), worktree);
            let assertions = suite["assertionResults"].as_array();
            if assertions.is_none_or(|a| a.is_empty()) {
                // A suite that never ran (a syntax error, a missing import) has no assertions.
                if suite["status"].as_str() == Some("failed") {
                    keep(
                        &mut results,
                        TestCaseResult {
                            name: file.clone(),
                            file: Some(file),
                            status: TestStatus::Failed,
                            duration_ms: None,
                            message: suite["message"].as_str().map(clean),
                        },
                    );
                }
                continue;
            }
            for test in assertions.into_iter().flatten() {
                let name = test["fullName"]
                    .as_str()
                    .or_else(|| test["title"].as_str())
                    .unwrap_or_default();
                let messages: Vec<&str> = test["failureMessages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                keep(
                    &mut results,
                    TestCaseResult {
                        name: name.to_string(),
                        file: Some(file.clone()),
                        status: status_of(test["status"].as_str().unwrap_or_default()),
                        // Jest reports whole milliseconds, not seconds.
                        duration_ms: test["duration"]
                            .as_f64()
                            .filter(|d| d.is_finite() && *d >= 0.0)
                            .map(|d| d.round() as u64),
                        message: (!messages.is_empty()).then(|| clean(&messages.join("\n"))),
                    },
                );
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use clusia_core::tests::TestStatus;

    use super::*;

    fn report(worktree: &std::path::Path) -> String {
        let root = worktree.display();
        serde_json::json!({
            "numTotalTests": 3,
            "testResults": [
                {
                    "name": format!("{root}/src/cart.test.js"),
                    "status": "failed",
                    "assertionResults": [
                        {"fullName": "cart adds items", "title": "adds items",
                         "status": "passed", "duration": 3, "failureMessages": []},
                        {"fullName": "cart removes items", "title": "removes items",
                         "status": "failed", "duration": 7,
                         "failureMessages": ["\u{1b}[31mError: expected 1\u{1b}[39m\n    at Object.<anonymous>"]},
                        {"fullName": "cart later", "title": "later",
                         "status": "pending", "duration": null, "failureMessages": []}
                    ]
                },
                {
                    "name": format!("{root}/src/broken.test.js"),
                    "status": "failed",
                    "message": "SyntaxError: Unexpected token",
                    "assertionResults": []
                }
            ]
        })
        .to_string()
    }

    #[test]
    fn the_report_gives_results_with_relative_files_and_clean_messages() {
        let dir = tempfile::tempdir().unwrap();
        let all = Jest.finish(dir.path(), &report(dir.path()));
        let shown: Vec<_> = all
            .iter()
            .map(|r| (r.name.as_str(), r.file.as_deref(), r.status, r.duration_ms))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "cart adds items",
                    Some("src/cart.test.js"),
                    TestStatus::Passed,
                    Some(3)
                ),
                (
                    "cart removes items",
                    Some("src/cart.test.js"),
                    TestStatus::Failed,
                    Some(7)
                ),
                (
                    "cart later",
                    Some("src/cart.test.js"),
                    TestStatus::Skipped,
                    None
                ),
                (
                    "src/broken.test.js",
                    Some("src/broken.test.js"),
                    TestStatus::Failed,
                    None
                ),
            ]
        );
        assert_eq!(
            all[1].message.as_deref(),
            Some("Error: expected 1\n    at Object.<anonymous>")
        );
        assert_eq!(
            all[3].message.as_deref(),
            Some("SyntaxError: Unexpected token")
        );
    }

    #[test]
    fn the_report_is_found_after_other_output() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = format!("> jest --json\n\n{}\n", report(dir.path()));
        assert_eq!(Jest.finish(dir.path(), &stdout).len(), 4);
        let spread = format!("noise {{ not json\n{}", report(dir.path()));
        assert_eq!(Jest.finish(dir.path(), &spread).len(), 4);
    }

    #[test]
    fn no_report_gives_no_results() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Jest.finish(dir.path(), "").is_empty());
        assert!(Jest.finish(dir.path(), "{\"other\": 1}").is_empty());
        assert!(
            Jest.finish(dir.path(), "x } y {").is_empty(),
            "a closing brace before the first opening one"
        );
        assert!(Jest.finish(dir.path(), "}{").is_empty());
        assert!(Jest.line("{\"testResults\": []}").is_empty());
    }
}
