//! `stories/<owner~repo~n>/v<n>.json` and `read.json`: the Stories of a review, one file per
//! version, and the hunks the reviewer has read, so a reopened review shows them without asking
//! the agent again.

use std::fs;
use std::io;
use std::path::Path;

use clusia_core::story::{MAX_READ_MARKS, MAX_STORY_BYTES, ReadMarks, StoryVersion};
use clusia_core::{Paths, PrRef};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

/// The biggest file a version can be: a spec at its cap, the notes, and room for the rest.
const MAX_VERSION_BYTES: u64 = (MAX_STORY_BYTES * 2) as u64 + 64 * 1024;
/// The biggest `read.json` that is read.
const MAX_MARKS_BYTES: u64 = 4 * 1024 * 1024;

fn version_file(paths: &Paths, pr: &PrRef, n: u32) -> std::path::PathBuf {
    paths.stories_for(pr).join(format!("v{n}.json"))
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
pub fn save_story_version(paths: &Paths, pr: &PrRef, version: &StoryVersion) -> io::Result<()> {
    let json = serde_json::to_vec(version).map_err(io::Error::other)?;
    write_atomic(&version_file(paths, pr, version.number), &json)
}

fn read_version(path: &Path, n: u32) -> Option<StoryVersion> {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_VERSION_BYTES {
        tracing::warn!(file = %path.display(), size, "a story version is too big to be one and was skipped");
        return None;
    }
    let parsed = fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| {
        serde_json::from_slice::<StoryVersion>(&bytes).map_err(|e| e.to_string())
    });
    match parsed {
        Ok(v) if v.number == n => Some(v),
        Ok(v) => {
            tracing::warn!(file = %path.display(), number = v.number, "a story version names another number and was skipped");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, file = %path.display(), "a story version was unreadable and was skipped");
            None
        }
    }
}

/// Every readable version of `pr`, by number. A file that cannot be read is skipped and left
/// where it is: it is never renamed or deleted.
pub fn load_story_versions(paths: &Paths, pr: &PrRef) -> Vec<StoryVersion> {
    let Ok(entries) = fs::read_dir(paths.stories_for(pr)) else {
        return Vec::new();
    };
    let mut versions: Vec<StoryVersion> = entries
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
pub fn load_latest_story_version(paths: &Paths, pr: &PrRef) -> Option<StoryVersion> {
    let mut numbered: Vec<(u32, std::path::PathBuf)> = fs::read_dir(paths.stories_for(pr))
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
pub fn load_story_version(paths: &Paths, pr: &PrRef, n: u32) -> Option<StoryVersion> {
    let path = version_file(paths, pr, n);
    path.exists().then(|| read_version(&path, n)).flatten()
}

/// The number the next version takes: one more than the highest `v<n>.json` file, whether or
/// not it can be read, so an unreadable file is never overwritten. `None` when the highest is
/// already `u32::MAX`: there is no number left.
pub fn next_story_number(paths: &Paths, pr: &PrRef) -> Option<u32> {
    let highest = fs::read_dir(paths.stories_for(pr))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| any_number(&e.file_name().to_string_lossy()))
        .max()
        .unwrap_or(0);
    highest.checked_add(1)
}

/// The hunks `pr`'s reviewer has read. A file that is missing gives none; one that is not valid
/// or is too big is set aside and gives none. Past `MAX_READ_MARKS` only the last marks are kept.
pub fn load_read_marks(paths: &Paths, pr: &PrRef) -> ReadMarks {
    let path = paths.stories_for(pr).join("read.json");
    let Ok(meta) = fs::metadata(&path) else {
        return ReadMarks::default();
    };
    let parsed = if meta.len() > MAX_MARKS_BYTES {
        None
    } else {
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ReadMarks>(&bytes).ok())
    };
    match parsed {
        Some(mut marks) => {
            let over = marks.hunks.len().saturating_sub(MAX_READ_MARKS);
            marks.hunks.drain(..over);
            marks
        }
        None => {
            if let Err(e) = quarantine(&path, now_unix()) {
                tracing::warn!(error = %e, file = %path.display(), "read marks could not be set aside");
            }
            ReadMarks::default()
        }
    }
}

/// Writes the hunks `pr`'s reviewer has read.
pub fn save_read_marks(paths: &Paths, pr: &PrRef, marks: &ReadMarks) -> io::Result<()> {
    let json = serde_json::to_vec(marks).map_err(io::Error::other)?;
    write_atomic(&paths.stories_for(pr).join("read.json"), &json)
}

/// Removes every version and the read marks of `pr`; there being none is fine.
pub fn delete_stories(paths: &Paths, pr: &PrRef) -> io::Result<()> {
    match fs::remove_dir_all(paths.stories_for(pr)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::story::{StorySpec, StoryVersionState};

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn spec() -> StorySpec {
        serde_json::from_value(serde_json::json!({
            "sentence": "Tokens refresh once.",
            "steps": [{"id": "a", "title": "The lock", "hunks": [{"path": "src/a.rs", "start_line": 1, "end_line": 3}]}]
        }))
        .unwrap()
    }

    fn version(number: u32) -> StoryVersion {
        StoryVersion {
            number,
            commit: "3f2a1c9d".repeat(5),
            harness: "Claude Code".into(),
            started_at: format!("2026-10-19T12:00:{number:02}Z"),
            state: StoryVersionState::Written,
            spec: Some(spec()),
            notes: vec!["one note".into()],
        }
    }

    fn setup() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        (dir, paths)
    }

    fn numbers(p: &Paths) -> Vec<u32> {
        load_story_versions(p, &pr())
            .iter()
            .map(|v| v.number)
            .collect()
    }

    #[test]
    fn no_folder_is_no_versions_and_nothing_is_written() {
        let (_d, p) = setup();
        assert!(load_story_versions(&p, &pr()).is_empty());
        assert!(load_story_version(&p, &pr(), 1).is_none());
        assert!(load_latest_story_version(&p, &pr()).is_none());
        assert_eq!(next_story_number(&p, &pr()), Some(1));
        assert_eq!(load_read_marks(&p, &pr()), ReadMarks::default());
        assert!(!p.stories_dir().exists());
        delete_stories(&p, &pr()).unwrap();
    }

    #[test]
    fn versions_round_trip_in_number_order_and_per_pull_request() {
        let (_d, p) = setup();
        for n in [10, 2, 1] {
            save_story_version(&p, &pr(), &version(n)).unwrap();
        }
        assert_eq!(numbers(&p), [1, 2, 10]);
        assert_eq!(load_story_version(&p, &pr(), 2), Some(version(2)));
        assert!(load_story_version(&p, &pr(), 3).is_none());
        assert_eq!(load_latest_story_version(&p, &pr()), Some(version(10)));
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        assert!(load_story_versions(&p, &other).is_empty());
    }

    #[test]
    fn saving_a_number_again_replaces_it() {
        let (_d, p) = setup();
        save_story_version(&p, &pr(), &version(1)).unwrap();
        let mut again = version(1);
        again.state = StoryVersionState::NotWritten;
        save_story_version(&p, &pr(), &again).unwrap();
        assert_eq!(load_story_versions(&p, &pr()), vec![again]);
        let names: Vec<_> = fs::read_dir(p.stories_for(&pr()))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("v1.json")]);
    }

    #[test]
    fn an_unreadable_file_is_skipped_and_left_where_it_is_and_its_number_is_not_reused() {
        let (_d, p) = setup();
        save_story_version(&p, &pr(), &version(1)).unwrap();
        save_story_version(&p, &pr(), &version(3)).unwrap();
        let bad = p.stories_for(&pr()).join("v2.json");
        fs::write(&bad, "{ not json").unwrap();
        assert_eq!(numbers(&p), [1, 3]);
        assert_eq!(fs::read_to_string(&bad).unwrap(), "{ not json");
        assert!(load_story_version(&p, &pr(), 2).is_none());
        assert_eq!(next_story_number(&p, &pr()), Some(4));
        fs::write(p.stories_for(&pr()).join("v3.json"), "{ not json").unwrap();
        assert_eq!(
            load_latest_story_version(&p, &pr()),
            Some(version(1)),
            "the highest that reads"
        );
    }

    #[test]
    fn a_set_aside_copy_still_counts_for_the_next_number() {
        let (_d, p) = setup();
        save_story_version(&p, &pr(), &version(1)).unwrap();
        fs::write(p.stories_for(&pr()).join("v5.json.corrupt-1"), "x").unwrap();
        assert_eq!(next_story_number(&p, &pr()), Some(6));
        fs::write(p.stories_for(&pr()).join("v4294967295.json"), "x").unwrap();
        assert_eq!(next_story_number(&p, &pr()), None, "no number is left");
    }

    #[test]
    fn a_file_that_names_another_number_or_is_too_big_is_skipped() {
        let (_d, p) = setup();
        save_story_version(&p, &pr(), &version(1)).unwrap();
        let dir = p.stories_for(&pr());
        fs::copy(dir.join("v1.json"), dir.join("v5.json")).unwrap();
        let big = fs::File::create(dir.join("v6.json")).unwrap();
        big.set_len(MAX_VERSION_BYTES + 1).unwrap();
        assert_eq!(numbers(&p), [1]);
        assert_eq!(next_story_number(&p, &pr()), Some(7));
    }

    fn marks() -> ReadMarks {
        ReadMarks {
            hunks: vec![clusia_core::story::ReadHunk {
                commit: "a".repeat(40),
                path: "src/a.rs".into(),
                start_line: 1,
                end_line: 3,
            }],
        }
    }

    #[test]
    fn read_marks_round_trip_and_do_not_count_as_a_version() {
        let (_d, p) = setup();
        save_read_marks(&p, &pr(), &marks()).unwrap();
        assert_eq!(load_read_marks(&p, &pr()), marks());
        assert!(load_story_versions(&p, &pr()).is_empty());
        assert_eq!(next_story_number(&p, &pr()), Some(1));
        save_story_version(&p, &pr(), &version(1)).unwrap();
        assert_eq!(
            load_read_marks(&p, &pr()),
            marks(),
            "saving a version leaves the marks"
        );
    }

    #[test]
    fn unreadable_read_marks_are_quarantined_and_the_default_is_used() {
        let (_d, p) = setup();
        let file = p.stories_for(&pr()).join("read.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{ not json").unwrap();
        assert_eq!(load_read_marks(&p, &pr()), ReadMarks::default());
        assert!(!file.exists());
        let set_aside: Vec<_> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(set_aside.len(), 1);
        assert!(
            set_aside[0].starts_with("read.json.corrupt-"),
            "{set_aside:?}"
        );
    }

    #[test]
    fn read_marks_past_the_cap_keep_the_last_ones() {
        let (_d, p) = setup();
        let many = ReadMarks {
            hunks: (0..MAX_READ_MARKS as u32 + 5)
                .map(|i| clusia_core::story::ReadHunk {
                    commit: "a".repeat(40),
                    path: "src/a.rs".into(),
                    start_line: i + 1,
                    end_line: i + 1,
                })
                .collect(),
        };
        save_read_marks(&p, &pr(), &many).unwrap();
        let loaded = load_read_marks(&p, &pr());
        assert_eq!(loaded.hunks.len(), MAX_READ_MARKS);
        assert_eq!(loaded.hunks[0].start_line, 6);
    }

    #[test]
    fn deleting_removes_versions_and_marks() {
        let (_d, p) = setup();
        save_story_version(&p, &pr(), &version(1)).unwrap();
        save_read_marks(&p, &pr(), &marks()).unwrap();
        delete_stories(&p, &pr()).unwrap();
        assert!(!p.stories_for(&pr()).exists());
        assert_eq!(load_read_marks(&p, &pr()), ReadMarks::default());
        delete_stories(&p, &pr()).unwrap();
    }
}
