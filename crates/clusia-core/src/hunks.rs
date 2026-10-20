//! A pure parser for `git diff -U0 -M` text: which lines of which files changed, with no
//! agent, no git process and no network.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
}

/// One file of the diff: `old_path` is `None` for an added file, `new_path` for a deleted one.
/// A binary file, a mode-only change or a pure rename keeps its entry with no hunks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHunks {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub hunks: Vec<Hunk>,
}

/// Head-side lines of one hunk; a deleted file's hunks carry its old lines.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HunkRef {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `@@ -a[,b] +c[,d] @@ …`
pub(crate) fn parse_hunk_header(line: &str) -> Option<Hunk> {
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
pub(crate) fn split_git_paths(rest: &str) -> (String, String) {
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

/// Reads one git-quoted token (`"…"` with C escapes and octal bytes) from the start of `s`,
/// and returns the decoded text with what follows the closing quote.
fn unquote(s: &str) -> Option<(String, &str)> {
    let body = s.strip_prefix('"')?;
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                return Some((String::from_utf8_lossy(&out).into_owned(), &body[i + 1..]));
            }
            b'\\' if i + 1 < bytes.len() => {
                i += 1;
                match bytes[i] {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'v' => out.push(11),
                    d @ b'0'..=b'7' => {
                        let mut value = u32::from(d - b'0');
                        let mut digits = 1;
                        while digits < 3
                            && i + 1 < bytes.len()
                            && matches!(bytes[i + 1], b'0'..=b'7')
                        {
                            i += 1;
                            value = value * 8 + u32::from(bytes[i] - b'0');
                            digits += 1;
                        }
                        out.push((value & 0xff) as u8);
                    }
                    other => out.push(other),
                }
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

/// A path token of a `---`, `+++` or `rename` line: quoted or plain.
fn path_token(s: &str) -> String {
    match unquote(s) {
        Some((decoded, _)) => decoded,
        None => s.trim_end_matches('\t').to_string(),
    }
}

/// The two path tokens of a `diff --git` line, each plain or quoted, without `a/` and `b/`.
fn header_paths(rest: &str) -> (String, String) {
    if let Some((old, after)) = unquote(rest) {
        let new = path_token(after.trim_start());
        return (strip_side(&old, "a/"), strip_side(&new, "b/"));
    }
    if rest.ends_with('"')
        && let Some(at) = rest.rfind(" \"")
        && let Some((new, _)) = unquote(&rest[at + 1..])
    {
        return (strip_side(&rest[..at], "a/"), strip_side(&new, "b/"));
    }
    split_git_paths(rest)
}

fn strip_side(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

/// The path of a `---` / `+++` line; `/dev/null` is no path.
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    let path = path_token(rest);
    if path == "/dev/null" {
        None
    } else {
        Some(strip_side(&path, prefix))
    }
}

struct Pending {
    old: Option<String>,
    new: Option<String>,
    added: bool,
    deleted: bool,
    renamed: bool,
    in_hunks: bool,
    hunks: Vec<Hunk>,
}

impl Pending {
    fn finish(self) -> FileHunks {
        let status = if self.added {
            FileStatus::Added
        } else if self.deleted {
            FileStatus::Deleted
        } else if self.renamed {
            FileStatus::Renamed
        } else {
            FileStatus::Modified
        };
        FileHunks {
            old_path: if self.added { None } else { self.old },
            new_path: if self.deleted { None } else { self.new },
            status,
            hunks: self.hunks,
        }
    }
}

/// Reads `git diff -U0 -M` text. Lines before the first `diff --git` and `\ No newline`
/// markers are ignored.
pub fn parse_hunks(diff: &str) -> Vec<FileHunks> {
    let mut files = Vec::new();
    let mut current: Option<Pending> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(p) = current.take() {
                files.push(p.finish());
            }
            let (old, new) = header_paths(rest);
            current = Some(Pending {
                old: Some(old),
                new: Some(new),
                added: false,
                deleted: false,
                renamed: false,
                in_hunks: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(p) = current.as_mut() else { continue };
        if line.starts_with("@@ ") {
            p.in_hunks = true;
            if let Some(h) = parse_hunk_header(line) {
                p.hunks.push(h);
            }
        } else if p.in_hunks {
            // A body line such as `--- x` (a removed `-- x`) is content, not a header.
        } else if line.starts_with("new file mode") {
            p.added = true;
        } else if line.starts_with("deleted file mode") {
            p.deleted = true;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            p.old = Some(path_token(path));
            p.renamed = true;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            p.new = Some(path_token(path));
            p.renamed = true;
        } else if let Some(path) = line.strip_prefix("--- ") {
            p.old = side_path(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            p.new = side_path(path, "b/");
        }
    }
    if let Some(p) = current.take() {
        files.push(p.finish());
    }
    files
}

/// One hunk's head-side span. A hunk that only removed lines points at the line they were
/// removed before (1 at the top of a file); a deleted file uses its old lines, and one of its
/// hunks with none gives `None`.
pub fn hunk_ref(file: &FileHunks, hunk: &Hunk) -> Option<HunkRef> {
    if file.status == FileStatus::Deleted {
        let path = file.old_path.clone()?;
        if hunk.old_len == 0 {
            return None;
        }
        return Some(HunkRef {
            path,
            start_line: hunk.old_start,
            end_line: hunk.old_start + hunk.old_len - 1,
        });
    }
    let path = file.new_path.clone()?;
    let (start_line, end_line) = if hunk.new_len == 0 {
        let at = hunk.new_start.max(1);
        (at, at)
    } else {
        (hunk.new_start, hunk.new_start + hunk.new_len - 1)
    };
    Some(HunkRef {
        path,
        start_line,
        end_line,
    })
}

/// Every file's hunks in order, through [`hunk_ref`].
pub fn hunk_refs(files: &[FileHunks]) -> Vec<HunkRef> {
    files
        .iter()
        .flat_map(|f| f.hunks.iter().filter_map(|h| hunk_ref(f, h)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = r#"diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -3,2 +3,3 @@ fn a() {
-old
-old2
+new
+new2
+new3
@@ -10 +10,0 @@
-gone
@@ -20,0 +21,2 @@
+x
+y
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1,4 @@
+a
+b
+c
+d
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 4444444..0000000
--- a/old.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
diff --git a/src/a.rs b/src/b.rs
similarity index 90%
rename from src/a.rs
rename to src/b.rs
index 5555555..6666666 100644
--- a/src/a.rs
+++ b/src/b.rs
@@ -7 +7 @@ fn z
-before
+after
diff --git a/bin/run.sh b/bin/run.sh
old mode 100644
new mode 100755
"#;

    fn h(old_start: u32, old_len: u32, new_start: u32, new_len: u32) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
        }
    }

    fn some(p: &str) -> Option<String> {
        Some(p.to_string())
    }

    #[test]
    fn every_status_keeps_its_paths_and_hunks() {
        let files = parse_hunks(DIFF);
        assert_eq!(
            files,
            vec![
                FileHunks {
                    old_path: some("src/lib.rs"),
                    new_path: some("src/lib.rs"),
                    status: FileStatus::Modified,
                    hunks: vec![h(3, 2, 3, 3), h(10, 1, 10, 0), h(20, 0, 21, 2)],
                },
                FileHunks {
                    old_path: None,
                    new_path: some("docs/new.md"),
                    status: FileStatus::Added,
                    hunks: vec![h(0, 0, 1, 4)]
                },
                FileHunks {
                    old_path: some("old.txt"),
                    new_path: None,
                    status: FileStatus::Deleted,
                    hunks: vec![h(1, 2, 0, 0)]
                },
                FileHunks {
                    old_path: some("src/a.rs"),
                    new_path: some("src/b.rs"),
                    status: FileStatus::Renamed,
                    hunks: vec![h(7, 1, 7, 1)]
                },
                FileHunks {
                    old_path: some("bin/run.sh"),
                    new_path: some("bin/run.sh"),
                    status: FileStatus::Modified,
                    hunks: vec![]
                },
            ]
        );
    }

    #[test]
    fn hunk_refs_are_head_side_and_a_deleted_file_uses_its_old_lines() {
        let r = |p: &str, a: u32, b: u32| HunkRef {
            path: p.to_string(),
            start_line: a,
            end_line: b,
        };
        assert_eq!(
            hunk_refs(&parse_hunks(DIFF)),
            vec![
                r("src/lib.rs", 3, 5),
                r("src/lib.rs", 10, 10),
                r("src/lib.rs", 21, 22),
                r("docs/new.md", 1, 4),
                r("old.txt", 1, 2),
                r("src/b.rs", 7, 7),
            ]
        );
    }

    #[test]
    fn lines_removed_at_the_top_of_a_file_point_at_line_one() {
        let files = parse_hunks(
            "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +0,0 @@\n-a\n-b\n",
        );
        assert_eq!(files[0].hunks, vec![h(1, 2, 0, 0)]);
        assert_eq!(
            hunk_refs(&files),
            vec![HunkRef {
                path: "x.rs".into(),
                start_line: 1,
                end_line: 1
            }]
        );
    }

    #[test]
    fn quoted_and_spaced_paths_are_decoded() {
        let quoted = "diff --git \"a/caf\\303\\251.rs\" \"b/caf\\303\\251.rs\"\n--- \"a/caf\\303\\251.rs\"\n+++ \"b/caf\\303\\251.rs\"\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse_hunks(quoted);
        assert_eq!(
            (files[0].old_path.clone(), files[0].new_path.clone()),
            (some("café.rs"), some("café.rs"))
        );
        let spaced = "diff --git a/my file.rs b/my file.rs\n--- a/my file.rs\t\n+++ b/my file.rs\t\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(parse_hunks(spaced)[0].new_path, some("my file.rs"));
    }

    #[test]
    fn binary_files_and_the_no_newline_marker_are_harmless() {
        let diff = "diff --git a/img.png b/img.png\nnew file mode 100644\nindex 0000000..1111111\nBinary files /dev/null and b/img.png differ\ndiff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n";
        let files = parse_hunks(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(
            (files[0].status, files[0].hunks.len()),
            (FileStatus::Added, 0)
        );
        assert_eq!(files[1].hunks, vec![h(1, 1, 1, 1)]);
    }

    #[test]
    fn empty_and_garbage_input_give_no_files() {
        assert!(parse_hunks("").is_empty());
        assert!(parse_hunks("not a diff\n@@ -1 +1 @@\n").is_empty());
    }

    #[test]
    fn a_status_is_written_in_snake_case() {
        assert_eq!(
            serde_json::to_value(FileStatus::Renamed).expect("json"),
            serde_json::json!("renamed")
        );
        let hunk = serde_json::to_value(h(1, 2, 3, 4)).expect("json");
        assert_eq!(
            hunk,
            serde_json::json!({ "old_start": 1, "old_len": 2, "new_start": 3, "new_len": 4 })
        );
    }
}
