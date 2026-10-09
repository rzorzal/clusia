//! No test may start the real `claude`. A test that starts one of Clúsia's programs has to set
//! `CLUSIA_CLAUDE_BIN` to a program that is not there; this lists the ones that do not.

use std::path::{Path, PathBuf};

/// What a test source contains when it starts the daemon, the CLI or the app.
const STARTS_A_PROGRAM: [&str; 5] = [
    "CARGO_BIN_EXE_clusiad",
    "CARGO_BIN_EXE_clusia\"",
    "CARGO_BIN_EXE_clusia-app",
    "CLUSIA_DAEMON_BIN",
    "clusiad_bin(",
];

fn sources_under(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources_under(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn every_test_that_starts_a_program_keeps_the_real_agent_out() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut sources = Vec::new();
    for krate in std::fs::read_dir(crates).unwrap().flatten() {
        sources_under(&krate.path().join("tests"), &mut sources);
    }
    assert!(
        sources.len() > 20,
        "found only {} test sources",
        sources.len()
    );
    let offenders: Vec<&PathBuf> = sources
        .iter()
        .filter(|file| !file.ends_with("clusiad/tests/no_real_agent.rs"))
        .filter(|file| {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            STARTS_A_PROGRAM.iter().any(|marker| text.contains(marker))
                && !text.contains("CLUSIA_CLAUDE_BIN")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "these tests start a program without setting CLUSIA_CLAUDE_BIN: {offenders:?}"
    );
}
