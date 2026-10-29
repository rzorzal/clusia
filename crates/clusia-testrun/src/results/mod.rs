//! Reads what a test command printed into per-test results, and what a list command printed
//! into the known tests. The text is the PR's: nothing here opens, runs or follows it.

mod gojson;
mod jest;
mod junit;
mod libtest;
mod list;
mod pytest;

use std::path::Path;

use clusia_core::tests::{MESSAGE_MAX, ResultsFormat, TestCaseResult};

pub use list::parse_list;

/// Turns the output of one run into results.
pub trait ResultParser: Send {
    /// One complete line of the run's output (no line break), from either stream, in the order
    /// it arrived. Returns the results that line finished, for live `TestResult` events.
    fn line(&mut self, line: &str) -> Vec<TestCaseResult>;
    /// The end of the run. Returns the run's full list of results, with their messages, in the
    /// order the tests finished, at most `RESULTS_MAX`; the daemon stores it as the run's
    /// results. What `line` returned is only for the live events.
    fn finish(&mut self, worktree: &Path, stdout: &str) -> Vec<TestCaseResult>;
}

/// The whole run is one pass or fail from its exit code: the daemon sets it, so there is
/// nothing to read.
struct Plain;

impl ResultParser for Plain {
    fn line(&mut self, _line: &str) -> Vec<TestCaseResult> {
        Vec::new()
    }

    fn finish(&mut self, _worktree: &Path, _stdout: &str) -> Vec<TestCaseResult> {
        Vec::new()
    }
}

/// The parser for a recipe's `results` format.
pub fn parser_for(format: &ResultsFormat) -> Box<dyn ResultParser> {
    match format {
        ResultsFormat::Junit { path } => Box::new(junit::Junit::new(path)),
        ResultsFormat::Libtest => Box::<libtest::Libtest>::default(),
        ResultsFormat::GoJson => Box::<gojson::GoJson>::default(),
        ResultsFormat::Pytest => Box::<pytest::Pytest>::default(),
        ResultsFormat::Jest => Box::new(jest::Jest),
        ResultsFormat::Plain => Box::new(Plain),
    }
}

/// `text` without ANSI colour and cursor sequences (`ESC [ … letter`).
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for n in chars.by_ref() {
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A message as stored: no colour codes, trimmed, at most `MESSAGE_MAX` bytes, cut at a
/// character boundary and marked with a final `…`.
pub(crate) fn clean(text: &str) -> String {
    let text = strip_ansi(text);
    let text = text.trim();
    if text.len() <= MESSAGE_MAX {
        return text.to_string();
    }
    let mut end = MESSAGE_MAX - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The most text a parser buffers for one test's message, before it is cut to `MESSAGE_MAX`.
pub(crate) const BUFFER_MAX: usize = MESSAGE_MAX * 4;

/// Lines held for one test, never more than `BUFFER_MAX` bytes of them.
#[derive(Default)]
pub(crate) struct Buffered {
    pub lines: Vec<String>,
    bytes: usize,
}

impl Buffered {
    /// Keeps `line` while there is room; the rest of a long output is dropped.
    pub fn push(&mut self, line: &str) {
        if self.bytes + line.len() < BUFFER_MAX {
            self.bytes += line.len() + 1;
            self.lines.push(line.to_string());
        }
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Pushes `result` unless the run already holds `RESULTS_MAX`.
pub(crate) fn keep(results: &mut Vec<TestCaseResult>, result: TestCaseResult) {
    if results.len() < clusia_core::tests::RESULTS_MAX {
        results.push(result);
    }
}

/// Seconds as whole milliseconds.
pub(crate) fn millis(seconds: f64) -> Option<u64> {
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clusia_core::tests::{MESSAGE_MAX, RESULTS_MAX, ResultsFormat, TestStatus};

    use super::*;

    #[test]
    fn colour_codes_are_removed_and_the_text_trimmed() {
        assert_eq!(
            clean("\u{1b}[31mError: expected 1\u{1b}[39m\n   "),
            "Error: expected 1"
        );
    }

    #[test]
    fn a_long_message_is_cut_at_a_character_boundary_with_an_ellipsis() {
        let long = "x".repeat(MESSAGE_MAX + 50);
        let cut = clean(&long);
        assert_eq!(cut.len(), MESSAGE_MAX);
        assert!(cut.ends_with('…'));
        let accents = "é".repeat(MESSAGE_MAX);
        let cut = clean(&accents);
        assert!(cut.len() <= MESSAGE_MAX && cut.ends_with('…'));
        assert!(cut.is_char_boundary(cut.len() - '…'.len_utf8()));
    }

    #[test]
    fn the_plain_format_gives_nothing() {
        let mut parser = parser_for(&ResultsFormat::Plain);
        assert!(parser.line("test a ... ok").is_empty());
        assert!(parser.finish(Path::new("."), "test a ... ok").is_empty());
    }

    #[test]
    fn a_run_keeps_at_most_results_max_results() {
        let mut parser = parser_for(&ResultsFormat::Libtest);
        for n in 0..RESULTS_MAX + 100 {
            parser.line(&format!("test t{n} ... ok"));
        }
        let all = parser.finish(Path::new("."), "");
        assert_eq!(all.len(), RESULTS_MAX);
        assert!(all.iter().all(|r| r.status == TestStatus::Passed));
    }
}
