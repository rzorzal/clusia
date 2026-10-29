//! JUnit XML: one file, or the `*.xml` files of one folder, inside the worktree.

use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use clusia_core::printable::plain_relative;
use clusia_core::tests::{TestCaseResult, TestStatus};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use super::{ResultParser, clean, keep, millis};

/// The most report files read from a folder.
const MAX_FILES: usize = 500;
/// The largest report file read, in bytes.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const PATH_MAX: usize = 1024;

pub(super) struct Junit {
    path: String,
}

impl Junit {
    pub(super) fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
        }
    }
}

/// The report files `path` names, none of them reached through a symlink.
fn report_files(worktree: &Path, path: &str) -> Vec<PathBuf> {
    if !plain_relative(path, PATH_MAX) {
        return Vec::new();
    }
    let Ok(root) = worktree.canonicalize() else {
        return Vec::new();
    };
    let target = root.join(path);
    let Ok(meta) = fs::symlink_metadata(&target) else {
        return Vec::new();
    };
    // An earlier component may be a link too: the real path must stay inside the worktree.
    let Ok(real) = target.canonicalize() else {
        return Vec::new();
    };
    if meta.file_type().is_symlink() || !real.starts_with(&root) {
        return Vec::new();
    }
    if meta.is_file() {
        return vec![real];
    }
    let Ok(entries) = fs::read_dir(&real) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("xml"))
        })
        .filter(|e| {
            e.metadata()
                .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    files.truncate(MAX_FILES);
    files
}

/// The text of a report file: opened without following a link and read up to
/// `MAX_FILE_BYTES`; a file that is longer is skipped, whatever its length said before the open.
fn read_capped(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_FILE_BYTES).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

fn attr(element: &BytesStart<'_>, key: &str) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| AsRef::<str>::as_ref(&a.key) == key)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// The character an XML entity name stands for: the five predefined ones and `#n` / `#xn`.
fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Failure,
    Skipped,
}

/// A `<failure>`, `<error>` or `<skipped>` being read.
struct Slot {
    kind: Kind,
    message: Option<String>,
    text: String,
}

struct Case {
    name: String,
    file: Option<String>,
    duration_ms: Option<u64>,
    status: TestStatus,
    message: Option<String>,
}

impl Case {
    fn open(element: &BytesStart<'_>) -> Self {
        let name = attr(element, "name").unwrap_or_default();
        let name = match attr(element, "classname").filter(|c| !c.is_empty()) {
            Some(class) if !name.starts_with(&class) => format!("{class}.{name}"),
            _ => name,
        };
        Self {
            name,
            file: attr(element, "file").filter(|f| !f.is_empty()),
            duration_ms: attr(element, "time")
                .and_then(|t| t.trim().parse::<f64>().ok())
                .and_then(millis),
            status: TestStatus::Passed,
            message: None,
        }
    }

    fn close_slot(&mut self, slot: Slot) {
        let text = slot.text.trim();
        let parts: Vec<&str> = [slot.message.as_deref(), Some(text)]
            .into_iter()
            .flatten()
            .filter(|p| !p.is_empty())
            .collect();
        let message = (!parts.is_empty()).then(|| clean(&parts.join("\n")));
        match slot.kind {
            Kind::Failure => {
                self.status = TestStatus::Failed;
                self.message = message;
            }
            Kind::Skipped => {
                if self.status == TestStatus::Passed {
                    self.status = TestStatus::Skipped;
                    self.message = message;
                }
            }
        }
    }

    fn into_result(self) -> TestCaseResult {
        TestCaseResult {
            name: self.name,
            file: self.file,
            status: self.status,
            duration_ms: self.duration_ms,
            message: self.message,
        }
    }
}

fn slot_kind(name: &str) -> Option<Kind> {
    match name {
        "failure" | "error" => Some(Kind::Failure),
        "skipped" => Some(Kind::Skipped),
        _ => None,
    }
}

/// Reads every `<testcase>` of `text` into `out`. A file that is cut off keeps the cases that
/// closed before the cut.
fn read_report(text: &str, out: &mut Vec<TestCaseResult>) {
    let mut reader = Reader::from_str(text);
    let mut case: Option<Case> = None;
    let mut slot: Option<Slot> = None;
    loop {
        let Ok(event) = reader.read_event() else {
            return;
        };
        match event {
            Event::Start(e) => match AsRef::<str>::as_ref(&e.name()) {
                "testcase" => case = Some(Case::open(&e)),
                name => {
                    if let Some(kind) = slot_kind(name)
                        && case.is_some()
                    {
                        slot = Some(Slot {
                            kind,
                            message: attr(&e, "message"),
                            text: String::new(),
                        });
                    }
                }
            },
            Event::Empty(e) => match AsRef::<str>::as_ref(&e.name()) {
                "testcase" => keep(out, Case::open(&e).into_result()),
                name => {
                    if let Some(kind) = slot_kind(name)
                        && let Some(open) = case.as_mut()
                    {
                        open.close_slot(Slot {
                            kind,
                            message: attr(&e, "message"),
                            text: String::new(),
                        });
                    }
                }
            },
            Event::End(e) => match AsRef::<str>::as_ref(&e.name()) {
                "testcase" => {
                    if let Some(done) = case.take() {
                        keep(out, done.into_result());
                    }
                    slot = None;
                }
                name if slot_kind(name).is_some() => {
                    if let (Some(open), Some(done)) = (case.as_mut(), slot.take()) {
                        open.close_slot(done);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some(open) = slot.as_mut() {
                    open.text.push_str(&t);
                }
            }
            Event::CData(t) => {
                if let Some(open) = slot.as_mut() {
                    open.text.push_str(&t);
                }
            }
            // `&amp;` and its kind arrive as events of their own, between two pieces of text.
            Event::GeneralRef(r) => {
                if let Some(open) = slot.as_mut()
                    && let Some(c) = entity(&r)
                {
                    open.text.push(c);
                }
            }
            Event::Eof => return,
            _ => {}
        }
    }
}

impl ResultParser for Junit {
    fn line(&mut self, _line: &str) -> Vec<TestCaseResult> {
        Vec::new()
    }

    fn finish(&mut self, worktree: &Path, _stdout: &str) -> Vec<TestCaseResult> {
        let mut results = Vec::new();
        for file in report_files(worktree, &self.path) {
            if let Some(text) = read_capped(&file) {
                read_report(&text, &mut results);
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use clusia_core::tests::TestStatus;

    use super::*;

    const REPORT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="widgets" tests="4">
    <testcase classname="tests.test_pay" name="test_total" file="tests/test_pay.py" time="0.012"/>
    <testcase classname="tests.test_pay" name="test_refund" time="1.5">
      <failure message="assert 3 == 4" type="AssertionError">Traceback (most recent call last):
  File "tests/test_pay.py", line 9
AssertionError: a &amp; b</failure>
    </testcase>
    <testcase classname="tests.test_pay" name="test_later"><skipped message="not today"/></testcase>
    <testcase classname="" name="boom"><error message="exploded"/></testcase>
  </testsuite>
</testsuites>
"#;

    fn parser(path: &str) -> Junit {
        Junit::new(path)
    }

    #[test]
    fn a_report_file_gives_results() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("reports")).unwrap();
        fs::write(dir.path().join("reports/a.xml"), REPORT).unwrap();
        let all = parser("reports/a.xml").finish(dir.path(), "");
        let shown: Vec<_> = all
            .iter()
            .map(|r| (r.name.as_str(), r.file.as_deref(), r.status, r.duration_ms))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "tests.test_pay.test_total",
                    Some("tests/test_pay.py"),
                    TestStatus::Passed,
                    Some(12)
                ),
                (
                    "tests.test_pay.test_refund",
                    None,
                    TestStatus::Failed,
                    Some(1500)
                ),
                ("tests.test_pay.test_later", None, TestStatus::Skipped, None),
                ("boom", None, TestStatus::Failed, None),
            ]
        );
        assert_eq!(
            all[1].message.as_deref(),
            Some(
                "assert 3 == 4\nTraceback (most recent call last):\n  File \"tests/test_pay.py\", line 9\nAssertionError: a & b"
            )
        );
        assert_eq!(all[2].message.as_deref(), Some("not today"));
        assert_eq!(all[3].message.as_deref(), Some("exploded"));
    }

    #[test]
    fn a_folder_gives_every_xml_file_in_name_order_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("reports");
        fs::create_dir_all(reports.join("nested")).unwrap();
        let one = |name: &str| {
            format!(r#"<testsuite><testcase classname="" name="{name}"/></testsuite>"#)
        };
        fs::write(reports.join("b.xml"), one("second")).unwrap();
        fs::write(reports.join("a.xml"), one("first")).unwrap();
        fs::write(reports.join("notes.txt"), one("ignored")).unwrap();
        fs::write(reports.join("nested/c.xml"), one("nested")).unwrap();
        let all = parser("reports").finish(dir.path(), "");
        let names: Vec<_> = all.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn a_symlink_and_a_path_outside_the_worktree_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.xml"), REPORT).unwrap();
        fs::create_dir_all(dir.path().join("reports")).unwrap();
        symlink(
            outside.path().join("secret.xml"),
            dir.path().join("reports/link.xml"),
        )
        .unwrap();
        assert!(parser("reports").finish(dir.path(), "").is_empty());
        assert!(parser("reports/link.xml").finish(dir.path(), "").is_empty());
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        assert!(
            parser("linked/secret.xml")
                .finish(dir.path(), "")
                .is_empty()
        );
        for bad in ["../secret.xml", "/etc/hosts", "a\\b.xml"] {
            assert!(parser(bad).finish(dir.path(), "").is_empty(), "{bad}");
        }
    }

    #[test]
    fn a_missing_or_broken_report_gives_nothing_and_a_half_written_one_gives_what_closed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parser("nope.xml").finish(dir.path(), "").is_empty());
        fs::write(dir.path().join("bad.xml"), "not xml at all <<<").unwrap();
        assert!(parser("bad.xml").finish(dir.path(), "").is_empty());
        fs::write(
            dir.path().join("cut.xml"),
            r#"<testsuite><testcase classname="" name="done"/><testcase classname="" name="cut">"#,
        )
        .unwrap();
        let all = parser("cut.xml").finish(dir.path(), "");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "done");
    }

    #[test]
    fn a_report_over_the_size_cap_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let cap = usize::try_from(MAX_FILE_BYTES).unwrap();
        let mut big = br#"<testsuite><testcase classname="" name="big"/></testsuite>"#.to_vec();
        big.resize(cap + 1, b' ');
        fs::write(dir.path().join("big.xml"), &big).unwrap();
        assert!(parser("big.xml").finish(dir.path(), "").is_empty());
        big.truncate(cap);
        fs::write(dir.path().join("fits.xml"), &big).unwrap();
        assert_eq!(parser("fits.xml").finish(dir.path(), "").len(), 1);
    }

    #[test]
    fn a_name_that_already_starts_with_its_class_is_not_doubled() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("r.xml"),
            r#"<testsuite><testcase classname="cart.test" name="cart.test adds"/></testsuite>"#,
        )
        .unwrap();
        let all = parser("r.xml").finish(dir.path(), "");
        assert_eq!(all[0].name, "cart.test adds");
    }

    #[test]
    fn a_line_gives_nothing() {
        assert!(parser("r.xml").line("<testcase name=\"a\"/>").is_empty());
    }
}
