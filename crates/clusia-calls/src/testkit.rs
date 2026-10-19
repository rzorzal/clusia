//! Helpers for the tests: an index from file texts and its rows in a comparable shape.

use std::time::{Duration, Instant};

use clusia_core::hunks::{FileHunks, FileStatus, Hunk, hunk_ref};

use crate::{CallIndex, Indexer, SourceFile};

pub(crate) fn far() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

pub(crate) fn index_of(files: &[(&str, &str)]) -> CallIndex {
    index_with(files, &[])
}

pub(crate) fn index_with(files: &[(&str, &str)], dependencies: &[&str]) -> CallIndex {
    let deps = dependencies.iter().map(|d| d.to_string()).collect();
    let mut indexer = Indexer::new("c0ffee", deps, far());
    for (path, text) in files {
        assert!(indexer.add(SourceFile { path, text }), "{path} was refused");
    }
    indexer.finish()
}

/// `(qualified, start line, end line, signature lines)` per function, in index order.
pub(crate) fn shapes(index: &CallIndex) -> Vec<(String, u32, u32, u32)> {
    index
        .functions
        .iter()
        .map(|f| {
            (
                f.qualified.clone(),
                f.start_line,
                f.end_line,
                f.signature_lines,
            )
        })
        .collect()
}

pub(crate) type CallRow = (String, u32, String, Option<String>, bool);

/// `(caller's qualified name, line, callee name, qualifier, receiver)` per call, in index order.
pub(crate) fn call_rows(index: &CallIndex) -> Vec<CallRow> {
    index
        .calls
        .iter()
        .map(|c| {
            let from = index
                .functions
                .iter()
                .find(|f| f.id == c.from)
                .expect("caller");
            (
                from.qualified.clone(),
                c.line,
                c.name.clone(),
                c.qualifier.clone(),
                c.receiver,
            )
        })
        .collect()
}

/// `(module, names)` per import, in index order.
pub(crate) fn import_rows(index: &CallIndex) -> Vec<(String, Vec<String>)> {
    index
        .imports
        .iter()
        .map(|i| (i.module.clone(), i.names.clone()))
        .collect()
}

pub(crate) fn rows(spec: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    spec.iter()
        .map(|(m, n)| (m.to_string(), n.iter().map(|s| s.to_string()).collect()))
        .collect()
}

/// `files` Rust files of `per_file` functions each; every function calls the next one of its
/// file and the same-numbered one of the next file, and every name is unique.
pub(crate) fn generated_repo(files: usize, per_file: usize) -> Vec<(String, String)> {
    (0..files)
        .map(|i| {
            let mut text = String::new();
            for j in 0..per_file {
                text.push_str(&format!(
                    "pub fn f{i}_{j}() {{\n    f{i}_{}();\n    f{}_{j}();\n}}\n",
                    (j + 1) % per_file,
                    (i + 1) % files
                ));
            }
            (format!("src/m{i}.rs"), text)
        })
        .collect()
}

/// A file's hunks from `(old_start, old_len, new_start, new_len)` tuples.
pub(crate) fn file(
    old: Option<&str>,
    new: Option<&str>,
    status: FileStatus,
    hunks: &[(u32, u32, u32, u32)],
) -> FileHunks {
    FileHunks {
        old_path: old.map(str::to_string),
        new_path: new.map(str::to_string),
        status,
        hunks: hunks
            .iter()
            .map(|&(old_start, old_len, new_start, new_len)| Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
            })
            .collect(),
    }
}

/// A text lookup over `(path, text)` pairs.
pub(crate) fn texts(files: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> = files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect();
    move |path| map.get(path).cloned()
}

/// Panics unless every changed head line of `files` lies in a hunk of some step.
pub(crate) fn assert_covers(steps: &[crate::StaticStep], files: &[FileHunks]) {
    for f in files {
        for hunk in &f.hunks {
            let Some(r) = hunk_ref(f, hunk) else { continue };
            for line in r.start_line..=r.end_line {
                assert!(
                    steps
                        .iter()
                        .any(|s| s.hunks.iter().any(|x| x.path == r.path
                            && x.start_line <= line
                            && line <= x.end_line)),
                    "{}:{line} is in no step",
                    r.path
                );
            }
        }
    }
}
