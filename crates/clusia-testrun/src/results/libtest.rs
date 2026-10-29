//! `cargo test` output: `test name ... ok` lines and the `failures:` sections.

use std::collections::HashMap;
use std::path::Path;

use clusia_core::tests::{TestCaseResult, TestStatus};

use super::{Buffered, ResultParser, clean, keep, millis, strip_ansi};

#[derive(Default)]
pub(super) struct Libtest {
    /// The binary whose tests print next, from its `Running` line; none for doc tests.
    file: Option<String>,
    results: Vec<TestCaseResult>,
    /// The failure section being read: the test's name and its lines so far, capped.
    open: Option<(String, Buffered)>,
    messages: HashMap<String, String>,
}

impl Libtest {
    fn close(&mut self) {
        if let Some((name, lines)) = self.open.take() {
            self.messages.insert(name, clean(&lines.lines.join("\n")));
        }
    }
}

/// `Running unittests src/lib.rs (target/…)` gives `Some(Some("src/lib.rs"))`; `Doc-tests x`
/// gives `Some(None)`; any other line gives `None`.
fn binary_file(line: &str) -> Option<Option<String>> {
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix("Running ") {
        let rest = rest.strip_prefix("unittests ").unwrap_or(rest);
        return Some(Some(rest.split(" (").next().unwrap_or(rest).to_string()));
    }
    line.starts_with("Doc-tests ").then_some(None)
}

/// A trailing ` <0.250s>` (`--report-time`) split off the outcome.
fn split_time(outcome: &str) -> (&str, Option<u64>) {
    let outcome = outcome.trim_end();
    if let Some(open) = outcome.rfind(" <")
        && let Some(inner) = outcome[open + 2..].strip_suffix('>')
        && let Some(seconds) = inner.strip_suffix('s').and_then(|s| s.parse::<f64>().ok())
    {
        return (outcome[..open].trim_end(), millis(seconds));
    }
    (outcome, None)
}

fn test_line(line: &str) -> Option<(String, TestStatus, Option<u64>)> {
    let (name, outcome) = line.strip_prefix("test ")?.rsplit_once(" ... ")?;
    let (outcome, duration) = split_time(outcome);
    let status = if outcome == "ok" {
        TestStatus::Passed
    } else if outcome.starts_with("FAILED") {
        TestStatus::Failed
    } else if outcome.starts_with("ignored") {
        TestStatus::Skipped
    } else {
        return None;
    };
    let name = name.strip_suffix(" - should panic").unwrap_or(name);
    Some((name.to_string(), status, duration))
}

impl ResultParser for Libtest {
    fn line(&mut self, raw: &str) -> Vec<TestCaseResult> {
        let text = strip_ansi(raw);
        let line = text.trim_end();
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            self.close();
            self.open = Some((name.to_string(), Buffered::default()));
            return Vec::new();
        }
        if let Some((_, lines)) = &mut self.open {
            let ends = line == "failures:"
                || line.starts_with("test result:")
                || binary_file(line).is_some();
            if !ends {
                lines.push(line);
                return Vec::new();
            }
            self.close();
        }
        if let Some(file) = binary_file(line) {
            self.file = file;
            return Vec::new();
        }
        let Some((name, status, duration_ms)) = test_line(line) else {
            return Vec::new();
        };
        // A doc test is named `<file> - <item> (line n)`.
        let file = match (&self.file, name.split_once(" - ")) {
            (Some(file), _) => Some(file.clone()),
            (None, Some((file, _))) => Some(file.to_string()),
            (None, None) => None,
        };
        let result = TestCaseResult {
            name,
            file,
            status,
            duration_ms,
            message: None,
        };
        keep(&mut self.results, result.clone());
        vec![result]
    }

    fn finish(&mut self, _worktree: &Path, _stdout: &str) -> Vec<TestCaseResult> {
        self.close();
        let mut results = std::mem::take(&mut self.results);
        for result in &mut results {
            if result.status == TestStatus::Failed {
                result.message = self.messages.get(&result.name).cloned();
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clusia_core::tests::{TestCaseResult, TestStatus};

    use super::*;

    fn run(text: &str) -> (Vec<TestCaseResult>, Vec<TestCaseResult>) {
        let mut parser = Libtest::default();
        let live: Vec<_> = text.lines().flat_map(|l| parser.line(l)).collect();
        (live, parser.finish(Path::new("."), ""))
    }

    fn result(name: &str, file: Option<&str>, status: TestStatus) -> TestCaseResult {
        TestCaseResult {
            name: name.into(),
            file: file.map(Into::into),
            status,
            duration_ms: None,
            message: None,
        }
    }

    const OUTPUT: &str = "\
     Running unittests src/lib.rs (target/debug/deps/widgets-1a2b)

running 4 tests
test checks::tests::adds ... ok
test checks::tests::subtracts ... FAILED
test checks::tests::slow ... ignored
test checks::tests::timed ... ok <0.250s>

failures:

---- checks::tests::subtracts stdout ----
thread 'checks::tests::subtracts' panicked at src/lib.rs:12:9:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    checks::tests::subtracts

test result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    #[test]
    fn results_come_from_the_status_lines_and_the_failure_gets_its_message() {
        let (live, all) = run(OUTPUT);
        assert_eq!(
            live.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            [
                "checks::tests::adds",
                "checks::tests::subtracts",
                "checks::tests::slow",
                "checks::tests::timed"
            ]
        );
        assert_eq!(
            live[1].message, None,
            "the message comes after the status line"
        );
        let mut expected = vec![
            result(
                "checks::tests::adds",
                Some("src/lib.rs"),
                TestStatus::Passed,
            ),
            result(
                "checks::tests::subtracts",
                Some("src/lib.rs"),
                TestStatus::Failed,
            ),
            result(
                "checks::tests::slow",
                Some("src/lib.rs"),
                TestStatus::Skipped,
            ),
            result(
                "checks::tests::timed",
                Some("src/lib.rs"),
                TestStatus::Passed,
            ),
        ];
        expected[1].message = Some(
            "thread 'checks::tests::subtracts' panicked at src/lib.rs:12:9:\n\
             assertion `left == right` failed\n  left: 1\n right: 2"
                .into(),
        );
        expected[3].duration_ms = Some(250);
        assert_eq!(all, expected);
    }

    #[test]
    fn doc_tests_name_their_file_and_a_should_panic_suffix_is_dropped() {
        let (_, all) = run("   Doc-tests widgets\n\
             test src/lib.rs - add (line 5) ... ok\n\
             test checks::tests::boom - should panic ... ok\n");
        assert_eq!(
            all,
            [
                result(
                    "src/lib.rs - add (line 5)",
                    Some("src/lib.rs"),
                    TestStatus::Passed
                ),
                result("checks::tests::boom", None, TestStatus::Passed),
            ]
        );
    }

    #[test]
    fn each_binary_has_its_own_file() {
        let (_, all) = run("     Running unittests src/lib.rs (target/debug/deps/a)\n\
             test a::one ... ok\n\
             \n\
             test result: ok. 1 passed; 0 failed\n\
             \n\
             \x20    Running tests/it.rs (target/debug/deps/it)\n\
             test two ... ok\n");
        assert_eq!(all[0].file.as_deref(), Some("src/lib.rs"));
        assert_eq!(all[1].file.as_deref(), Some("tests/it.rs"));
    }

    #[test]
    fn the_result_line_and_unknown_lines_are_not_tests() {
        let (live, all) = run("test result: ok. 3 passed; 0 failed\n\
             test bench_it ... bench:  1,000 ns/iter (+/- 10)\n\
             warning: unused variable\n");
        assert!(live.is_empty() && all.is_empty());
    }

    #[test]
    fn a_long_failure_section_is_buffered_up_to_a_cap() {
        let mut parser = Libtest::default();
        parser.line("test a::b ... FAILED");
        parser.line("---- a::b stdout ----");
        for _ in 0..10_000 {
            parser.line(&"x".repeat(100));
        }
        let held = parser.open.as_ref().unwrap().1.bytes();
        assert!(held <= crate::results::BUFFER_MAX, "{held}");
        let all = parser.finish(Path::new("."), "");
        let message = all[0].message.as_deref().unwrap();
        assert!(message.len() <= clusia_core::tests::MESSAGE_MAX);
    }

    #[test]
    fn colour_codes_do_not_hide_a_result() {
        let (_, all) = run("test a::b ... \u{1b}[32mok\u{1b}[0m\n");
        assert_eq!(all[0].status, TestStatus::Passed);
    }
}
