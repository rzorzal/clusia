//! `diagrams/<owner~repo~n>/v<n>.json`: the drawings of a review, one file per version, so a
//! reopened review shows them without asking the agent again.

use std::fs;
use std::io;
use std::path::Path;

use clusia_core::diagram::{DiagramVersion, MAX_DIAGRAMS, MAX_SPEC_BYTES};
use clusia_core::keywords::MAX_MAP_BYTES;
use clusia_core::{Paths, PrRef};

use crate::atomic::write_atomic;

/// The biggest file a version can be: every drawing at its cap, a keyword map at its cap, and
/// room for the rest.
const MAX_VERSION_BYTES: u64 = (MAX_DIAGRAMS * MAX_SPEC_BYTES + MAX_MAP_BYTES) as u64 + 64 * 1024;

fn version_file(paths: &Paths, pr: &PrRef, n: u32) -> std::path::PathBuf {
    paths.diagrams_for(pr).join(format!("v{n}.json"))
}

/// The number in a file named `v<n>.json`, and nothing else.
fn exact_number(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('v')?.strip_suffix(".json")?;
    (!digits.is_empty() && !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// The number in a file named `v<n>.json` or `v<n>.json.<anything>` (a copy set aside).
fn any_number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('v')?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, tail) = rest.split_at(end);
    let named = tail == ".json" || tail.starts_with(".json.");
    // Digits past `u32::MAX` still name a version: they take the last number.
    (named && !digits.is_empty()).then(|| digits.parse().unwrap_or(u32::MAX))
}

/// Writes `version` as `v<number>.json`, replacing the file of the same number.
pub fn save_diagram_version(paths: &Paths, pr: &PrRef, version: &DiagramVersion) -> io::Result<()> {
    let json = serde_json::to_vec(version).map_err(io::Error::other)?;
    write_atomic(&version_file(paths, pr, version.number), &json)
}

fn read_version(path: &Path, n: u32) -> Option<DiagramVersion> {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_VERSION_BYTES {
        tracing::warn!(file = %path.display(), size, "a diagram version is too big to be one and was skipped");
        return None;
    }
    let parsed = fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| {
        serde_json::from_slice::<DiagramVersion>(&bytes).map_err(|e| e.to_string())
    });
    match parsed {
        Ok(v) if v.number == n => Some(v),
        Ok(v) => {
            tracing::warn!(file = %path.display(), number = v.number, "a diagram version names another number and was skipped");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, file = %path.display(), "a diagram version was unreadable and was skipped");
            None
        }
    }
}

/// Every readable version of `pr`, by number. A file that cannot be read is skipped and left
/// where it is: it is never renamed or deleted.
pub fn load_diagram_versions(paths: &Paths, pr: &PrRef) -> Vec<DiagramVersion> {
    let Ok(entries) = fs::read_dir(paths.diagrams_for(pr)) else {
        return Vec::new();
    };
    let mut versions: Vec<DiagramVersion> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let n = exact_number(&e.file_name().to_string_lossy())?;
            read_version(&e.path(), n)
        })
        .collect();
    versions.sort_by_key(|v| v.number);
    versions
}

/// The highest version of `pr` that can be read, reading no file below it.
pub fn load_latest_diagram_version(paths: &Paths, pr: &PrRef) -> Option<DiagramVersion> {
    let mut numbered: Vec<(u32, std::path::PathBuf)> = fs::read_dir(paths.diagrams_for(pr))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| Some((exact_number(&e.file_name().to_string_lossy())?, e.path())))
        .collect();
    numbered.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    numbered
        .into_iter()
        .find_map(|(n, path)| read_version(&path, n))
}

/// Version `n` of `pr`, if it exists and can be read.
pub fn load_diagram_version(paths: &Paths, pr: &PrRef, n: u32) -> Option<DiagramVersion> {
    let path = version_file(paths, pr, n);
    path.exists().then(|| read_version(&path, n)).flatten()
}

/// The number the next version takes: one more than the highest `v<n>.json` file, whether or
/// not it can be read, so an unreadable file is never overwritten. `None` when the highest is
/// already `u32::MAX`: there is no number left.
pub fn next_diagram_number(paths: &Paths, pr: &PrRef) -> Option<u32> {
    let highest = fs::read_dir(paths.diagrams_for(pr))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| any_number(&e.file_name().to_string_lossy()))
        .max()
        .unwrap_or(0);
    highest.checked_add(1)
}

/// Removes every version of `pr`; there being none is fine.
pub fn delete_diagrams(paths: &Paths, pr: &PrRef) -> io::Result<()> {
    match fs::remove_dir_all(paths.diagrams_for(pr)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::diagram::{
        DiagramEntry, DiagramKind, DiagramSpec, EntryState, Node, PlanEntry, Shape, VersionPhase,
        keywords_entry,
    };
    use clusia_core::keywords::KeywordMap;

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn version(number: u32) -> DiagramVersion {
        let mut spec = DiagramSpec::new("flow", DiagramKind::Flow, "Token refresh flow");
        spec.nodes = vec![Node::new("a", "Client", Shape::Box)];
        DiagramVersion {
            number,
            commit: "3f2a1c9d".repeat(5),
            harness: "Claude Code".into(),
            drawn_at: 1_700_000_000 + i64::from(number),
            phase: VersionPhase::Done,
            entries: vec![DiagramEntry {
                plan: PlanEntry {
                    key: "flow".into(),
                    kind: DiagramKind::Flow,
                    title: "Token refresh flow".into(),
                    purpose: "How a token is replaced.".into(),
                },
                state: EntryState::Ready {
                    spec,
                    notes: Vec::new(),
                },
            }],
        }
    }

    fn setup() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        (dir, paths)
    }

    #[test]
    fn no_folder_is_no_versions_and_nothing_is_written() {
        let (_d, p) = setup();
        assert!(load_diagram_versions(&p, &pr()).is_empty());
        assert!(load_diagram_version(&p, &pr(), 1).is_none());
        assert_eq!(next_diagram_number(&p, &pr()), Some(1));
        assert!(!p.diagrams_dir().exists());
        delete_diagrams(&p, &pr()).unwrap();
    }

    #[test]
    fn versions_round_trip_in_number_order() {
        let (_d, p) = setup();
        for n in [10, 2, 1] {
            save_diagram_version(&p, &pr(), &version(n)).unwrap();
        }
        let numbers: Vec<u32> = load_diagram_versions(&p, &pr())
            .iter()
            .map(|v| v.number)
            .collect();
        assert_eq!(numbers, [1, 2, 10]);
        assert_eq!(load_diagram_version(&p, &pr(), 2), Some(version(2)));
        assert!(load_diagram_version(&p, &pr(), 3).is_none());
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        assert!(load_diagram_versions(&p, &other).is_empty());
    }

    #[test]
    fn saving_a_number_again_replaces_it() {
        let (_d, p) = setup();
        save_diagram_version(&p, &pr(), &version(1)).unwrap();
        let mut again = version(1);
        again.phase = VersionPhase::Drawing;
        save_diagram_version(&p, &pr(), &again).unwrap();
        let loaded = load_diagram_versions(&p, &pr());
        assert_eq!(loaded, vec![again]);
        let names: Vec<_> = fs::read_dir(p.diagrams_for(&pr()))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("v1.json")]);
    }

    #[test]
    fn an_unreadable_file_is_skipped_and_left_where_it_is() {
        let (_d, p) = setup();
        save_diagram_version(&p, &pr(), &version(1)).unwrap();
        save_diagram_version(&p, &pr(), &version(3)).unwrap();
        let bad = p.diagrams_for(&pr()).join("v2.json");
        fs::write(&bad, "{ not json").unwrap();
        let numbers: Vec<u32> = load_diagram_versions(&p, &pr())
            .iter()
            .map(|v| v.number)
            .collect();
        assert_eq!(numbers, [1, 3]);
        assert_eq!(fs::read_to_string(&bad).unwrap(), "{ not json");
        assert!(load_diagram_version(&p, &pr(), 2).is_none());
        assert_eq!(next_diagram_number(&p, &pr()), Some(4));
    }

    #[test]
    fn the_latest_is_the_highest_version_that_reads() {
        let (_d, p) = setup();
        assert!(load_latest_diagram_version(&p, &pr()).is_none());
        for n in [1, 2, 3] {
            save_diagram_version(&p, &pr(), &version(n)).unwrap();
        }
        assert_eq!(load_latest_diagram_version(&p, &pr()), Some(version(3)));
        fs::write(p.diagrams_for(&pr()).join("v3.json"), "{ not json").unwrap();
        assert_eq!(load_latest_diagram_version(&p, &pr()), Some(version(2)));
    }

    #[test]
    fn a_file_whose_content_names_another_number_is_skipped() {
        let (_d, p) = setup();
        save_diagram_version(&p, &pr(), &version(1)).unwrap();
        let copy = p.diagrams_for(&pr()).join("v5.json");
        fs::copy(p.diagrams_for(&pr()).join("v1.json"), &copy).unwrap();
        let numbers: Vec<u32> = load_diagram_versions(&p, &pr())
            .iter()
            .map(|v| v.number)
            .collect();
        assert_eq!(numbers, [1]);
        assert!(load_diagram_version(&p, &pr(), 5).is_none());
    }

    #[test]
    fn only_files_named_v_number_json_are_versions() {
        let (_d, p) = setup();
        save_diagram_version(&p, &pr(), &version(1)).unwrap();
        let dir = p.diagrams_for(&pr());
        for name in [
            "v2.json.bak",
            "v.json",
            "vx.json",
            "v02x.json",
            "v02.json",
            "notes.txt",
            "V3.json",
        ] {
            fs::copy(dir.join("v1.json"), dir.join(name)).unwrap();
        }
        let numbers: Vec<u32> = load_diagram_versions(&p, &pr())
            .iter()
            .map(|v| v.number)
            .collect();
        assert_eq!(numbers, [1]);
    }

    #[test]
    fn the_next_number_counts_every_file_of_the_family() {
        let (_d, p) = setup();
        let dir = p.diagrams_for(&pr());
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(next_diagram_number(&p, &pr()), Some(1));
        for (name, next) in [
            ("v1.json", Some(2)),
            ("v7.json.corrupt-1700000000", Some(8)),
            ("v3.json", Some(8)),
            ("v4294967294.json", Some(u32::MAX)),
            ("v4294967295.json", None),
        ] {
            fs::write(dir.join(name), "x").unwrap();
            assert_eq!(next_diagram_number(&p, &pr()), next, "{name}");
        }
        fs::write(dir.join("v8x.json"), "x").unwrap();
        fs::write(dir.join("v9.jsonx"), "x").unwrap();
        fs::write(dir.join("w10.json"), "x").unwrap();
        assert_eq!(next_diagram_number(&p, &pr()), None);
    }

    #[test]
    fn a_number_past_the_last_one_leaves_no_number_to_take() {
        let (_d, p) = setup();
        let dir = p.diagrams_for(&pr());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("v99999999999.json"), "x").unwrap();
        assert_eq!(next_diagram_number(&p, &pr()), None);
    }

    #[test]
    fn a_file_bigger_than_any_version_is_not_read() {
        let (_d, p) = setup();
        save_diagram_version(&p, &pr(), &version(1)).unwrap();
        let file = p.diagrams_for(&pr()).join("v1.json");
        let mut padded = fs::read(&file).unwrap();
        padded.resize(MAX_VERSION_BYTES as usize + 1, b' ');
        fs::write(&file, &padded).unwrap();
        assert!(load_diagram_version(&p, &pr(), 1).is_none());
        assert!(load_diagram_versions(&p, &pr()).is_empty());
        assert_eq!(
            fs::read(&file).unwrap().len(),
            padded.len(),
            "left where it is"
        );
    }

    #[test]
    fn delete_removes_one_review_and_is_idempotent() {
        let (_d, p) = setup();
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        save_diagram_version(&p, &pr(), &version(1)).unwrap();
        save_diagram_version(&p, &other, &version(1)).unwrap();
        delete_diagrams(&p, &pr()).unwrap();
        assert!(!p.diagrams_for(&pr()).exists());
        assert!(p.diagrams_for(&other).exists());
        delete_diagrams(&p, &pr()).unwrap();
    }

    #[test]
    fn a_version_with_a_big_keyword_map_is_still_read() {
        let (_d, p) = setup();
        let places: Vec<_> = (0..130_000u32)
            .map(|i| serde_json::json!([0, i, 0, 65535, 0]))
            .collect();
        let map: KeywordMap = serde_json::from_value(serde_json::json!({
            "plan": {
                "title": "T", "caption": "",
                "axes": [
                    {"name": "A", "low": "l", "high": "h"},
                    {"name": "B", "low": "l", "high": "h"},
                    {"name": "C", "low": "l", "high": "h"}
                ],
                "families": [{"id": "f", "label": "F", "seed": {"word": "retry", "path": "a.rs", "line": 1}, "words": ["retry"]}],
                "use_cases": [{"id": "u", "name": "U"}]
            },
            "regions": [], "paths": ["a.rs"], "words": [{"text": "retry", "family": 0}],
            "places": places, "overflow": [0]
        }))
        .unwrap();
        let mut v = version(1);
        v.entries.push(DiagramEntry {
            plan: keywords_entry(),
            state: EntryState::ReadyMap {
                map: Box::new(map),
                notes: vec![],
            },
        });
        save_diagram_version(&p, &pr(), &v).unwrap();
        let size = fs::metadata(version_file(&p, &pr(), 1)).unwrap().len();
        assert!(
            size > (MAX_DIAGRAMS * MAX_SPEC_BYTES) as u64 + 64 * 1024,
            "{size}"
        );
        assert!(size < MAX_VERSION_BYTES, "{size}");
        assert_eq!(load_diagram_version(&p, &pr(), 1), Some(v));
    }
}
