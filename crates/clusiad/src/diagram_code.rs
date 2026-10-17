//! The code a diagram box or a place of the keyword map points at, for the node modal: the lines
//! around its anchor as they were at the commit the diagram was drawn from.

use std::path::Path;

use clusia_core::diagram::{CodeAnchor, DiagramVersion, EntryState};
use clusia_core::keywords::{KEYWORDS_KEY, parse_place_node};
use clusia_core::printable::printable_lines;
use clusia_core::{PrRef, added_lines};
use clusia_git::{file_at, is_object_id, repo_of_worktree};
use clusia_protocol::{CodeLine, DiagramCode, ErrorCode, Outcome, ProtocolError, Reply};

use crate::diagrams::stored_version;
use crate::reviews;
use crate::state::Shared;

/// Lines shown after the anchor; the window starts on its first line, as the header says.
const AFTER: u32 = 2;
/// The most lines one answer holds.
const MAX_LINES: u32 = 200;
/// The longest line kept, in characters.
const LINE_MAX: usize = 1_000;
/// The biggest file the modal reads.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn not_found(message: String) -> Outcome {
    Outcome::Err(ProtocolError::new(ErrorCode::NotFound, message))
}

/// The commit of version `number` and the anchor of `node` (a diagram box or a place of the
/// keyword map) of diagram `key` in it; the anchor is `None` for a box that is not tied to code. Whatever the client says about lines is
/// never read: only what the daemon stored counts.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(crate) fn anchored_node(
    shared: &Shared,
    pr: &PrRef,
    number: u32,
    key: &str,
    node: &str,
) -> Result<(String, Option<CodeAnchor>), Outcome> {
    let Some(version) = stored_version(shared, pr, number) else {
        return Err(not_found(format!(
            "there is no version {number} of the diagrams"
        )));
    };
    Ok((
        version.commit.clone(),
        find_anchor(&version, number, key, node)?,
    ))
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn find_anchor(
    version: &DiagramVersion,
    number: u32,
    key: &str,
    node: &str,
) -> Result<Option<CodeAnchor>, Outcome> {
    let Some(entry) = version.entry(key) else {
        return Err(not_found(format!(
            "there is no diagram {key} in version {number}"
        )));
    };
    match &entry.state {
        EntryState::Ready { spec, .. } => match spec.nodes.iter().find(|n| n.id == node) {
            Some(found) => Ok(found.anchor.clone()),
            None => Err(not_found(format!(
                "there is no box {node} in diagram {key}"
            ))),
        },
        EntryState::ReadyMap { map, .. } => {
            match parse_place_node(node).and_then(|i| map.anchor(i)) {
                Some(anchor) => Ok(Some(anchor)),
                None => Err(not_found(format!(
                    "there is no place {node} in diagram {key}"
                ))),
            }
        }
        _ => Err(reviews::invalid_state("That diagram was not drawn")),
    }
}

fn seven(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// A plain relative path: no leading `/`, no empty, `.` or `..` part, no control character.
fn plain(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

/// `relative` of the worktree `root`, when it is a regular file that really lies inside it (a
/// link out of the worktree is not followed) and is small text.
fn read_in_worktree(root: &Path, relative: &str) -> Option<String> {
    if !plain(relative) {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let path = root.join(relative).canonicalize().ok()?;
    if !path.starts_with(&root) {
        return None;
    }
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    // Lossy, like `file_at`, so a file reads the same at the head and at an older commit.
    let text = String::from_utf8_lossy(&std::fs::read(&path).ok()?).into_owned();
    (!text.contains('\0')).then_some(text)
}

/// The lines `start ..= end + AFTER` of `text`, clamped to the file and to `MAX_LINES`:
/// the number of the first and the lines. Empty when the window lies past the end.
fn window(text: &str, start: u32, end: u32) -> (u32, Vec<String>) {
    let first = start.max(1);
    let last = end
        .saturating_add(AFTER)
        .min(first.saturating_add(MAX_LINES - 1));
    let lines = text
        .lines()
        .skip(first as usize - 1)
        .take((last - first + 1) as usize)
        .map(|line| {
            let kept: String = line.chars().take(LINE_MAX).collect();
            let shown = printable_lines(&kept);
            if line.chars().count() > LINE_MAX {
                format!("{shown}…")
            } else {
                shown
            }
        })
        .collect();
    (first, lines)
}

fn unavailable(anchor: &CodeAnchor, commit: &str, why: String) -> Outcome {
    Outcome::Ok(Reply::DiagramCode(DiagramCode {
        path: anchor.path.clone(),
        commit: commit.to_string(),
        start_line: anchor.start_line,
        lines: Vec::new(),
        unavailable: Some(why),
    }))
}

/// `ReadDiagramCode`. A place of the keyword map gets one line before it. A version at the review's head is read from the worktree, and its lines
/// the pull request adds are marked; an older one is read from its pinned commit, with no line
/// marked. Code that cannot be read comes back as `unavailable`, not as an error.
pub(crate) async fn read(
    shared: &Shared,
    pr: &PrRef,
    number: u32,
    key: &str,
    node: &str,
) -> Outcome {
    let (commit, anchor) = match anchored_node(shared, pr, number, key, node) {
        Ok(found) => found,
        Err(out) => return out,
    };
    let Some(anchor) = anchor else {
        return reviews::bad_request("This box is not tied to code");
    };
    let head = match reviews::load_stored(shared, pr) {
        Ok(Some(review)) => review.head_sha,
        Ok(None) => return not_found(format!("no review for {pr}; open it first")),
        Err(out) => return out,
    };
    let at_head = commit == head;
    let worktree = shared.paths.worktree_for(pr);
    let text = if at_head {
        let (root, path) = (worktree.clone(), anchor.path.clone());
        tokio::task::spawn_blocking(move || read_in_worktree(&root, &path))
            .await
            .ok()
            .flatten()
    } else if is_object_id(&commit) {
        match repo_of_worktree(&worktree).await {
            Ok(repo) => file_at(&repo, &commit, &anchor.path, MAX_FILE_BYTES)
                .await
                .ok(),
            Err(_) => None,
        }
    } else {
        None
    };
    let Some(text) = text else {
        return unavailable(
            &anchor,
            &commit,
            format!("The code at {} is no longer available.", seven(&commit)),
        );
    };
    // A place is one line: one line of context above it, so it reads in place.
    let before = u32::from(key == KEYWORDS_KEY && parse_place_node(node).is_some());
    let (first, lines) = window(
        &text,
        anchor.start_line.saturating_sub(before).max(1),
        anchor.end_line,
    );
    if lines.is_empty() {
        return unavailable(
            &anchor,
            &commit,
            format!("These lines are not in the file at {}.", seven(&commit)),
        );
    }
    // Only the box's own lines are tinted: the context after them is not what it stands for.
    let changed = if at_head {
        match reviews::files_for(shared, pr, &head).await {
            Ok(files) => files
                .iter()
                .find(|f| f.path == anchor.path)
                .and_then(|f| f.patch.as_deref())
                .map(added_lines)
                .unwrap_or_default(),
            Err(_) => Default::default(),
        }
    } else {
        Default::default()
    };
    let lines = (first..)
        .zip(lines)
        .map(|(number, text)| CodeLine {
            number,
            changed: (anchor.start_line..=anchor.end_line).contains(&number)
                && changed.contains(&number),
            text,
        })
        .collect();
    Outcome::Ok(Reply::DiagramCode(DiagramCode {
        path: anchor.path,
        commit,
        start_line: first,
        lines,
        unavailable: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_starts_at_the_anchor_and_ends_two_lines_after_it_clamped_to_the_file() {
        let text = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (first, lines) = window(&text, 10, 12);
        assert_eq!(first, 10);
        assert_eq!((lines[0].as_str(), lines.len()), ("line 10", 5));
        assert_eq!(lines[4], "line 14");
        let (first, lines) = window(&text, 1, 2);
        assert_eq!((first, lines.len()), (1, 4));
        let (first, lines) = window(&text, 19, 20);
        assert_eq!((first, lines.len(), lines[1].as_str()), (19, 2, "line 20"));
        let (_, past) = window(&text, 40, 41);
        assert!(past.is_empty());
    }

    #[test]
    fn the_window_holds_at_most_two_hundred_lines_and_short_lines() {
        let text = (1..=500)
            .map(|_| "x".repeat(2_000))
            .collect::<Vec<_>>()
            .join("\n");
        let (first, lines) = window(&text, 10, 400);
        assert_eq!((first, lines.len()), (10, 200));
        assert_eq!(lines[0].chars().count(), LINE_MAX + 1);
        assert!(lines[0].ends_with('…'));
    }

    #[test]
    fn the_window_shows_controls_and_bidi_as_escapes_and_keeps_tabs() {
        let (_, lines) = window("a\u{1b}[2J\tb\u{202e}c\r\nd", 1, 1);
        assert_eq!(lines[0], "a\\u{1b}[2J\tb\\u{202e}c");
        assert_eq!(lines[1], "d");
    }

    #[test]
    fn a_path_is_plain_or_refused() {
        assert!(plain("src/auth/refresh.rs"));
        for bad in [
            "",
            "/etc/passwd",
            "../a",
            "a/../b",
            "./a",
            "a//b",
            "a\\b",
            "a\nb",
        ] {
            assert!(!plain(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_worktree_is_read_only_inside_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wt");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "outside").unwrap();
        std::os::unix::fs::symlink("../secret.txt", root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("src/a.rs", root.join("inside.txt")).unwrap();
        std::fs::write(root.join("bin.dat"), b"a\0b").unwrap();
        std::fs::write(root.join("latin.txt"), b"caf\xe9\n").unwrap();
        std::fs::write(
            root.join("big.txt"),
            vec![b'x'; MAX_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            read_in_worktree(&root, "src/a.rs").as_deref(),
            Some("one\ntwo\n")
        );
        assert_eq!(
            read_in_worktree(&root, "inside.txt").as_deref(),
            Some("one\ntwo\n"),
            "a link that stays inside is fine"
        );
        assert_eq!(
            read_in_worktree(&root, "latin.txt").as_deref(),
            Some("caf\u{fffd}\n"),
            "text that is not UTF-8 is shown with replacement characters, as at an older commit"
        );
        for refused in [
            "link.txt",
            "../secret.txt",
            "src",
            "missing",
            "bin.dat",
            "big.txt",
        ] {
            assert_eq!(read_in_worktree(&root, refused), None, "{refused}");
        }
    }
}
