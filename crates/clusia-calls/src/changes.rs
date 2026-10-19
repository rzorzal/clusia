//! Which functions a diff changed, by comparing the head index with the base index of the
//! changed files: a changed body, a changed signature, a new function, a removed one, or one
//! moved to another place.

use std::collections::{HashMap, HashSet};

use clusia_core::hunks::{FileHunks, Hunk};
use serde::{Deserialize, Serialize};

use crate::index::{CallIndex, Function, FunctionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Changed,
    Signature,
    New,
    Removed,
    Moved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFunction {
    /// The head id; the base id for a removed function.
    pub id: FunctionId,
    pub kind: ChangeKind,
    pub added: u32,
    pub removed: u32,
    /// Where a moved function was at base: its path and start line.
    pub moved_from: Option<(String, u32)>,
}

/// The signature lines of `function` in `text`, each trimmed, joined by one space.
pub fn signature_text(text: &str, function: &Function) -> String {
    text.lines()
        .skip((function.start_line as usize).saturating_sub(1))
        .take(function.signature_lines as usize)
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The trimmed lines of a function's body, from its text.
fn body_lines<'a>(text: &'a str, function: &Function) -> Vec<&'a str> {
    text.lines()
        .skip((function.start_line as usize).saturating_sub(1))
        .take((function.end_line - function.start_line + 1) as usize)
        .map(str::trim)
        .collect()
}

/// Whether two bodies are equal after trimming each line, or differ by at most a fifth of
/// the longer one's lines.
fn similar(head: &[&str], base: &[&str]) -> bool {
    if head == base {
        return true;
    }
    let (long, short) = if head.len() >= base.len() {
        (head, base)
    } else {
        (base, head)
    };
    let mut spare: HashMap<&str, usize> = HashMap::new();
    for line in short {
        *spare.entry(line).or_default() += 1;
    }
    let unmatched = long
        .iter()
        .filter(|line| match spare.get_mut(*line) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .count();
    unmatched * 5 <= long.len()
}

/// Lines of `[start, end]` that lie in `[a, b]`.
fn overlap(start: u32, end: u32, a: u32, b: u32) -> u32 {
    let (lo, hi) = (start.max(a), end.min(b));
    if lo <= hi { hi - lo + 1 } else { 0 }
}

fn touches(hunk: &Hunk, function: &Function) -> bool {
    if hunk.new_len > 0 {
        overlap(
            hunk.new_start,
            hunk.new_start + hunk.new_len - 1,
            function.start_line,
            function.end_line,
        ) > 0
    } else {
        function.start_line <= hunk.new_start && hunk.new_start < function.end_line
    }
}

/// Net lines the hunks wholly above `base` add before it.
fn shift(hunks: &[Hunk], base: &Function) -> i64 {
    hunks
        .iter()
        .filter(|h| {
            if h.old_len > 0 {
                h.old_start + h.old_len <= base.start_line
            } else {
                h.old_start < base.start_line
            }
        })
        .map(|h| i64::from(h.new_len) - i64::from(h.old_len))
        .sum()
}

fn added_lines(hunks: &[Hunk], head: &Function) -> u32 {
    hunks
        .iter()
        .filter(|h| h.new_len > 0)
        .map(|h| {
            overlap(
                h.new_start,
                h.new_start + h.new_len - 1,
                head.start_line,
                head.end_line,
            )
        })
        .sum()
}

fn removed_lines(files: &[&FileHunks], base: &Function) -> u32 {
    files
        .iter()
        .flat_map(|f| f.hunks.iter())
        .filter(|h| h.old_len > 0)
        .map(|h| {
            overlap(
                h.old_start,
                h.old_start + h.old_len - 1,
                base.start_line,
                base.end_line,
            )
        })
        .sum()
}

/// Classifies the functions `hunks` touch. `base` holds only the changed files at base and
/// `head` the whole repository; the texts come by path and may be missing, which reads as "not
/// similar" and skips the signature comparison. Sorted by id.
pub fn changed_functions(
    head: &CallIndex,
    base: &CallIndex,
    hunks: &[FileHunks],
    head_text: &dyn Fn(&str) -> Option<String>,
    base_text: &dyn Fn(&str) -> Option<String>,
) -> Vec<ChangedFunction> {
    let mut head_by_path: HashMap<&str, Vec<&Function>> = HashMap::new();
    for f in &head.functions {
        head_by_path.entry(f.path.as_str()).or_default().push(f);
    }
    let mut base_by_path: HashMap<&str, Vec<&Function>> = HashMap::new();
    let mut base_by_name: HashMap<&str, Vec<&Function>> = HashMap::new();
    for f in &base.functions {
        base_by_path.entry(f.path.as_str()).or_default().push(f);
        base_by_name
            .entry(f.qualified.as_str())
            .or_default()
            .push(f);
    }
    let head_keys: HashSet<(&str, &str)> = head
        .functions
        .iter()
        .map(|f| (f.path.as_str(), f.qualified.as_str()))
        .collect();
    let mut files_by_old: HashMap<&str, Vec<&FileHunks>> = HashMap::new();
    for f in hunks {
        if let Some(old) = &f.old_path {
            files_by_old.entry(old.as_str()).or_default().push(f);
        }
    }

    let mut out: Vec<ChangedFunction> = Vec::new();
    let mut taken: HashSet<&FunctionId> = HashSet::new();
    let mut moved_sources: HashSet<&FunctionId> = HashSet::new();
    let mut seen_files: HashSet<(&str, &str)> = HashSet::new();

    for f in hunks.iter().filter(|f| !f.hunks.is_empty()) {
        let head_path = f.new_path.as_deref();
        let base_path = f.old_path.as_deref();
        if let Some(head_path) = head_path {
            let mut heads: Vec<&Function> = head_by_path
                .get(head_path)
                .map(|v| v.to_vec())
                .unwrap_or_default();
            heads.sort_by(|a, b| a.id.cmp(&b.id));
            for h in heads {
                if !f.hunks.iter().any(|hunk| touches(hunk, h)) {
                    continue;
                }
                let in_path = base_path.and_then(|p| {
                    base_by_path
                        .get(p)?
                        .iter()
                        .find(|b| b.qualified == h.qualified)
                        .copied()
                });
                let cross = if in_path.is_none() {
                    base_by_name
                        .get(h.qualified.as_str())
                        .and_then(|candidates| {
                            candidates
                                .iter()
                                .find(|b| {
                                    !head_keys.contains(&(b.path.as_str(), b.qualified.as_str()))
                                        && !taken.contains(&b.id)
                                })
                                .copied()
                        })
                } else {
                    None
                };
                let head_body = head_text(&h.path);
                let head_lines = head_body.as_deref().map(|t| body_lines(t, h));
                let is_similar =
                    |b: &Function, base_body: &Option<String>| match (&head_lines, base_body) {
                        (Some(hl), Some(bt)) => similar(hl, &body_lines(bt, b)),
                        _ => false,
                    };
                let added = added_lines(&f.hunks, h);
                let (kind, counterpart) = match (in_path, cross) {
                    (Some(b), _) => {
                        let base_body = base_text(&b.path);
                        let displaced =
                            i64::from(h.start_line) != i64::from(b.start_line) + shift(&f.hunks, b);
                        if displaced && is_similar(b, &base_body) {
                            (ChangeKind::Moved, Some(b))
                        } else if let (Some(ht), Some(bt)) = (&head_body, &base_body)
                            && signature_text(ht, h) != signature_text(bt, b)
                        {
                            (ChangeKind::Signature, Some(b))
                        } else {
                            (ChangeKind::Changed, Some(b))
                        }
                    }
                    (None, Some(b)) => {
                        if is_similar(b, &base_text(&b.path)) {
                            (ChangeKind::Moved, Some(b))
                        } else {
                            (ChangeKind::New, None)
                        }
                    }
                    (None, None) => (ChangeKind::New, None),
                };
                let removed = counterpart
                    .map(|b| {
                        removed_lines(
                            files_by_old
                                .get(b.path.as_str())
                                .map_or(&[][..], |v| v.as_slice()),
                            b,
                        )
                    })
                    .unwrap_or(0);
                let moved_from = if kind == ChangeKind::Moved {
                    counterpart.map(|b| (b.path.clone(), b.start_line))
                } else {
                    None
                };
                if kind == ChangeKind::Moved
                    && let Some(b) = counterpart
                {
                    moved_sources.insert(&b.id);
                    if cross.is_some() {
                        taken.insert(&b.id);
                    }
                }
                out.push(ChangedFunction {
                    id: h.id.clone(),
                    kind,
                    added,
                    removed,
                    moved_from,
                });
            }
        }
        if let Some(old) = base_path
            && seen_files.insert((old, head_path.unwrap_or("")))
        {
            let files = files_by_old.get(old).map_or(&[][..], |v| v.as_slice());
            for b in base_by_path.get(old).into_iter().flatten() {
                let at_head =
                    head_path.is_some_and(|p| head_keys.contains(&(p, b.qualified.as_str())));
                if at_head {
                    continue;
                }
                out.push(ChangedFunction {
                    id: b.id.clone(),
                    kind: ChangeKind::Removed,
                    added: 0,
                    removed: removed_lines(files, b),
                    moved_from: None,
                });
            }
        }
    }

    // A base function that moved elsewhere is not also removed.
    out.retain(|c| c.kind != ChangeKind::Removed || !moved_sources.contains(&c.id));
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{file, index_of, texts};
    use clusia_core::hunks::FileHunks;
    use clusia_core::hunks::FileStatus::{Added, Deleted, Modified, Renamed};

    fn id(v: &str) -> FunctionId {
        FunctionId(v.to_string())
    }

    fn change(i: &str, kind: ChangeKind, added: u32, removed: u32) -> ChangedFunction {
        ChangedFunction {
            id: id(i),
            kind,
            added,
            removed,
            moved_from: None,
        }
    }

    const BASE: &str = "pub fn keep() {\n    one();\n}\n\npub fn edited(a: u32) {\n    one();\n    two();\n}\n\npub fn resig(a: u32) {\n    one();\n}\n\npub fn gone() {\n    one();\n}\n\npub fn one() {}\npub fn two() {}\n";
    const HEAD: &str = "pub fn keep() {\n    one();\n}\n\npub fn edited(a: u32) {\n    one();\n    three();\n    two();\n}\n\npub fn resig(a: u32, b: u32) {\n    one();\n}\n\npub fn added() {\n    one();\n}\n\npub fn one() {}\npub fn two() {}\n";

    fn run(
        base: &[(&str, &str)],
        head: &[(&str, &str)],
        hunks: &[FileHunks],
    ) -> Vec<ChangedFunction> {
        changed_functions(
            &index_of(head),
            &index_of(base),
            hunks,
            &texts(head),
            &texts(base),
        )
    }

    fn store_hunks() -> Vec<FileHunks> {
        vec![file(
            Some("src/store.rs"),
            Some("src/store.rs"),
            Modified,
            &[(6, 0, 7, 1), (10, 1, 11, 1), (14, 3, 15, 3)],
        )]
    }

    #[test]
    fn changed_signature_new_and_removed_with_their_line_counts() {
        let got = run(
            &[("src/store.rs", BASE)],
            &[("src/store.rs", HEAD)],
            &store_hunks(),
        );
        assert_eq!(
            got,
            vec![
                change("src/store.rs#added#15", ChangeKind::New, 3, 0),
                change("src/store.rs#edited#5", ChangeKind::Changed, 1, 0),
                change("src/store.rs#gone#14", ChangeKind::Removed, 0, 3),
                change("src/store.rs#resig#11", ChangeKind::Signature, 1, 1),
            ]
        );
    }

    #[test]
    fn without_any_text_a_signature_change_reads_as_changed() {
        let (head, base) = (
            index_of(&[("src/store.rs", HEAD)]),
            index_of(&[("src/store.rs", BASE)]),
        );
        let none = |_: &str| None;
        let got = changed_functions(&head, &base, &store_hunks(), &none, &none);
        let kinds: Vec<ChangeKind> = got.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ChangeKind::New,
                ChangeKind::Changed,
                ChangeKind::Removed,
                ChangeKind::Changed
            ]
        );
    }

    #[test]
    fn signature_text_joins_the_trimmed_signature_lines() {
        let index = index_of(&[(
            "a.rs",
            "pub fn f(\n    a: u32,\n    b: u32,\n) {\n    body();\n}\n",
        )]);
        assert_eq!(
            signature_text(
                "pub fn f(\n    a: u32,\n    b: u32,\n) {\n    body();\n}\n",
                &index.functions[0]
            ),
            "pub fn f( a: u32, b: u32, ) {"
        );
    }

    #[test]
    fn a_line_removed_inside_a_function_touches_it() {
        let base = "pub fn f() {\n    a();\n    b();\n    c();\n}\n";
        let head = "pub fn f() {\n    a();\n    c();\n}\n";
        let hunks = [file(
            Some("src/a.rs"),
            Some("src/a.rs"),
            Modified,
            &[(3, 1, 2, 0)],
        )];
        assert_eq!(
            run(&[("src/a.rs", base)], &[("src/a.rs", head)], &hunks),
            vec![change("src/a.rs#f#1", ChangeKind::Changed, 0, 1)]
        );
    }

    #[test]
    fn a_line_removed_right_after_a_function_does_not_touch_it() {
        let base = "pub fn f() {\n    a();\n}\n// old note\npub fn g() {}\n";
        let head = "pub fn f() {\n    a();\n}\npub fn g() {}\n";
        let hunks = [file(
            Some("src/a.rs"),
            Some("src/a.rs"),
            Modified,
            &[(4, 1, 3, 0)],
        )];
        assert!(run(&[("src/a.rs", base)], &[("src/a.rs", head)], &hunks).is_empty());
    }

    fn body(altered: usize) -> String {
        let mut s = String::from("pub fn moved() {\n");
        for i in 0..8 {
            s.push_str(&if i < altered {
                format!("    t{i}();\n")
            } else {
                format!("    s{i}();\n")
            });
        }
        s.push_str("}\n");
        s
    }

    fn move_case(altered: usize) -> Vec<ChangedFunction> {
        let (base, head_b) = (body(0), body(altered));
        let hunks = [
            file(
                Some("src/a.rs"),
                Some("src/a.rs"),
                Modified,
                &[(1, 10, 0, 0)],
            ),
            file(None, Some("src/b.rs"), Added, &[(0, 0, 1, 10)]),
        ];
        run(
            &[("src/a.rs", base.as_str())],
            &[("src/a.rs", ""), ("src/b.rs", head_b.as_str())],
            &hunks,
        )
    }

    #[test]
    fn a_function_cut_and_pasted_into_another_file_is_moved_when_at_most_a_fifth_differs() {
        for altered in [0, 2] {
            assert_eq!(
                move_case(altered),
                vec![ChangedFunction {
                    id: id("src/b.rs#moved#1"),
                    kind: ChangeKind::Moved,
                    added: 10,
                    removed: 10,
                    moved_from: Some(("src/a.rs".to_string(), 1)),
                }],
                "{altered} lines altered"
            );
        }
        let got = move_case(3);
        let kinds: Vec<(&str, ChangeKind)> =
            got.iter().map(|c| (c.id.0.as_str(), c.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("src/a.rs#moved#1", ChangeKind::Removed),
                ("src/b.rs#moved#1", ChangeKind::New)
            ]
        );
    }

    #[test]
    fn a_copy_is_new_because_the_original_is_still_there() {
        let text = body(0);
        let hunks = [file(None, Some("src/b.rs"), Added, &[(0, 0, 1, 10)])];
        let got = run(
            &[("src/a.rs", text.as_str())],
            &[("src/a.rs", text.as_str()), ("src/b.rs", text.as_str())],
            &hunks,
        );
        assert_eq!(
            got,
            vec![change("src/b.rs#moved#1", ChangeKind::New, 10, 0)]
        );
    }

    #[test]
    fn a_rename_with_an_edit_is_changed_and_a_pure_rename_reports_nothing() {
        let hunks = [
            file(
                Some("src/old.rs"),
                Some("src/new.rs"),
                Renamed,
                &[(2, 0, 3, 1)],
            ),
            file(Some("src/p.rs"), Some("src/q.rs"), Renamed, &[]),
        ];
        let got = run(
            &[
                ("src/old.rs", "pub fn f() {\n    a();\n}\n"),
                ("src/p.rs", "pub fn g() {}\n"),
            ],
            &[
                ("src/new.rs", "pub fn f() {\n    a();\n    b();\n}\n"),
                ("src/q.rs", "pub fn g() {}\n"),
            ],
            &hunks,
        );
        assert_eq!(
            got,
            vec![change("src/new.rs#f#1", ChangeKind::Changed, 1, 0)]
        );
    }

    #[test]
    fn a_deleted_file_lists_each_function_as_removed_and_an_added_file_as_new() {
        let hunks = [
            file(Some("src/gone.rs"), None, Deleted, &[(1, 2, 0, 0)]),
            file(None, Some("src/fresh.rs"), Added, &[(0, 0, 1, 1)]),
        ];
        let got = run(
            &[("src/gone.rs", "pub fn a() {}\npub fn b() {}\n")],
            &[("src/fresh.rs", "pub fn c() {}\n")],
            &hunks,
        );
        assert_eq!(
            got,
            vec![
                change("src/fresh.rs#c#1", ChangeKind::New, 1, 0),
                change("src/gone.rs#a#1", ChangeKind::Removed, 0, 1),
                change("src/gone.rs#b#2", ChangeKind::Removed, 0, 1),
            ]
        );
    }

    #[test]
    fn a_mode_only_change_and_an_empty_diff_report_nothing() {
        let text = "pub fn f() {}\n";
        let hunks = [file(Some("a.rs"), Some("a.rs"), Modified, &[])];
        assert!(run(&[("a.rs", text)], &[("a.rs", text)], &hunks).is_empty());
        assert!(run(&[], &[("a.rs", text)], &[]).is_empty());
    }
}
