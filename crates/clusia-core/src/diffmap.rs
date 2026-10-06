//! Following commented lines across commits (spec §6.6); deterministic, no agent involved.

use std::collections::{BTreeSet, HashMap};

use crate::draft::{Draft, ItemStatus, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Modified { new_path: String, hunks: Vec<Hunk> },
    Deleted,
}

/// Per old path: how the file changed between two commits. Paths not listed did not change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffMap {
    files: HashMap<String, FileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineMap {
    Same,
    Moved { path: String, line: u32 },
    Changed,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Relocation {
    pub moved: usize,
    pub obsolete: usize,
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `@@ -a[,b] +c[,d] @@ …`
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

/// `a/<path> b/<path>`: without a rename both halves are equal, so split in the middle.
/// Renames are corrected by the `rename from/to` lines that follow.
fn split_git_paths(rest: &str) -> (String, String) {
    let n = rest.len();
    if n >= 5 && (n - 5).is_multiple_of(2) {
        let half = (n - 5) / 2;
        if rest.starts_with("a/")
            && rest.get(2 + half..5 + half) == Some(" b/")
            && let (Some(a), Some(b)) = (rest.get(2..2 + half), rest.get(5 + half..))
        {
            return (a.to_string(), b.to_string());
        }
    }
    match rest.split_once(" b/") {
        Some((a, b)) => (a.trim_start_matches("a/").to_string(), b.to_string()),
        None => (rest.to_string(), rest.to_string()),
    }
}

struct Pending {
    old: String,
    new: String,
    added: bool,
    deleted: bool,
    hunks: Vec<Hunk>,
}

impl DiffMap {
    pub fn parse(diff: &str) -> DiffMap {
        let mut files = HashMap::new();
        let mut current: Option<Pending> = None;
        let flush = |pending: Option<Pending>, files: &mut HashMap<String, FileChange>| {
            let Some(p) = pending else { return };
            if p.added {
                return;
            }
            let change = if p.deleted {
                FileChange::Deleted
            } else {
                FileChange::Modified {
                    new_path: p.new,
                    hunks: p.hunks,
                }
            };
            files.insert(p.old, change);
        };
        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix("diff --git ") {
                flush(current.take(), &mut files);
                let (old, new) = split_git_paths(rest);
                current = Some(Pending {
                    old,
                    new,
                    added: false,
                    deleted: false,
                    hunks: Vec::new(),
                });
                continue;
            }
            let Some(p) = current.as_mut() else { continue };
            if line.starts_with("new file mode") {
                p.added = true;
            } else if line.starts_with("deleted file mode") {
                p.deleted = true;
            } else if let Some(path) = line.strip_prefix("rename from ") {
                p.old = path.to_string();
            } else if let Some(path) = line.strip_prefix("rename to ") {
                p.new = path.to_string();
            } else if line.starts_with("@@ ")
                && let Some(h) = parse_hunk_header(line)
            {
                p.hunks.push(h);
            }
        }
        flush(current.take(), &mut files);
        DiffMap { files }
    }

    /// Where old line `line` of `path` is in the new commit.
    pub fn map_line(&self, path: &str, line: u32) -> LineMap {
        match self.files.get(path) {
            None => LineMap::Same,
            Some(FileChange::Deleted) => LineMap::Deleted,
            Some(FileChange::Modified { new_path, hunks }) => match shift(hunks, line) {
                None => LineMap::Changed,
                Some(new_line) if new_line == line && new_path == path => LineMap::Same,
                Some(new_line) => LineMap::Moved {
                    path: new_path.clone(),
                    line: new_line,
                },
            },
        }
    }
}

/// New position of an unchanged old line, or `None` when a hunk replaced or removed it.
fn shift(hunks: &[Hunk], line: u32) -> Option<u32> {
    let mut offset: i64 = 0;
    for h in hunks {
        if h.old_len == 0 {
            // Pure insertion after old line `old_start`.
            if line > h.old_start {
                offset += i64::from(h.new_len);
            } else {
                break;
            }
        } else {
            if line < h.old_start {
                break;
            }
            if line < h.old_start + h.old_len {
                return None;
            }
            offset += i64::from(h.new_len) - i64::from(h.old_len);
        }
    }
    u32::try_from(i64::from(line) + offset).ok()
}

fn obsolete(reason: &str) -> ItemStatus {
    ItemStatus::Obsolete {
        reason: reason.to_string(),
    }
}

/// Re-anchors every line comment of `draft` from the old head/base to the new ones.
pub fn relocate(
    draft: &mut Draft,
    head: &DiffMap,
    base: Option<&DiffMap>,
    new_head: &str,
    new_base: &str,
) -> Relocation {
    let mut report = Relocation::default();
    for item in &mut draft.items {
        if matches!(item.status, ItemStatus::Obsolete { .. }) {
            continue;
        }
        let Some(anchor) = item.anchor.as_mut() else {
            continue;
        };
        let (map, new_commit) = match anchor.side {
            Side::Right => (Some(head), new_head),
            Side::Left => (base, new_base),
        };
        let Some(map) = map else {
            item.status = obsolete("the base branch changed");
            report.obsolete += 1;
            continue;
        };
        let start = anchor.start_line.unwrap_or(anchor.line);
        let mapped: Vec<LineMap> = (start..=anchor.line)
            .map(|l| map.map_line(&anchor.path, l))
            .collect();
        if mapped.contains(&LineMap::Deleted) {
            item.status = obsolete("the file was deleted");
            report.obsolete += 1;
            continue;
        }
        if mapped.contains(&LineMap::Changed) {
            item.status = obsolete("the commented lines changed");
            report.obsolete += 1;
            continue;
        }
        anchor.commit = new_commit.to_string();
        let end = mapped.last().cloned().unwrap_or(LineMap::Same);
        let first = mapped.first().cloned().unwrap_or(LineMap::Same);
        if end == LineMap::Same && first == LineMap::Same {
            continue;
        }
        let (from_path, from_line) = (anchor.path.clone(), anchor.line);
        if let LineMap::Moved { path, line } = end {
            anchor.path = path;
            anchor.line = line;
        }
        if anchor.start_line.is_some()
            && let LineMap::Moved { line, .. } = first
        {
            anchor.start_line = Some(line);
        }
        item.status = ItemStatus::Moved {
            from_path,
            from_line,
        };
        report.moved += 1;
    }
    report
}

/// (left, right) line numbers present in a GitHub file patch, i.e. where a review comment is accepted.
pub fn commentable_lines(patch: &str) -> (BTreeSet<u32>, BTreeSet<u32>) {
    let (mut left, mut right) = (BTreeSet::new(), BTreeSet::new());
    let (mut old, mut new) = (0u32, 0u32);
    for line in patch.lines() {
        if line.starts_with("@@ ") {
            if let Some(h) = parse_hunk_header(line) {
                old = h.old_start;
                new = h.new_start;
            }
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                right.insert(new);
                new += 1;
            }
            Some(b'-') => {
                left.insert(old);
                old += 1;
            }
            Some(b' ') => {
                left.insert(old);
                right.insert(new);
                old += 1;
                new += 1;
            }
            _ => {} // "\ No newline at end of file"
        }
    }
    (left, right)
}

pub fn can_comment(patch: &str, side: Side, line: u32) -> bool {
    let (left, right) = commentable_lines(patch);
    match side {
        Side::Left => left.contains(&line),
        Side::Right => right.contains(&line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::{Anchor, DraftKind};

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -2,0 +3,2 @@ fn top() {
+    let x = 1;
+    let y = 2;
@@ -10 +12 @@ fn mid() {
-    old();
+    new();
@@ -20,3 +22,0 @@ fn bottom() {
-    a();
-    b();
-    c();
diff --git a/old/name.rs b/new/name.rs
similarity index 95%
rename from old/name.rs
rename to new/name.rs
index 333..444 100644
--- a/old/name.rs
+++ b/new/name.rs
@@ -1 +1,2 @@
-use x;
+use x;
+use y;
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index 555..000
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn gone() {}
-
diff --git a/fresh.rs b/fresh.rs
new file mode 100644
index 000..666
--- /dev/null
+++ b/fresh.rs
@@ -0,0 +1 @@
+fn fresh() {}
diff --git a/with space.rs b/with space.rs
index 777..888 100644
--- a/with space.rs
+++ b/with space.rs
@@ -5,0 +6 @@
+// added
";

    #[test]
    fn maps_lines_through_hunks() {
        let m = DiffMap::parse(DIFF);
        assert_eq!(m.map_line("src/a.rs", 1), LineMap::Same);
        assert_eq!(
            m.map_line("src/a.rs", 2),
            LineMap::Same,
            "insertion is after line 2"
        );
        assert_eq!(
            m.map_line("src/a.rs", 3),
            LineMap::Moved {
                path: "src/a.rs".into(),
                line: 5
            }
        );
        assert_eq!(
            m.map_line("src/a.rs", 9),
            LineMap::Moved {
                path: "src/a.rs".into(),
                line: 11
            }
        );
        assert_eq!(m.map_line("src/a.rs", 10), LineMap::Changed);
        assert_eq!(
            m.map_line("src/a.rs", 15),
            LineMap::Moved {
                path: "src/a.rs".into(),
                line: 17
            }
        );
        assert_eq!(m.map_line("src/a.rs", 21), LineMap::Changed);
        assert_eq!(
            m.map_line("src/a.rs", 30),
            LineMap::Moved {
                path: "src/a.rs".into(),
                line: 29
            }
        );
        assert_eq!(m.map_line("untouched.rs", 7), LineMap::Same);
        assert_eq!(
            m.map_line("with space.rs", 9),
            LineMap::Moved {
                path: "with space.rs".into(),
                line: 10
            }
        );
    }

    #[test]
    fn follows_renames_and_deletions() {
        let m = DiffMap::parse(DIFF);
        assert_eq!(m.map_line("old/name.rs", 1), LineMap::Changed);
        assert_eq!(
            m.map_line("old/name.rs", 5),
            LineMap::Moved {
                path: "new/name.rs".into(),
                line: 6
            }
        );
        assert_eq!(m.map_line("gone.rs", 1), LineMap::Deleted);
        assert_eq!(
            m.map_line("fresh.rs", 1),
            LineMap::Same,
            "added files have no old lines to map"
        );
    }

    fn draft_with(anchors: &[(&str, u32, Option<u32>, Side)]) -> Draft {
        let mut d = Draft::default();
        for (path, line, start, side) in anchors {
            let commit = if *side == Side::Right { "h1" } else { "b1" };
            let a = Anchor {
                path: (*path).into(),
                line: *line,
                start_line: *start,
                side: *side,
                commit: commit.into(),
            };
            d.add(DraftKind::LineComment, Some(a), None, "c", 1)
                .unwrap();
        }
        d
    }

    #[test]
    fn relocate_moves_marks_and_restamps() {
        let mut d = draft_with(&[
            ("src/a.rs", 1, None, Side::Right),
            ("src/a.rs", 3, None, Side::Right),
            ("src/a.rs", 10, None, Side::Right),
            ("gone.rs", 1, None, Side::Right),
            ("old/name.rs", 5, Some(4), Side::Right),
        ]);
        d.add(DraftKind::General, None, None, "overall", 1).unwrap();
        let report = relocate(
            &mut d,
            &DiffMap::parse(DIFF),
            Some(&DiffMap::default()),
            "h2",
            "b1",
        );
        assert_eq!(
            report,
            Relocation {
                moved: 2,
                obsolete: 2
            }
        );

        let a1 = d.items[0].anchor.as_ref().unwrap();
        assert_eq!(
            (a1.line, a1.commit.as_str(), &d.items[0].status),
            (1, "h2", &ItemStatus::Ok)
        );
        let a2 = d.items[1].anchor.as_ref().unwrap();
        assert_eq!((a2.line, a2.commit.as_str()), (5, "h2"));
        assert_eq!(
            d.items[1].status,
            ItemStatus::Moved {
                from_path: "src/a.rs".into(),
                from_line: 3
            }
        );
        assert_eq!(
            d.items[2].status,
            ItemStatus::Obsolete {
                reason: "the commented lines changed".into()
            }
        );
        assert_eq!(
            d.items[3].status,
            ItemStatus::Obsolete {
                reason: "the file was deleted".into()
            }
        );
        let a5 = d.items[4].anchor.as_ref().unwrap();
        assert_eq!(
            (a5.path.as_str(), a5.start_line, a5.line),
            ("new/name.rs", Some(5), 6)
        );
    }

    #[test]
    fn relocate_marks_ranges_with_any_changed_line() {
        let mut d = draft_with(&[("src/a.rs", 11, Some(9), Side::Right)]);
        relocate(&mut d, &DiffMap::parse(DIFF), None, "h2", "b2");
        assert_eq!(
            d.items[0].status,
            ItemStatus::Obsolete {
                reason: "the commented lines changed".into()
            }
        );
    }

    #[test]
    fn relocate_left_side_needs_a_base_map() {
        let mut d = draft_with(&[("src/a.rs", 4, None, Side::Left)]);
        let report = relocate(&mut d, &DiffMap::default(), None, "h2", "b2");
        assert_eq!(report.obsolete, 1);
        assert_eq!(
            d.items[0].status,
            ItemStatus::Obsolete {
                reason: "the base branch changed".into()
            }
        );

        let mut d = draft_with(&[("src/a.rs", 4, None, Side::Left)]);
        relocate(
            &mut d,
            &DiffMap::default(),
            Some(&DiffMap::default()),
            "h2",
            "b2",
        );
        assert_eq!(d.items[0].anchor.as_ref().unwrap().commit, "b2");
    }

    #[test]
    fn obsolete_items_stay_obsolete() {
        let mut d = draft_with(&[("src/a.rs", 3, None, Side::Right)]);
        d.items[0].status = ItemStatus::Obsolete {
            reason: "earlier".into(),
        };
        let report = relocate(&mut d, &DiffMap::parse(DIFF), None, "h2", "b2");
        assert_eq!(report, Relocation::default());
        assert_eq!(d.items[0].anchor.as_ref().unwrap().line, 3);
    }

    #[test]
    fn commentable_lines_follow_the_patch() {
        let patch = "@@ -10,4 +10,5 @@ fn f() {\n ctx\n-old\n+new\n+more\n ctx2\n\\ No newline at end of file";
        let (left, right) = commentable_lines(patch);
        assert_eq!(left.into_iter().collect::<Vec<_>>(), vec![10, 11, 12]);
        assert_eq!(right.into_iter().collect::<Vec<_>>(), vec![10, 11, 12, 13]);
        assert!(can_comment(patch, Side::Right, 12));
        assert!(can_comment(patch, Side::Left, 11));
        assert!(!can_comment(patch, Side::Right, 40));
    }
}
