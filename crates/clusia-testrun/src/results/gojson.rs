//! `go test -json`: one JSON event per line.

use std::collections::HashMap;
use std::path::Path;

use clusia_core::tests::{TestCaseResult, TestStatus};
use serde_json::Value;

use clusia_core::tests::RESULTS_MAX;

use super::{Buffered, ResultParser, clean, keep, millis};

#[derive(Default)]
pub(super) struct GoJson {
    /// The output of each running test, by package and name: at most `RESULTS_MAX` tests, each
    /// with a capped amount of text.
    output: HashMap<(String, String), Buffered>,
    results: Vec<TestCaseResult>,
}

/// The first `file.go` named by an `file.go:12:` prefix.
fn go_file(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|line| {
        let (file, rest) = line.trim_start().split_once(':')?;
        (file.ends_with(".go") && rest.starts_with(|c: char| c.is_ascii_digit()))
            .then(|| file.to_string())
    })
}

impl ResultParser for GoJson {
    fn line(&mut self, line: &str) -> Vec<TestCaseResult> {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            return Vec::new();
        };
        let (Some(action), Some(test)) = (event["Action"].as_str(), event["Test"].as_str()) else {
            return Vec::new();
        };
        let key = (
            event["Package"].as_str().unwrap_or_default().to_string(),
            test.to_string(),
        );
        let status = match action {
            "output" => {
                if let Some(text) = event["Output"].as_str()
                    && (self.output.contains_key(&key) || self.output.len() < RESULTS_MAX)
                {
                    self.output.entry(key).or_default().push(text);
                }
                return Vec::new();
            }
            "pass" => TestStatus::Passed,
            "fail" => TestStatus::Failed,
            "skip" => TestStatus::Skipped,
            _ => return Vec::new(),
        };
        let lines = self.output.remove(&key).unwrap_or_default().lines;
        let file = go_file(&lines);
        let message = (status == TestStatus::Failed).then(|| {
            let kept: Vec<&str> = lines
                .iter()
                .map(|l| l.trim_end())
                .filter(|l| !l.starts_with("=== ") && !l.starts_with("--- "))
                .collect();
            clean(&kept.join("\n"))
        });
        let result = TestCaseResult {
            name: key.1,
            file,
            status,
            duration_ms: event["Elapsed"].as_f64().and_then(millis),
            message,
        };
        keep(&mut self.results, result.clone());
        vec![result]
    }

    fn finish(&mut self, _worktree: &Path, _stdout: &str) -> Vec<TestCaseResult> {
        std::mem::take(&mut self.results)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clusia_core::tests::{TestCaseResult, TestStatus};

    use super::*;

    const PKG: &str = "example.com/widgets/add";

    fn event(action: &str, test: Option<&str>, extra: serde_json::Value) -> String {
        let mut value = serde_json::json!({"Action": action, "Package": PKG});
        if let Some(test) = test {
            value["Test"] = test.into();
        }
        for (key, v) in extra.as_object().unwrap() {
            value[key] = v.clone();
        }
        value.to_string()
    }

    fn output(test: &str, text: &str) -> String {
        event("output", Some(test), serde_json::json!({ "Output": text }))
    }

    fn run(lines: &[String]) -> Vec<TestCaseResult> {
        let mut parser = GoJson::default();
        for line in lines {
            parser.line(line);
        }
        parser.finish(Path::new("."), "")
    }

    #[test]
    fn a_failure_keeps_its_output_without_the_run_and_fail_lines() {
        let all = run(&[
            event("run", Some("TestAdd"), serde_json::json!({})),
            output("TestAdd", "=== RUN   TestAdd\n"),
            output("TestAdd", "    add_test.go:14: got 3, want 4\n"),
            output("TestAdd", "--- FAIL: TestAdd (0.01s)\n"),
            event(
                "fail",
                Some("TestAdd"),
                serde_json::json!({"Elapsed": 0.012}),
            ),
            event("pass", Some("TestSub"), serde_json::json!({"Elapsed": 0})),
            event("skip", Some("TestSlow"), serde_json::json!({"Elapsed": 0})),
            event("fail", None, serde_json::json!({"Elapsed": 0.05})),
        ]);
        assert_eq!(
            all,
            [
                TestCaseResult {
                    name: "TestAdd".into(),
                    file: Some("add_test.go".into()),
                    status: TestStatus::Failed,
                    duration_ms: Some(12),
                    message: Some("add_test.go:14: got 3, want 4".into()),
                },
                TestCaseResult {
                    name: "TestSub".into(),
                    file: None,
                    status: TestStatus::Passed,
                    duration_ms: Some(0),
                    message: None,
                },
                TestCaseResult {
                    name: "TestSlow".into(),
                    file: None,
                    status: TestStatus::Skipped,
                    duration_ms: Some(0),
                    message: None,
                },
            ],
            "the package's own fail event is not a test"
        );
    }

    #[test]
    fn a_result_is_live_when_its_action_arrives() {
        let mut parser = GoJson::default();
        assert!(
            parser
                .line(&event("run", Some("TestAdd"), serde_json::json!({})))
                .is_empty()
        );
        let live = parser.line(&event(
            "pass",
            Some("TestAdd"),
            serde_json::json!({"Elapsed": 0.5}),
        ));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].duration_ms, Some(500));
    }

    #[test]
    fn the_output_held_per_test_and_the_tests_held_are_capped() {
        let mut parser = GoJson::default();
        for _ in 0..10_000 {
            parser.line(&output("TestBig", &"x".repeat(100)));
        }
        let held = parser.output.values().map(|b| b.bytes()).sum::<usize>();
        assert!(held <= crate::results::BUFFER_MAX, "{held}");
        for n in 0..clusia_core::tests::RESULTS_MAX + 50 {
            parser.line(&output(&format!("TestMany{n}"), "line\n"));
        }
        assert!(parser.output.len() <= clusia_core::tests::RESULTS_MAX);
    }

    #[test]
    fn a_line_that_is_not_json_is_ignored() {
        let mut parser = GoJson::default();
        assert!(
            parser
                .line("FAIL\texample.com/widgets/add\t0.01s")
                .is_empty()
        );
        assert!(parser.line("").is_empty());
        assert!(parser.finish(Path::new("."), "").is_empty());
    }
}
