//! `pytest -v`: `path::name STATUS [ nn%]` lines and the `FAILED path::name - text` summary.

use std::collections::HashMap;
use std::path::Path;

use clusia_core::tests::{TestCaseResult, TestStatus};

use super::{ResultParser, clean, keep, strip_ansi};

const STATUSES: [(&str, TestStatus); 6] = [
    ("PASSED", TestStatus::Passed),
    ("FAILED", TestStatus::Failed),
    ("SKIPPED", TestStatus::Skipped),
    ("XFAIL", TestStatus::Skipped),
    ("XPASS", TestStatus::Passed),
    ("ERROR", TestStatus::Failed),
];

#[derive(Default)]
pub(super) struct Pytest {
    results: Vec<TestCaseResult>,
    messages: HashMap<String, String>,
}

/// A summary line `FAILED id - text` or `ERROR id - text`, as (id, text).
fn summary(line: &str) -> Option<(&str, &str)> {
    let rest = line
        .strip_prefix("FAILED ")
        .or_else(|| line.strip_prefix("ERROR "))?;
    let (id, text) = rest.split_once(" - ").unwrap_or((rest, ""));
    id.contains("::").then_some((id.trim(), text.trim()))
}

fn status_of(word: &str) -> Option<TestStatus> {
    STATUSES.iter().find(|(w, _)| *w == word).map(|(_, s)| *s)
}

/// A verbose line as (id, status): `id STATUS [ 50%]`, `id SKIPPED (why)  [ 50%]` or the
/// xdist form `[gw0] STATUS id`, with or without a `[ 25%]` token before the status.
fn status_line(line: &str) -> Option<(String, TestStatus)> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("[gw") {
        let (_, rest) = rest.split_once("] ")?;
        // Newer xdist prints the progress before the status: `[gw0] [ 25%] PASSED id`.
        let rest = match rest.strip_prefix('[').and_then(|r| r.split_once("%] ")) {
            Some((_, after)) => after,
            None => rest,
        };
        let (word, id) = rest.split_once(' ')?;
        let status = status_of(word)?;
        return id.contains("::").then(|| (id.trim().to_string(), status));
    }
    let mut text = line;
    if text.ends_with(']')
        && let Some(open) = text.rfind('[')
        && text[open..].contains('%')
    {
        text = text[..open].trim_end();
    }
    for (word, status) in STATUSES {
        let Some(at) = text.rfind(&format!(" {word}")) else {
            continue;
        };
        let after = &text[at + 1 + word.len()..];
        if after.is_empty() || after.starts_with(" (") {
            let id = text[..at].trim();
            return id.contains("::").then(|| (id.to_string(), status));
        }
    }
    None
}

impl ResultParser for Pytest {
    fn line(&mut self, raw: &str) -> Vec<TestCaseResult> {
        let text = strip_ansi(raw);
        let line = text.trim_end();
        if let Some((id, message)) = summary(line) {
            if !message.is_empty() {
                self.messages.insert(id.to_string(), clean(message));
            }
            return Vec::new();
        }
        let Some((name, status)) = status_line(line) else {
            return Vec::new();
        };
        let file = name.split("::").next().map(str::to_string);
        let result = TestCaseResult {
            name,
            file,
            status,
            duration_ms: None,
            message: None,
        };
        keep(&mut self.results, result.clone());
        vec![result]
    }

    fn finish(&mut self, _worktree: &Path, _stdout: &str) -> Vec<TestCaseResult> {
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
        let mut parser = Pytest::default();
        let live: Vec<_> = text.lines().flat_map(|l| parser.line(l)).collect();
        (live, parser.finish(Path::new("."), ""))
    }

    const OUTPUT: &str = "\
============================= test session starts ==============================
collected 4 items

tests/test_pay.py::test_total PASSED                                     [ 25%]
tests/test_pay.py::test_refund FAILED                                    [ 50%]
tests/test_pay.py::TestCart::test_empty SKIPPED (not today)              [ 75%]
tests/test_pay.py::test_param[a b] PASSED                                [100%]

=================================== FAILURES ===================================
_________________________________ test_refund __________________________________
E       assert 3 == 4
=========================== short test summary info ============================
FAILED tests/test_pay.py::test_refund - assert 3 == 4
========================= 1 failed, 2 passed, 1 skipped in 0.12s ===============
";

    #[test]
    fn verbose_lines_give_results_and_the_summary_gives_the_message() {
        let (live, all) = run(OUTPUT);
        assert_eq!(live.len(), 4);
        assert_eq!(live[1].message, None);
        let names: Vec<_> = all.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "tests/test_pay.py::test_total",
                "tests/test_pay.py::test_refund",
                "tests/test_pay.py::TestCart::test_empty",
                "tests/test_pay.py::test_param[a b]",
            ]
        );
        let statuses: Vec<_> = all.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            [
                TestStatus::Passed,
                TestStatus::Failed,
                TestStatus::Skipped,
                TestStatus::Passed
            ]
        );
        assert!(
            all.iter()
                .all(|r| r.file.as_deref() == Some("tests/test_pay.py"))
        );
        assert_eq!(all[1].message.as_deref(), Some("assert 3 == 4"));
        assert_eq!(all[0].message, None);
    }

    #[test]
    fn the_xdist_form_and_the_odd_statuses_are_read() {
        let (_, all) = run("[gw0] PASSED tests/test_a.py::test_one\n\
             [gw1] FAILED tests/test_a.py::test_two\n\
             tests/test_a.py::test_three XFAIL                [ 60%]\n\
             tests/test_a.py::test_four XPASS                 [ 80%]\n\
             tests/test_a.py::test_five ERROR                 [100%]\n");
        let statuses: Vec<_> = all.iter().map(|r| (r.name.as_str(), r.status)).collect();
        assert_eq!(
            statuses,
            [
                ("tests/test_a.py::test_one", TestStatus::Passed),
                ("tests/test_a.py::test_two", TestStatus::Failed),
                ("tests/test_a.py::test_three", TestStatus::Skipped),
                ("tests/test_a.py::test_four", TestStatus::Passed),
                ("tests/test_a.py::test_five", TestStatus::Failed),
            ]
        );
    }

    #[test]
    fn the_xdist_form_with_a_progress_token_is_read() {
        let (live, all) = run("[gw0] [ 25%] PASSED tests/test_x.py::test_a\n\
             [gw1] [ 50%] FAILED tests/test_x.py::test_b\n\
             [gw0] [ 62%] SKIPPED tests/test_x.py::test_c\n\
             [gw1] [ 75%] ERROR tests/test_x.py::test_d\n\
             [gw0] [ 87%] XFAIL tests/test_x.py::test_e\n\
             [gw1] [100%] XPASS tests/test_x.py::test_f\n\
             FAILED tests/test_x.py::test_b - assert 1 == 2\n");
        assert_eq!(live.len(), 6);
        let statuses: Vec<_> = all.iter().map(|r| (r.name.as_str(), r.status)).collect();
        assert_eq!(
            statuses,
            [
                ("tests/test_x.py::test_a", TestStatus::Passed),
                ("tests/test_x.py::test_b", TestStatus::Failed),
                ("tests/test_x.py::test_c", TestStatus::Skipped),
                ("tests/test_x.py::test_d", TestStatus::Failed),
                ("tests/test_x.py::test_e", TestStatus::Skipped),
                ("tests/test_x.py::test_f", TestStatus::Passed),
            ]
        );
        assert_eq!(all[1].message.as_deref(), Some("assert 1 == 2"));
        assert_eq!(all[1].file.as_deref(), Some("tests/test_x.py"));
    }

    #[test]
    fn a_summary_line_is_not_a_result_and_other_lines_are_ignored() {
        let (live, all) = run("FAILED tests/test_a.py::test_two - boom\n\
             rootdir: /work\n\
             plugins: xdist-3.6\n");
        assert!(live.is_empty() && all.is_empty());
    }
}
