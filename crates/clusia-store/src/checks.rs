//! `checks/<owner~repo~n>.json`: what the security check and the audit found in a review, so a
//! reopened review shows it without asking the agent again. Losing it only costs a new run.

use std::fs;
use std::io;

use clusia_core::{CheckResult, Paths, PrRef};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

pub fn save_checks(paths: &Paths, pr: &PrRef, results: &[CheckResult]) -> io::Result<()> {
    let json = serde_json::to_vec(results).map_err(io::Error::other)?;
    write_atomic(&paths.checks_file(pr), &json)
}

/// The saved results, or none when there is no file. An unreadable file is moved aside.
pub fn load_checks(paths: &Paths, pr: &PrRef) -> io::Result<Vec<CheckResult>> {
    let path = paths.checks_file(pr);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(results) => Ok(results),
        Err(e) => {
            let moved = quarantine(&path, now_unix())?;
            tracing::warn!(error = %e, file = %moved.display(), "check results were unreadable and were quarantined");
            Ok(Vec::new())
        }
    }
}

/// Removes the results of `pr`; a missing file is fine.
pub fn delete_checks(paths: &Paths, pr: &PrRef) -> io::Result<()> {
    match fs::remove_file(paths.checks_file(pr)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::{CheckKind, Finding, Pass, Severity};

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn results() -> Vec<CheckResult> {
        vec![
            CheckResult {
                kind: CheckKind::Security,
                head: "h".repeat(40),
                files: 7,
                findings: vec![Finding {
                    id: "0123456789abcdef".into(),
                    kind: CheckKind::Security,
                    area: "security".into(),
                    severity: Some(Severity::High),
                    title: "Token written to the log".into(),
                    file: "src/auth/store.rs".into(),
                    line: Some(52),
                    start_line: None,
                    end_line: None,
                    body: "The token is logged.".into(),
                    comment: "Do not log the token.".into(),
                    code: Some("log::info!(\"{token}\");".into()),
                    anchored: true,
                }],
                passes: Vec::new(),
                unreadable: 1,
                areas: Vec::new(),
                at: 1_700_000_000,
            },
            CheckResult {
                kind: CheckKind::Audit,
                head: "h".repeat(40),
                files: 7,
                findings: Vec::new(),
                passes: vec![Pass {
                    area: "tests".into(),
                    text: "Covered by refresh_race.".into(),
                    place: Some("src/auth/refresh.rs".into()),
                }],
                unreadable: 0,
                areas: vec!["tests".into()],
                at: 1_700_000_100,
            },
        ]
    }

    fn setup() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        (dir, paths)
    }

    #[test]
    fn a_missing_file_is_no_results_and_nothing_is_written() {
        let (_d, p) = setup();
        assert!(load_checks(&p, &pr()).unwrap().is_empty());
        assert!(!p.checks_dir().exists());
    }

    #[test]
    fn save_load_delete_round_trip() {
        let (_d, p) = setup();
        save_checks(&p, &pr(), &results()).unwrap();
        assert!(p.checks_file(&pr()).exists());
        assert_eq!(load_checks(&p, &pr()).unwrap(), results());
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        assert!(load_checks(&p, &other).unwrap().is_empty());
        delete_checks(&p, &pr()).unwrap();
        assert!(!p.checks_file(&pr()).exists());
        delete_checks(&p, &pr()).unwrap();
    }

    #[test]
    fn unreadable_results_are_quarantined() {
        let (_d, p) = setup();
        let file = p.checks_file(&pr());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{ not json").unwrap();
        assert!(load_checks(&p, &pr()).unwrap().is_empty());
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
