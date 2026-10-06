//! A GitHub file patch as the review window draws it: unified rows, split pairs, intraline
//! changes, and where comments sit. Line numbers and commentable lines follow
//! `clusia_core::commentable_lines`, so the window offers exactly the lines the daemon accepts.

use std::ops::Range;

use clusia_core::{Side, can_comment};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKind {
    Hunk,
    Context,
    Added,
    Removed,
}

/// One patch line. `text` has no `+`/`-`/space marker; a `Hunk` row carries the whole
/// `@@ … @@` header and no numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

/// `(old_start, new_start)` of `@@ -a[,b] +c[,d] @@ …`.
fn hunk_starts(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    // Same acceptance as `clusia_core`: a length, when present, must parse too.
    let start = |range: &str| match range.split_once(',') {
        Some((start, len)) => {
            len.parse::<u32>().ok()?;
            start.parse::<u32>().ok()
        }
        None => range.parse::<u32>().ok(),
    };
    Some((start(old)?, start(new)?))
}

/// Rows of a patch. `\ No newline at end of file` and anything else unknown is dropped.
pub fn parse_patch(patch: &str) -> Vec<Row> {
    let (mut old, mut new) = (0u32, 0u32);
    let mut rows = Vec::new();
    for line in patch.lines() {
        if line.starts_with("@@ ") {
            if let Some((o, n)) = hunk_starts(line) {
                old = o;
                new = n;
            }
            rows.push(Row {
                kind: RowKind::Hunk,
                old: None,
                new: None,
                text: line.to_string(),
            });
            continue;
        }
        let (kind, o, n) = match line.as_bytes().first() {
            Some(b'+') => (RowKind::Added, None, Some(new)),
            Some(b'-') => (RowKind::Removed, Some(old), None),
            Some(b' ') => (RowKind::Context, Some(old), Some(new)),
            _ => continue,
        };
        if o.is_some() {
            old += 1;
        }
        if n.is_some() {
            new += 1;
        }
        rows.push(Row {
            kind,
            old: o,
            new: n,
            text: line[1..].to_string(),
        });
    }
    rows
}

/// One line of the Split view: indices into the rows. A hunk header spans both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitRow {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub hunk: Option<usize>,
}

/// Context rows sit on both sides; a run of removed lines is paired line by line with the run
/// of added lines that follows it, and the longer run's extra lines stand alone.
pub fn split_rows(rows: &[Row]) -> Vec<SplitRow> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        match rows[i].kind {
            RowKind::Hunk => {
                out.push(SplitRow {
                    left: None,
                    right: None,
                    hunk: Some(i),
                });
                i += 1;
            }
            RowKind::Context => {
                out.push(SplitRow {
                    left: Some(i),
                    right: Some(i),
                    hunk: None,
                });
                i += 1;
            }
            RowKind::Removed | RowKind::Added => {
                let removed_end = run_end(rows, i, RowKind::Removed);
                let added_end = run_end(rows, removed_end, RowKind::Added);
                let removed: Vec<usize> = (i..removed_end).collect();
                let added: Vec<usize> = (removed_end..added_end).collect();
                for k in 0..removed.len().max(added.len()) {
                    out.push(SplitRow {
                        left: removed.get(k).copied(),
                        right: added.get(k).copied(),
                        hunk: None,
                    });
                }
                i = added_end;
            }
        }
    }
    out
}

fn run_end(rows: &[Row], from: usize, kind: RowKind) -> usize {
    rows[from..]
        .iter()
        .position(|r| r.kind != kind)
        .map_or(rows.len(), |n| from + n)
}

/// The changed part of a paired line: byte ranges in `old` and `new` between their common
/// prefix and suffix (on char boundaries). `None` when the lines are equal or share nothing.
pub fn intraline(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)> {
    if old == new {
        return None;
    }
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let (old_rest, new_rest) = (&old[prefix..], &new[prefix..]);
    let suffix: usize = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    if prefix == 0 && suffix == 0 {
        return None;
    }
    Some((prefix..old.len() - suffix, prefix..new.len() - suffix))
}

/// One side of a patch as text, for highlighting: line `i` of `text` is `rows[i]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideText {
    pub text: String,
    pub rows: Vec<usize>,
}

/// Left = context + removed lines (the old file's pieces); Right = context + added.
pub fn side_text(rows: &[Row], side: Side) -> SideText {
    let mut lines = Vec::new();
    let mut indices = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let on_side = match side {
            Side::Left => matches!(row.kind, RowKind::Context | RowKind::Removed),
            Side::Right => matches!(row.kind, RowKind::Context | RowKind::Added),
        };
        if on_side {
            lines.push(row.text.as_str());
            indices.push(i);
        }
    }
    SideText {
        text: lines.join("\n"),
        rows: indices,
    }
}

/// Where a click on this row comments in the Unified view: removed lines on the base side,
/// added and unchanged lines on the head side.
pub fn anchor_of(row: &Row) -> Option<(Side, u32)> {
    match row.kind {
        RowKind::Hunk => None,
        RowKind::Removed => row.old.map(|l| (Side::Left, l)),
        RowKind::Added | RowKind::Context => row.new.map(|l| (Side::Right, l)),
    }
}

/// The row's line number on `side` (Split view columns): Left for removed and context rows,
/// Right for added and context rows.
pub fn line_on(row: &Row, side: Side) -> Option<u32> {
    match (side, row.kind) {
        (Side::Left, RowKind::Removed | RowKind::Context) => row.old,
        (Side::Right, RowKind::Added | RowKind::Context) => row.new,
        _ => None,
    }
}

/// The row showing `line` on `side` (where a thread or a draft comment is drawn).
pub fn row_of(rows: &[Row], side: Side, line: u32) -> Option<usize> {
    rows.iter().position(|r| line_on(r, side) == Some(line))
}

/// Whether clicking this row (Unified) may open the comment editor: exactly
/// `clusia_core::can_comment` for its anchor.
pub fn commentable(patch: &str, row: &Row) -> bool {
    anchor_of(row).is_some_and(|(side, line)| can_comment(patch, side, line))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clusia_core::commentable_lines;

    use super::*;

    /// The hunk of the Review mockup (`src/auth/refresh.rs`).
    const PATCH: &str = "\
@@ -38,9 +38,11 @@ impl TokenStore {
     pub async fn refresh(&self) -> Result<Token, AuthError> {
-        let token = self.load()?;
-        if token.expires_at > now() {
+        let token = self.load().await?;
+        // Refresh one minute early so a request never races the expiry.
+        if token.expires_at > now() + Duration::from_secs(60) {
             return Ok(token);
         }
+        let _guard = self.refresh_lock.lock().await;
         let fresh = self.client.exchange(&token.refresh).await?;
         self.save(&fresh)?;
         Ok(fresh)
     }
@@ -60,4 +62,9 @@ impl TokenStore {
     fn expired(&self, token: &Token) -> bool {
         token.expires_at <= now()
     }
+
+    /// Drops the cached token so the next call refreshes it.
+    pub fn invalidate(&self) {
+        self.cache.lock().take();
+    }
 }
\\ No newline at end of file
";

    fn row(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> Row {
        Row {
            kind,
            old,
            new,
            text: text.into(),
        }
    }

    #[test]
    fn parses_numbers_and_markers() {
        let rows = parse_patch(PATCH);
        assert_eq!(rows.len(), 24, "the no-newline marker is dropped");
        assert_eq!(
            rows[0],
            row(
                RowKind::Hunk,
                None,
                None,
                "@@ -38,9 +38,11 @@ impl TokenStore {"
            )
        );
        assert_eq!(
            rows[1],
            row(
                RowKind::Context,
                Some(38),
                Some(38),
                "    pub async fn refresh(&self) -> Result<Token, AuthError> {"
            )
        );
        assert_eq!(
            rows[2],
            row(
                RowKind::Removed,
                Some(39),
                None,
                "        let token = self.load()?;"
            )
        );
        assert_eq!((rows[4].kind, rows[4].new), (RowKind::Added, Some(39)));
        assert_eq!((rows[6].kind, rows[6].new), (RowKind::Added, Some(41)));
        assert_eq!(
            (rows[7].kind, rows[7].old, rows[7].new),
            (RowKind::Context, Some(41), Some(42))
        );
        assert_eq!(
            (rows[9].kind, rows[9].new, rows[9].text.as_str()),
            (
                RowKind::Added,
                Some(44),
                "        let _guard = self.refresh_lock.lock().await;"
            )
        );
        assert_eq!(
            (rows[13].kind, rows[13].old, rows[13].new),
            (RowKind::Context, Some(46), Some(48))
        );
        assert_eq!(rows[14].kind, RowKind::Hunk);
        assert_eq!((rows[15].old, rows[15].new), (Some(60), Some(62)));
        assert_eq!(
            (rows[18].kind, rows[18].new, rows[18].text.as_str()),
            (RowKind::Added, Some(65), "")
        );
        assert_eq!(
            (rows[23].kind, rows[23].old, rows[23].new),
            (RowKind::Context, Some(63), Some(70))
        );
        let added = rows.iter().filter(|r| r.kind == RowKind::Added).count();
        let removed = rows.iter().filter(|r| r.kind == RowKind::Removed).count();
        assert_eq!((added, removed), (9, 2));
        assert!(parse_patch("").is_empty());
    }

    #[test]
    fn split_pairs_runs_and_leaves_extras_alone() {
        let rows = parse_patch(PATCH);
        let split = split_rows(&rows);
        let pair = |left, right| SplitRow {
            left,
            right,
            hunk: None,
        };
        assert_eq!(
            split[0],
            SplitRow {
                left: None,
                right: None,
                hunk: Some(0)
            }
        );
        assert_eq!(split[1], pair(Some(1), Some(1)));
        assert_eq!(
            split[2],
            pair(Some(2), Some(4)),
            "first removed with first added"
        );
        assert_eq!(split[3], pair(Some(3), Some(5)));
        assert_eq!(
            split[4],
            pair(None, Some(6)),
            "the extra added line stands alone"
        );
        assert_eq!(split[5], pair(Some(7), Some(7)));
        assert_eq!(
            split[7],
            pair(None, Some(9)),
            "an insertion has no left side"
        );
        assert_eq!(split.len(), 22);
        let only_removed = parse_patch("@@ -1,2 +1,0 @@\n-a\n-b\n");
        assert_eq!(
            split_rows(&only_removed)[1..],
            [pair(Some(1), None), pair(Some(2), None)]
        );
    }

    #[test]
    fn intraline_marks_the_changed_middle() {
        let old = "        let token = self.load()?;";
        let new = "        let token = self.load().await?;";
        let (o, n) = intraline(old, new).unwrap();
        assert_eq!(&old[o.clone()], "");
        assert_eq!(&new[n], ".await");
        assert_eq!(o, 31..31);
        let (o, n) = intraline("if a > now() {", "if a > now() + d {").unwrap();
        assert_eq!(("if a > now() {"[o].to_string()), "");
        assert_eq!(&"if a > now() + d {"[n], "+ d ");
        assert_eq!(intraline("same", "same"), None);
        assert_eq!(intraline("abc", "xyz"), None, "nothing in common");
        let (o, n) = intraline("ação = 1", "ação = 2").unwrap();
        assert_eq!((o, n), (9..10, 9..10), "byte offsets on char boundaries");
        assert_eq!(
            intraline("é", "è"),
            None,
            "a shared first byte is not a shared char"
        );
        assert_eq!(intraline("aa", "aaa"), Some((2..2, 2..3)));
    }

    #[test]
    fn side_texts_rebuild_each_version() {
        let rows = parse_patch(PATCH);
        let left = side_text(&rows, Side::Left);
        let right = side_text(&rows, Side::Right);
        assert_eq!(left.rows.len(), left.text.split('\n').count());
        assert_eq!(right.rows.len(), right.text.split('\n').count());
        assert!(left.rows.iter().all(|&i| rows[i].kind != RowKind::Added));
        assert!(right.rows.iter().all(|&i| rows[i].kind != RowKind::Removed));
        let lines: Vec<&str> = right.text.split('\n').collect();
        for (k, &i) in right.rows.iter().enumerate() {
            assert_eq!(lines[k], rows[i].text);
        }
        assert_eq!(right.rows.len(), 9 + 11);
        assert_eq!(left.rows.len(), 9 + 4);
    }

    #[test]
    fn anchors_and_placement() {
        let rows = parse_patch(PATCH);
        assert_eq!(anchor_of(&rows[0]), None);
        assert_eq!(anchor_of(&rows[1]), Some((Side::Right, 38)));
        assert_eq!(anchor_of(&rows[2]), Some((Side::Left, 39)));
        assert_eq!(anchor_of(&rows[4]), Some((Side::Right, 39)));
        assert_eq!(row_of(&rows, Side::Right, 41), Some(6));
        assert_eq!(row_of(&rows, Side::Right, 44), Some(9));
        assert_eq!(row_of(&rows, Side::Left, 39), Some(2));
        assert_eq!(
            row_of(&rows, Side::Left, 41),
            Some(7),
            "context rows have both sides"
        );
        assert_eq!(row_of(&rows, Side::Right, 50), None);
        assert_eq!(line_on(&rows[1], Side::Left), Some(38));
        assert_eq!(line_on(&rows[2], Side::Right), None);
    }

    #[test]
    fn commentable_matches_core() {
        let patches = [
            PATCH,
            "@@ -0,0 +1,3 @@\n+a\n+b\n+c\n",
            "@@ -1,3 +0,0 @@\n-a\n-b\n-c\n",
            "@@ -10 +10 @@\n-x\n+y\n@@ -20,2 +20,3 @@ fn f() {\n a\n+b\n c\n",
            "@@ bogus @@\n+a\n",
            "@@ -5 +5 @@\n a\n@@ -1,x +9 @@\n+b\n",
            "",
        ];
        for patch in patches {
            let rows = parse_patch(patch);
            for r in &rows {
                let want = anchor_of(r).is_some_and(|(s, l)| can_comment(patch, s, l));
                assert_eq!(commentable(patch, r), want, "{patch:?} {r:?}");
                if r.kind != RowKind::Hunk {
                    assert!(
                        commentable(patch, r),
                        "every code row is commentable: {r:?}"
                    );
                }
            }
            let (left, right) = commentable_lines(patch);
            let offered =
                |side| -> BTreeSet<u32> { rows.iter().filter_map(|r| line_on(r, side)).collect() };
            assert_eq!(offered(Side::Left), left, "{patch:?}");
            assert_eq!(offered(Side::Right), right, "{patch:?}");
        }
    }
}
