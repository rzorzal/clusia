//! `tests/<owner~repo~n>.json`: what the test setup and the test runs of a review know, so a
//! reopened review shows them. Losing it costs a new setup, never a review.

use std::fs;
use std::io;

use clusia_core::tests::TestsRecord;
use clusia_core::{Paths, PrRef};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

pub fn save_tests(paths: &Paths, pr: &PrRef, record: &TestsRecord) -> io::Result<()> {
    let json = serde_json::to_vec(record).map_err(io::Error::other)?;
    write_atomic(&paths.tests_file(pr), &json)
}

/// The saved record, or the default when there is no file. An unreadable file is moved aside.
pub fn load_tests(paths: &Paths, pr: &PrRef) -> io::Result<TestsRecord> {
    let path = paths.tests_file(pr);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(TestsRecord::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(record) => Ok(record),
        Err(e) => {
            let moved = quarantine(&path, now_unix())?;
            tracing::warn!(error = %e, file = %moved.display(), "the tests record was unreadable and was quarantined");
            Ok(TestsRecord::default())
        }
    }
}

/// Removes the record, the logs, the temp folder and the trusted files of `pr` (its sandbox
/// profiles, compose override and image build context, which the daemon names after the temp
/// folder); what is missing is fine.
pub fn delete_tests(paths: &Paths, pr: &PrRef) -> io::Result<()> {
    let ignore_missing = |result: io::Result<()>| match result {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    ignore_missing(fs::remove_file(paths.tests_file(pr)))?;
    ignore_missing(fs::remove_dir_all(paths.test_logs_for(pr)))?;
    let tmp = paths.test_tmp_for(pr);
    ignore_missing(fs::remove_dir_all(&tmp))?;
    let Some(short) = tmp.file_name() else {
        return Ok(());
    };
    ignore_missing(fs::remove_dir_all(
        paths.tests_dir().join("build").join(short),
    ))?;
    // `<short>-offline.sb`, `<short>-compose.yml`, … and their `.tmp` leftovers. The short
    // name has a fixed length, so no other review's files share the prefix.
    let prefix = format!("{}-", short.to_string_lossy());
    let entries = match fs::read_dir(paths.tests_dir().join("profiles")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            ignore_missing(fs::remove_file(entry.path()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clusia_core::tests::{RunOutcome, RunnerKind, TestRun, TestScope, Totals};

    use super::*;

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn setup() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        (dir, paths)
    }

    fn record() -> TestsRecord {
        TestsRecord {
            written_hash: Some("a".repeat(64)),
            runs: vec![TestRun {
                id: "r1".into(),
                head: "h".repeat(40),
                scope: TestScope::All,
                runner: RunnerKind::Local,
                command: "cargo test".into(),
                started_at: 1,
                duration_ms: 2,
                exit_code: Some(0),
                outcome: RunOutcome::Passed,
                totals: Totals {
                    passed: 3,
                    failed: 0,
                    skipped: 0,
                },
                results: Vec::new(),
                log: Some("r1.log".into()),
                tail: "ok".into(),
            }],
            ..TestsRecord::default()
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_record_and_nothing_is_written() {
        let (_d, p) = setup();
        assert_eq!(load_tests(&p, &pr()).unwrap(), TestsRecord::default());
        assert!(!p.tests_dir().exists());
    }

    #[test]
    fn save_load_round_trip_per_pull_request() {
        let (_d, p) = setup();
        save_tests(&p, &pr(), &record()).unwrap();
        assert!(p.tests_file(&pr()).exists());
        assert_eq!(load_tests(&p, &pr()).unwrap(), record());
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        assert_eq!(load_tests(&p, &other).unwrap(), TestsRecord::default());
    }

    #[test]
    fn delete_removes_the_file_the_logs_the_temp_folder_and_the_trusted_files() {
        let (_d, p) = setup();
        save_tests(&p, &pr(), &record()).unwrap();
        fs::create_dir_all(p.test_logs_for(&pr())).unwrap();
        fs::write(p.test_log(&pr(), "r1"), "log").unwrap();
        fs::create_dir_all(p.test_tmp_for(&pr())).unwrap();
        fs::write(p.test_tmp_for(&pr()).join("scratch"), "x").unwrap();
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        save_tests(&p, &other, &record()).unwrap();
        let tmp_name = |pr: &PrRef| {
            p.test_tmp_for(pr)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let profiles = p.tests_dir().join("profiles");
        let (mine, theirs) = (tmp_name(&pr()), tmp_name(&other));
        let suffixes = [
            "-offline.sb",
            "-online.sb",
            "-compose.yml",
            "-offline.sb.tmp",
            "-online.sb.tmp",
        ];
        fs::create_dir_all(&profiles).unwrap();
        for name in [&mine, &theirs] {
            for suffix in suffixes {
                fs::write(profiles.join(format!("{name}{suffix}")), "x").unwrap();
            }
            fs::create_dir_all(p.tests_dir().join("build").join(name)).unwrap();
            fs::write(
                p.tests_dir().join("build").join(name).join("Dockerfile"),
                "x",
            )
            .unwrap();
        }

        delete_tests(&p, &pr()).unwrap();
        assert!(!p.tests_file(&pr()).exists());
        assert!(!p.test_logs_for(&pr()).exists());
        assert!(!p.test_tmp_for(&pr()).exists());
        for suffix in suffixes {
            assert!(
                !profiles.join(format!("{mine}{suffix}")).exists(),
                "{suffix}"
            );
            assert!(
                profiles.join(format!("{theirs}{suffix}")).exists(),
                "{suffix}"
            );
        }
        assert!(!p.tests_dir().join("build").join(&mine).exists());
        assert!(p.tests_dir().join("build").join(&theirs).exists());
        assert!(p.tests_file(&other).exists(), "another review is untouched");
        delete_tests(&p, &pr()).unwrap();
    }

    #[test]
    fn an_unreadable_record_is_quarantined() {
        let (_d, p) = setup();
        let file = p.tests_file(&pr());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{ not json").unwrap();
        assert_eq!(load_tests(&p, &pr()).unwrap(), TestsRecord::default());
        assert!(!file.exists());
        let names: Vec<String> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("acme~widgets~7.json.corrupt-")),
            "{names:?}"
        );
    }
}
