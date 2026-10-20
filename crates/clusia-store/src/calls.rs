//! `calls/<owner~repo~n>/<sha>.json`: the call index of a review's head commit, so a reopened
//! review does not parse the repository again. Only the newest commit's file is kept.

use std::fs;
use std::io;

use clusia_calls::CallIndex;
use clusia_core::{Paths, PrRef};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

/// The biggest index file that is read.
const MAX_INDEX_BYTES: u64 = 96 * 1024 * 1024;

/// Whether `sha` can name a file: 7 to 64 lowercase hex digits.
fn is_sha(sha: &str) -> bool {
    (7..=64).contains(&sha.len()) && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Writes `index` as `<commit>.json` and removes the folder's other `.json` files. A set-aside
/// copy (`<sha>.json.corrupt-<ts>`) is not one of them and stays.
pub fn save_index(paths: &Paths, pr: &PrRef, index: &CallIndex) -> io::Result<()> {
    if !is_sha(&index.commit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the commit is not a hex id",
        ));
    }
    let dir = paths.calls_for(pr);
    let name = format!("{}.json", index.commit);
    let json = serde_json::to_vec(index).map_err(io::Error::other)?;
    write_atomic(&dir.join(&name), &json)?;
    for entry in fs::read_dir(&dir)?.filter_map(Result::ok) {
        let other = entry.file_name().to_string_lossy().into_owned();
        if other != name
            && other.ends_with(".json")
            && let Err(e) = fs::remove_file(entry.path())
        {
            tracing::warn!(error = %e, file = %entry.path().display(), "an older call index could not be removed");
        }
    }
    Ok(())
}

/// The index of `sha` for `pr`. A file that is not a valid index of that commit, or is too big,
/// is set aside and gives `None`, so the index is built again.
pub fn load_index(paths: &Paths, pr: &PrRef, sha: &str) -> Option<CallIndex> {
    if !is_sha(sha) {
        return None;
    }
    let path = paths.calls_for(pr).join(format!("{sha}.json"));
    let meta = fs::metadata(&path).ok()?;
    let index = if meta.len() > MAX_INDEX_BYTES {
        None
    } else {
        // A file that cannot be read at all is left alone: the fault is not in its content.
        let bytes = fs::read(&path).ok()?;
        serde_json::from_slice::<CallIndex>(&bytes)
            .ok()
            .filter(|index| index.commit == sha)
    };
    if index.is_none()
        && let Err(e) = quarantine(&path, now_unix())
    {
        tracing::warn!(error = %e, file = %path.display(), "a call index could not be set aside");
    }
    index
}

/// Removes the index of `pr`; there being none is fine.
pub fn delete_calls(paths: &Paths, pr: &PrRef) -> io::Result<()> {
    match fs::remove_dir_all(paths.calls_for(pr)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_calls::{Call, Function, FunctionId, Import, Skipped};

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn index(commit: &str) -> CallIndex {
        let id = FunctionId("src/store.rs#TokenStore::refresh#12".into());
        CallIndex {
            commit: commit.into(),
            functions: vec![Function {
                id: id.clone(),
                qualified: "TokenStore::refresh".into(),
                name: "refresh".into(),
                path: "src/store.rs".into(),
                start_line: 12,
                end_line: 30,
                signature_lines: 1,
                lang: "rust".into(),
            }],
            calls: vec![Call {
                from: id,
                name: "sleep".into(),
                qualifier: Some("tokio".into()),
                receiver: false,
                line: 20,
            }],
            imports: vec![Import {
                path: "src/store.rs".into(),
                module: "tokio".into(),
                names: vec!["sleep".into()],
            }],
            skipped: vec![Skipped {
                path: "big.js".into(),
                reason: "over 512 KiB".into(),
            }],
            unsupported: vec!["main.go".into()],
            dependencies: vec!["tokio".into()],
            cut: false,
        }
    }

    const A: &str = "3f2a1c9d3f2a1c9d3f2a1c9d3f2a1c9d3f2a1c9d";
    const B: &str = "9be1d07a9be1d07a9be1d07a9be1d07a9be1d07a";

    fn setup() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        (dir, paths)
    }

    fn names(p: &Paths) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(p.calls_for(&pr()))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn nothing_saved_is_nothing_loaded_and_no_folder() {
        let (_d, p) = setup();
        assert!(load_index(&p, &pr(), A).is_none());
        assert!(!p.calls_dir().exists());
        delete_calls(&p, &pr()).unwrap();
    }

    #[test]
    fn an_index_round_trips_under_its_commit() {
        let (_d, p) = setup();
        save_index(&p, &pr(), &index(A)).unwrap();
        assert_eq!(names(&p), [format!("{A}.json")]);
        assert_eq!(load_index(&p, &pr(), A), Some(index(A)));
        assert!(load_index(&p, &pr(), B).is_none());
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        assert!(load_index(&p, &other, A).is_none());
    }

    #[test]
    fn saving_another_commit_removes_the_others_but_not_set_aside_files() {
        let (_d, p) = setup();
        save_index(&p, &pr(), &index(A)).unwrap();
        fs::write(p.calls_for(&pr()).join(format!("{A}.json.corrupt-1")), "x").unwrap();
        save_index(&p, &pr(), &index(B)).unwrap();
        assert_eq!(
            names(&p),
            [format!("{A}.json.corrupt-1"), format!("{B}.json")]
        );
        save_index(&p, &pr(), &index(B)).unwrap();
        assert_eq!(
            load_index(&p, &pr(), B),
            Some(index(B)),
            "saving the same commit again is fine"
        );
    }

    #[test]
    fn an_unreadable_index_is_quarantined_and_rebuilt_later() {
        let (_d, p) = setup();
        save_index(&p, &pr(), &index(A)).unwrap();
        let file = p.calls_for(&pr()).join(format!("{A}.json"));
        fs::write(&file, "{ not json").unwrap();
        assert!(load_index(&p, &pr(), A).is_none());
        assert!(!file.exists());
        let kept = names(&p);
        assert_eq!(kept.len(), 1);
        assert!(
            kept[0].starts_with(&format!("{A}.json.corrupt-")),
            "{kept:?}"
        );
        assert!(load_index(&p, &pr(), A).is_none(), "nothing left to read");
        save_index(&p, &pr(), &index(A)).unwrap();
        assert_eq!(load_index(&p, &pr(), A), Some(index(A)));
    }

    #[test]
    fn an_index_that_names_another_commit_is_quarantined() {
        let (_d, p) = setup();
        save_index(&p, &pr(), &index(A)).unwrap();
        let dir = p.calls_for(&pr());
        fs::rename(dir.join(format!("{A}.json")), dir.join(format!("{B}.json"))).unwrap();
        assert!(load_index(&p, &pr(), B).is_none());
        assert!(names(&p)[0].starts_with(&format!("{B}.json.corrupt-")));
    }

    #[test]
    fn a_commit_that_is_not_hex_never_names_a_file() {
        let (_d, p) = setup();
        for bad in [
            "../etc/passwd",
            "abc/def",
            "ABCDEF1",
            "abc",
            "",
            &"a".repeat(65),
            "g".repeat(10).as_str(),
        ] {
            let err = save_index(&p, &pr(), &index(bad)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
            assert!(load_index(&p, &pr(), bad).is_none(), "{bad:?}");
        }
        assert!(!p.calls_dir().exists(), "nothing was written");
    }

    #[test]
    fn deleting_removes_the_folder() {
        let (_d, p) = setup();
        save_index(&p, &pr(), &index(A)).unwrap();
        delete_calls(&p, &pr()).unwrap();
        assert!(!p.calls_for(&pr()).exists());
        delete_calls(&p, &pr()).unwrap();
    }
}
