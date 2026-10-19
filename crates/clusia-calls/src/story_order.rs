//! The Story's order before any agent has spoken: the changed functions callees first, files
//! the call graph cannot read as one step each, and everything else in one last step. Every
//! changed line lands in exactly one step.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use clusia_core::hunks::{FileHunks, HunkRef, hunk_ref};
use serde::{Deserialize, Serialize};

use crate::changes::{ChangeKind, ChangedFunction};
use crate::index::{CallIndex, Function};
use crate::packs::{language_name, pack_for};
use crate::resolve::{Resolved, Target};

/// Function steps kept as they are; the rest of the changed functions group by file. The
/// story format caps a step list at the same number.
const MAX_STEPS: usize = 12;
/// Longest id and "Other files" subtitle.
const ID_MAX: usize = 128;
const SUBTITLE_MAX: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticStep {
    /// A sanitised qualified name or path, unique in the list.
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// Why the step is where it is, from the index alone.
    pub explanation: String,
    pub hunks: Vec<HunkRef>,
    /// The last step, for everything that is not a function.
    pub other: bool,
}

/// `[A-Za-z0-9_.:-]` only, at most `ID_MAX` characters.
fn sanitise(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(ID_MAX)
        .collect()
}

/// Ids handed out so far; a repeat gets `:<line>` and then a counter.
#[derive(Default)]
struct Ids(HashSet<String>);

impl Ids {
    fn take(&mut self, text: &str, line: u32) -> String {
        let base = sanitise(text);
        let mut id = base.clone();
        if self.0.contains(&id) {
            id = format!("{base}:{line}");
        }
        let mut n = 2;
        while self.0.contains(&id) {
            id = format!("{base}:{line}:{n}");
            n += 1;
        }
        self.0.insert(id.clone());
        id
    }
}

/// Sorts ranges by `(path, start)` and merges those that overlap or touch.
fn merge(mut pieces: Vec<HunkRef>) -> Vec<HunkRef> {
    pieces.sort_by(|a, b| (&a.path, a.start_line).cmp(&(&b.path, b.start_line)));
    let mut out: Vec<HunkRef> = Vec::new();
    for piece in pieces {
        match out.last_mut() {
            Some(last)
                if last.path == piece.path
                    && piece.start_line <= last.end_line.saturating_add(1) =>
            {
                last.end_line = last.end_line.max(piece.end_line);
            }
            _ => out.push(piece),
        }
    }
    out
}

fn lines_of(hunks: &[HunkRef]) -> u32 {
    hunks.iter().map(|h| h.end_line - h.start_line + 1).sum()
}

fn lines_changed(hunks: &[HunkRef]) -> String {
    match lines_of(hunks) {
        1 => "1 line changed".to_string(),
        n => format!("{n} lines changed"),
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The text of `limit` characters at most, ending in `…` when it was cut.
fn cut(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let mut cut: String = text.chars().take(limit - 1).collect();
    cut.push('…');
    cut
}

/// The changed functions of the index in review order, as steps. `hunks` are the pull
/// request's, so the steps together cover its whole diff.
pub fn static_order(
    index: &CallIndex,
    resolved: &Resolved,
    changed: &[ChangedFunction],
    hunks: &[FileHunks],
) -> Vec<StaticStep> {
    let by_id: HashMap<&str, &Function> = index
        .functions
        .iter()
        .map(|f| (f.id.0.as_str(), f))
        .collect();
    let mut seen_ids: HashSet<&str> = HashSet::new();
    let steps: Vec<(&ChangedFunction, &Function)> = changed
        .iter()
        .filter(|c| c.kind != ChangeKind::Removed)
        .filter(|c| seen_ids.insert(c.id.0.as_str()))
        .filter_map(|c| by_id.get(c.id.0.as_str()).map(|f| (c, *f)))
        .collect();

    // Callees first: an edge `u -> v` between two steps puts `v` before `u`.
    let position: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, (_, f))| (f.id.0.as_str(), i))
        .collect();
    let mut calls: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); steps.len()];
    let mut callers_of: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for (from, target, _) in &resolved.edges {
        let Target::Local { ids } = target else {
            continue;
        };
        for id in ids {
            if id != from {
                callers_of
                    .entry(id.0.as_str())
                    .or_default()
                    .insert(from.0.as_str());
            }
            if let (Some(&u), Some(&v)) =
                (position.get(from.0.as_str()), position.get(id.0.as_str()))
                && u != v
            {
                calls[u].insert(v);
            }
        }
    }
    let key = |i: usize| (steps[i].1.path.as_str(), steps[i].1.start_line);
    let mut placed = vec![false; steps.len()];
    let mut ordered: Vec<usize> = Vec::with_capacity(steps.len());
    while ordered.len() < steps.len() {
        let remaining = (0..steps.len()).filter(|&i| !placed[i]);
        let ready = remaining
            .clone()
            .filter(|&i| calls[i].iter().all(|&v| placed[v]))
            .min_by_key(|&i| key(i));
        let next = ready.or_else(|| remaining.min_by_key(|&i| key(i)));
        let Some(next) = next else { break };
        placed[next] = true;
        ordered.push(next);
    }
    let rank: HashMap<usize, usize> = ordered.iter().enumerate().map(|(r, &i)| (i, r)).collect();

    // Pieces: each hunk clipped to the smallest step function that holds it, and what no
    // function holds goes to the last step.
    let mut by_path: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, f)) in steps.iter().enumerate() {
        by_path.entry(f.path.as_str()).or_default().push(i);
    }
    for owners in by_path.values_mut() {
        owners.sort_by_key(|&i| {
            let f = steps[i].1;
            (f.end_line - f.start_line, f.start_line)
        });
    }
    let mut pieces: Vec<Vec<HunkRef>> = vec![Vec::new(); steps.len()];
    let mut rest: Vec<HunkRef> = Vec::new();
    let mut unsupported: BTreeMap<String, (&'static str, Vec<HunkRef>)> = BTreeMap::new();
    for file in hunks {
        let path = match (&file.new_path, &file.old_path) {
            (Some(path), _) => path,
            (None, Some(old)) => old,
            (None, None) => continue,
        };
        let refs: Vec<HunkRef> = file
            .hunks
            .iter()
            .filter_map(|h| hunk_ref(file, h))
            .collect();
        if let Some(language) = language_name(path)
            && pack_for(path).is_none()
        {
            if !refs.is_empty() {
                unsupported.insert(path.clone(), (language, refs));
            }
            continue;
        }
        let owners = by_path.get(path.as_str());
        for hunk in &file.hunks {
            let Some(piece) = hunk_ref(file, hunk) else {
                continue;
            };
            let point = hunk.new_len == 0 && file.new_path.is_some();
            let mut free = vec![(piece.start_line, piece.end_line)];
            for &owner in owners.into_iter().flatten() {
                let f = steps[owner].1;
                if point {
                    if f.start_line <= piece.start_line && piece.start_line < f.end_line {
                        pieces[owner].push(piece.clone());
                        free.clear();
                        break;
                    }
                    continue;
                }
                let mut next_free = Vec::with_capacity(free.len() + 1);
                for (a, b) in free {
                    let (lo, hi) = (a.max(f.start_line), b.min(f.end_line));
                    if lo > hi {
                        next_free.push((a, b));
                        continue;
                    }
                    pieces[owner].push(HunkRef {
                        path: piece.path.clone(),
                        start_line: lo,
                        end_line: hi,
                    });
                    if a < lo {
                        next_free.push((a, lo - 1));
                    }
                    if hi < b {
                        next_free.push((hi + 1, b));
                    }
                }
                free = next_free;
                if free.is_empty() {
                    break;
                }
            }
            rest.extend(free.into_iter().map(|(a, b)| HunkRef {
                path: piece.path.clone(),
                start_line: a,
                end_line: b,
            }));
        }
    }

    let mut ids = Ids::default();
    let mut out: Vec<StaticStep> = Vec::new();
    let kept = ordered.len().min(MAX_STEPS);
    for &i in &ordered[..kept] {
        let (change, f) = steps[i];
        let detail = match change.kind {
            ChangeKind::Changed => format!("+{} −{}", change.added, change.removed),
            ChangeKind::Signature => "signature".to_string(),
            ChangeKind::Moved => format!("moved, +{} −{}", change.added, change.removed),
            _ => "new".to_string(),
        };
        let kind = match change.kind {
            ChangeKind::Changed => "Changed body".to_string(),
            ChangeKind::Signature => "Signature changed".to_string(),
            ChangeKind::New => "New".to_string(),
            _ => match &change.moved_from {
                Some((path, line)) => format!("Moved from {path}:{line}"),
                None => "Moved".to_string(),
            },
        };
        let mut parts = vec![kind];
        let called_by: Vec<&str> = callers_of
            .get(f.id.0.as_str())
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        if let Some(first) = called_by.first() {
            let name = by_id.get(first).map_or(*first, |c| c.qualified.as_str());
            let others = called_by.len() - 1;
            parts.push(if others > 0 {
                format!("called by {name} (+{others})")
            } else {
                format!("called by {name}")
            });
        }
        match calls[i].iter().max_by_key(|&&v| rank[&v]) {
            None => parts.push("comes first because nothing it calls changed".to_string()),
            Some(&last) => parts.push(format!("after {}", steps[last].1.qualified)),
        }
        out.push(StaticStep {
            id: ids.take(&f.qualified, f.start_line),
            title: f.qualified.clone(),
            subtitle: format!("{} · {detail}", f.path),
            explanation: parts.join(" · "),
            hunks: merge(std::mem::take(&mut pieces[i])),
            other: false,
        });
    }

    // The functions past the cap, one step per file in order of first appearance.
    let mut files: Vec<(&str, usize, Vec<HunkRef>)> = Vec::new();
    for &i in &ordered[kept..] {
        let path = steps[i].1.path.as_str();
        let at = match files.iter().position(|(p, _, _)| *p == path) {
            Some(at) => at,
            None => {
                files.push((path, 0, Vec::new()));
                files.len() - 1
            }
        };
        files[at].1 += 1;
        files[at].2.append(&mut pieces[i]);
    }
    for (path, n, group) in files {
        let group = merge(group);
        out.push(StaticStep {
            id: ids.take(path, 0),
            title: path.to_string(),
            subtitle: count(n, "function", "functions"),
            explanation: lines_changed(&group),
            hunks: group,
            other: false,
        });
    }

    for (path, (language, refs)) in unsupported {
        out.push(StaticStep {
            id: ids.take(&path, 0),
            title: path.clone(),
            subtitle: format!("{language} · no call graph"),
            explanation: lines_changed(&refs),
            hunks: refs,
            other: false,
        });
    }

    let rest = merge(rest);
    if !rest.is_empty() {
        let paths: BTreeSet<&str> = rest.iter().map(|h| h.path.as_str()).collect();
        let names: Vec<&str> = paths.iter().map(|p| file_name(p)).collect();
        let subtitle = cut(
            format!(
                "{} · {}",
                names.join(", "),
                count(paths.len(), "file", "files")
            ),
            SUBTITLE_MAX,
        );
        out.push(StaticStep {
            id: "other-files".to_string(),
            title: "Other files".to_string(),
            subtitle,
            explanation: lines_changed(&rest),
            hunks: rest,
            other: true,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{assert_covers, file, index_of};
    use crate::{ChangeKind, ChangedFunction, FunctionId, resolve};
    use clusia_core::hunks::FileStatus::{Added, Deleted, Modified};
    use clusia_core::hunks::{FileHunks, HunkRef};

    fn changed(i: &str, kind: ChangeKind, added: u32, removed: u32) -> ChangedFunction {
        ChangedFunction {
            id: FunctionId(i.to_string()),
            kind,
            added,
            removed,
            moved_from: None,
        }
    }

    fn r(path: &str, start: u32, end: u32) -> HunkRef {
        HunkRef {
            path: path.to_string(),
            start_line: start,
            end_line: end,
        }
    }

    fn order(
        files: &[(&str, &str)],
        changes: &[ChangedFunction],
        hunks: &[FileHunks],
    ) -> Vec<StaticStep> {
        let index = index_of(files);
        static_order(&index, &resolve(&index), changes, hunks)
    }

    fn step(
        id: &str,
        title: &str,
        subtitle: &str,
        explanation: &str,
        hunks: Vec<HunkRef>,
        other: bool,
    ) -> StaticStep {
        StaticStep {
            id: id.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            explanation: explanation.into(),
            hunks,
            other,
        }
    }

    const STORE: &str = "use std::io;\n\npub fn top() {\n    mid();\n}\n\npub fn mid() {\n    leaf();\n}\n\npub fn leaf() {}\n";

    #[test]
    fn callees_first_then_unsupported_files_then_other_files() {
        let hunks = vec![
            file(
                Some("src/store.rs"),
                Some("src/store.rs"),
                Modified,
                &[(1, 1, 1, 1), (4, 1, 4, 1), (8, 1, 8, 1), (11, 1, 11, 1)],
            ),
            file(
                Some("README.md"),
                Some("README.md"),
                Modified,
                &[(3, 0, 4, 2)],
            ),
            file(
                Some("Cargo.toml"),
                Some("Cargo.toml"),
                Modified,
                &[(5, 1, 5, 1)],
            ),
            file(
                Some("cmd/main.go"),
                Some("cmd/main.go"),
                Modified,
                &[(1, 1, 1, 1)],
            ),
        ];
        let changes = [
            changed("src/store.rs#top#3", ChangeKind::Changed, 1, 1),
            changed("src/store.rs#mid#7", ChangeKind::Changed, 1, 1),
            changed("src/store.rs#leaf#11", ChangeKind::Changed, 1, 1),
        ];
        let steps = order(&[("src/store.rs", STORE)], &changes, &hunks);
        assert_eq!(
            steps,
            vec![
                step(
                    "leaf",
                    "leaf",
                    "src/store.rs · +1 −1",
                    "Changed body · called by mid · comes first because nothing it calls changed",
                    vec![r("src/store.rs", 11, 11)],
                    false
                ),
                step(
                    "mid",
                    "mid",
                    "src/store.rs · +1 −1",
                    "Changed body · called by top · after leaf",
                    vec![r("src/store.rs", 8, 8)],
                    false
                ),
                step(
                    "top",
                    "top",
                    "src/store.rs · +1 −1",
                    "Changed body · after mid",
                    vec![r("src/store.rs", 4, 4)],
                    false
                ),
                step(
                    "cmd_main.go",
                    "cmd/main.go",
                    "Go · no call graph",
                    "1 line changed",
                    vec![r("cmd/main.go", 1, 1)],
                    false
                ),
                step(
                    "other-files",
                    "Other files",
                    "Cargo.toml, README.md, store.rs · 3 files",
                    "4 lines changed",
                    vec![
                        r("Cargo.toml", 5, 5),
                        r("README.md", 4, 5),
                        r("src/store.rs", 1, 1)
                    ],
                    true
                ),
            ]
        );
        assert_covers(&steps, &hunks);
    }

    #[test]
    fn each_kind_has_its_subtitle_and_a_hunk_is_clipped_to_its_function() {
        let src = "pub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn d() {}\n";
        let changes = [
            changed("src/s.rs#a#1", ChangeKind::Changed, 4, 1),
            changed("src/s.rs#b#2", ChangeKind::Signature, 1, 1),
            ChangedFunction {
                moved_from: Some(("src/a.rs".to_string(), 9)),
                ..changed("src/s.rs#c#3", ChangeKind::Moved, 6, 1)
            },
            changed("src/s.rs#d#4", ChangeKind::New, 1, 0),
        ];
        let hunks = [file(
            Some("src/s.rs"),
            Some("src/s.rs"),
            Modified,
            &[(1, 0, 1, 4)],
        )];
        let steps = order(&[("src/s.rs", src)], &changes, &hunks);
        let got: Vec<(&str, &str, &str, &[HunkRef])> = steps
            .iter()
            .map(|s| {
                (
                    s.title.as_str(),
                    s.subtitle.as_str(),
                    s.explanation.as_str(),
                    s.hunks.as_slice(),
                )
            })
            .collect();
        let first = "comes first because nothing it calls changed";
        assert_eq!(
            got,
            vec![
                (
                    "a",
                    "src/s.rs · +4 −1",
                    "Changed body · comes first because nothing it calls changed",
                    &[r("src/s.rs", 1, 1)][..]
                ),
                (
                    "b",
                    "src/s.rs · signature",
                    &format!("Signature changed · {first}")[..],
                    &[r("src/s.rs", 2, 2)][..]
                ),
                (
                    "c",
                    "src/s.rs · moved, +6 −1",
                    &format!("Moved from src/a.rs:9 · {first}")[..],
                    &[r("src/s.rs", 3, 3)][..]
                ),
                (
                    "d",
                    "src/s.rs · new",
                    &format!("New · {first}")[..],
                    &[r("src/s.rs", 4, 4)][..]
                ),
            ]
        );
    }

    #[test]
    fn a_cycle_is_broken_by_path_order() {
        let src = "pub fn a() {\n    b();\n}\npub fn b() {\n    a();\n}\n";
        let changes = [
            changed("src/c.rs#a#1", ChangeKind::Changed, 1, 1),
            changed("src/c.rs#b#4", ChangeKind::Changed, 1, 1),
        ];
        let hunks = [file(
            Some("src/c.rs"),
            Some("src/c.rs"),
            Modified,
            &[(2, 1, 2, 1), (5, 1, 5, 1)],
        )];
        let titles: Vec<String> = order(&[("src/c.rs", src)], &changes, &hunks)
            .into_iter()
            .map(|s| s.title)
            .collect();
        assert_eq!(titles, vec!["a", "b"]);
    }

    #[test]
    fn forty_changed_functions_keep_twelve_steps_and_group_the_rest_by_file() {
        let sources: Vec<(String, String)> = (0..4)
            .map(|f| {
                (
                    format!("src/m{f}.rs"),
                    (0..10)
                        .map(|j| format!("pub fn f{f}_{j}() {{}}\n"))
                        .collect::<String>(),
                )
            })
            .collect();
        let files: Vec<(&str, &str)> = sources
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let changes: Vec<ChangedFunction> = (0..4)
            .flat_map(|f| {
                (0..10).map(move |j| {
                    changed(
                        &format!("src/m{f}.rs#f{f}_{j}#{}", j + 1),
                        ChangeKind::New,
                        1,
                        0,
                    )
                })
            })
            .collect();
        let hunks: Vec<FileHunks> = (0..4)
            .map(|f| file(None, Some(&format!("src/m{f}.rs")), Added, &[(0, 0, 1, 10)]))
            .collect();
        let steps = order(&files, &changes, &hunks);
        assert_eq!(steps.len(), 15);
        assert_eq!(steps[11].title, "f1_1");
        assert_eq!(
            steps[12],
            step(
                "src_m1.rs",
                "src/m1.rs",
                "8 functions",
                "8 lines changed",
                vec![r("src/m1.rs", 3, 10)],
                false
            )
        );
        assert_eq!(
            steps[13],
            step(
                "src_m2.rs",
                "src/m2.rs",
                "10 functions",
                "10 lines changed",
                vec![r("src/m2.rs", 1, 10)],
                false
            )
        );
        assert_eq!(
            steps[0].explanation,
            "New · comes first because nothing it calls changed"
        );
        assert_eq!(steps[14].title, "src/m3.rs");
        assert_covers(&steps, &hunks);
    }

    #[test]
    fn a_deleted_source_file_and_a_deleted_unsupported_file_are_still_covered() {
        let hunks = [
            file(Some("old.go"), None, Deleted, &[(1, 3, 0, 0)]),
            file(Some("src/gone.rs"), None, Deleted, &[(1, 3, 0, 0)]),
        ];
        let steps = order(&[], &[], &hunks);
        assert_eq!(
            steps,
            vec![
                step(
                    "old.go",
                    "old.go",
                    "Go · no call graph",
                    "3 lines changed",
                    vec![r("old.go", 1, 3)],
                    false
                ),
                step(
                    "other-files",
                    "Other files",
                    "gone.rs · 1 file",
                    "3 lines changed",
                    vec![r("src/gone.rs", 1, 3)],
                    true
                ),
            ]
        );
        assert_covers(&steps, &hunks);
    }

    #[test]
    fn ids_are_safe_and_unique() {
        let changes = [
            changed("src/d.rs#f#1", ChangeKind::New, 1, 0),
            changed("src/d.rs#f#2", ChangeKind::New, 1, 0),
            changed("x.ts#K.#p#2", ChangeKind::New, 1, 0),
        ];
        let hunks = [
            file(None, Some("src/d.rs"), Added, &[(0, 0, 1, 2)]),
            file(None, Some("x.ts"), Added, &[(0, 0, 1, 3)]),
        ];
        let steps = order(
            &[
                ("src/d.rs", "pub fn f() {}\npub fn f() {}\n"),
                ("x.ts", "class K {\n  #p() {}\n}\n"),
            ],
            &changes,
            &hunks,
        );
        let ids: Vec<(&str, &str)> = steps
            .iter()
            .filter(|s| !s.other)
            .map(|s| (s.id.as_str(), s.title.as_str()))
            .collect();
        assert_eq!(ids, vec![("f", "f"), ("f:2", "f"), ("K._p", "K.#p")]);
    }

    #[test]
    fn a_pr_of_only_markdown_and_toml_is_one_other_files_step_and_a_go_only_pr_is_one_step_per_file()
     {
        let docs = [
            file(
                Some("README.md"),
                Some("README.md"),
                Modified,
                &[(1, 1, 1, 1)],
            ),
            file(
                Some("Cargo.toml"),
                Some("Cargo.toml"),
                Modified,
                &[(2, 1, 2, 1)],
            ),
        ];
        let steps = order(&[], &[], &docs);
        assert_eq!(
            steps,
            vec![step(
                "other-files",
                "Other files",
                "Cargo.toml, README.md · 2 files",
                "2 lines changed",
                vec![r("Cargo.toml", 2, 2), r("README.md", 1, 1)],
                true
            )]
        );
        assert_covers(&steps, &docs);

        let go = [
            file(Some("a.go"), Some("a.go"), Modified, &[(1, 1, 1, 1)]),
            file(None, Some("b.go"), Added, &[(0, 0, 1, 5)]),
        ];
        let steps = order(&[], &[], &go);
        let got: Vec<(&str, bool)> = steps.iter().map(|s| (s.id.as_str(), s.other)).collect();
        assert_eq!(got, vec![("a.go", false), ("b.go", false)]);
        assert_eq!(steps[1].explanation, "5 lines changed");
        assert_covers(&steps, &go);
    }

    #[test]
    fn no_changes_give_no_steps() {
        assert!(order(&[("src/store.rs", STORE)], &[], &[]).is_empty());
    }
}
