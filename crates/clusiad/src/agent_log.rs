//! The agent log: every event of a review's chat, one JSON line each, so a window opened later
//! (or `clusia agent log`) can replay the conversation.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Mutex;

use clusia_protocol::AgentLogEntry;

/// The log keeps at most this many bytes; past it the oldest half is dropped.
pub(crate) const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Appends and trims never interleave.
static WRITING: Mutex<()> = Mutex::new(());

pub(crate) fn turn_of(entry: &AgentLogEntry) -> u64 {
    match entry {
        AgentLogEntry::User { turn, .. }
        | AgentLogEntry::Text { turn, .. }
        | AgentLogEntry::ToolUse { turn, .. }
        | AgentLogEntry::Denied { turn, .. }
        | AgentLogEntry::Suggestion { turn, .. }
        | AgentLogEntry::Done { turn, .. }
        | AgentLogEntry::Error { turn, .. }
        | AgentLogEntry::Permission { turn, .. }
        | AgentLogEntry::Check { turn, .. } => *turn,
    }
}

pub(crate) fn append(path: &Path, entry: &AgentLogEntry) -> io::Result<()> {
    let _writing = WRITING.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&line)?;
    if file.metadata()?.len() > MAX_BYTES {
        trim(path)?;
    }
    Ok(())
}

/// Drops the oldest half of the file, cutting at a line boundary. When the newest entry alone
/// is longer than half the file, everything before it goes and it stays. A check turn cut in
/// two keeps its first `Check` line, moved to the top, so the rest of that turn still reads as
/// a check's and never as the chat's.
fn trim(path: &Path) -> io::Result<()> {
    let bytes = fs::read(path)?;
    let middle = bytes.len() / 2;
    let mut start = bytes[middle..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(bytes.len(), |i| middle + i + 1);
    if start == bytes.len() {
        let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        start = body.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    }
    let temporary = path.with_extension("jsonl.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&check_lines_cut_off(&bytes[..start], &bytes[start..]))?;
    file.write_all(&bytes[start..])?;
    fs::rename(&temporary, path)
}

/// The first `Check` line in `dropped` of each turn that still has entries in `kept`.
fn check_lines_cut_off(dropped: &[u8], kept: &[u8]) -> Vec<u8> {
    let parse = |line: &[u8]| serde_json::from_slice::<AgentLogEntry>(line).ok();
    let kept_turns: std::collections::HashSet<u64> = kept
        .split(|b| *b == b'\n')
        .filter_map(parse)
        .map(|entry| turn_of(&entry))
        .collect();
    let mut moved = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in dropped.split(|b| *b == b'\n') {
        let Some(entry @ AgentLogEntry::Check { .. }) = parse(line) else {
            continue;
        };
        let turn = turn_of(&entry);
        if kept_turns.contains(&turn) && moved.insert(turn) {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    out
}

/// Every entry, oldest first. A missing file is an empty log; a line that does not parse
/// (a write cut short by a crash) is skipped.
pub(crate) fn read(path: &Path) -> Vec<AgentLogEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The highest turn id in the log, `0` when it is empty.
pub(crate) fn last_turn(path: &Path) -> u64 {
    read(path).iter().map(turn_of).max().unwrap_or(0)
}

/// The turns that belong to a check, not to the chat: those with a `Check` entry. Every entry
/// of such a turn (its tools, its permission lines, its end) stays out of the chat.
pub(crate) fn check_turns(entries: &[AgentLogEntry]) -> std::collections::HashSet<u64> {
    entries
        .iter()
        .filter(|entry| matches!(entry, AgentLogEntry::Check { .. }))
        .map(turn_of)
        .collect()
}

/// The turns that started but have no ending, oldest first: a chat turn ends with `Done` or
/// `Error`, a check's with a `Check` entry that is `done`, `failed` or `stopped`.
pub(crate) fn unfinished_turns(entries: &[AgentLogEntry]) -> Vec<u64> {
    let mut started = Vec::new();
    let mut ended = std::collections::HashSet::new();
    for entry in entries {
        let turn = turn_of(entry);
        if !started.contains(&turn) {
            started.push(turn);
        }
        let over = match entry {
            AgentLogEntry::Done { .. } | AgentLogEntry::Error { .. } => true,
            AgentLogEntry::Check { state, .. } => {
                matches!(state.as_str(), "done" | "failed" | "stopped")
            }
            _ => false,
        };
        if over {
            ended.insert(turn);
        }
    }
    started
        .into_iter()
        .filter(|turn| !ended.contains(turn))
        .collect()
}

/// What the agent said in the latest turn the daemon asked for itself (a turn with no `User`
/// entry and no `Check` entry): the running summary that goes into the review notes.
pub(crate) fn last_summary(entries: &[AgentLogEntry]) -> Option<String> {
    let checks = check_turns(entries);
    let asked: std::collections::HashSet<u64> = entries
        .iter()
        .filter(|e| matches!(e, AgentLogEntry::User { .. }))
        .map(turn_of)
        .collect();
    let turn = entries
        .iter()
        .filter(|e| matches!(e, AgentLogEntry::Text { .. }))
        .map(turn_of)
        .filter(|turn| !asked.contains(turn) && !checks.contains(turn))
        .max()?;
    let text: String = entries
        .iter()
        .filter_map(|e| match e {
            AgentLogEntry::Text { turn: t, text, .. } if *t == turn => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn text(turn: u64, body: &str) -> AgentLogEntry {
        AgentLogEntry::Text {
            at: 1,
            turn,
            text: body.into(),
        }
    }

    #[test]
    fn entries_come_back_in_order_and_the_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent/acme~widgets~7.jsonl");
        assert!(read(&path).is_empty(), "a missing log is empty");
        append(
            &path,
            &AgentLogEntry::User {
                at: 1,
                turn: 1,
                text: "hi".into(),
            },
        )
        .unwrap();
        append(&path, &text(1, "hello")).unwrap();
        append(
            &path,
            &AgentLogEntry::Done {
                at: 2,
                turn: 1,
                duration_ms: 5,
            },
        )
        .unwrap();
        let entries = read(&path);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1], text(1, "hello"));
        assert_eq!(last_turn(&path), 1);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o077,
            0
        );
    }

    #[test]
    fn a_torn_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        append(&path, &text(1, "kept")).unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"text\",\"at\":").unwrap();
        assert_eq!(read(&path), vec![text(1, "kept")]);
    }

    #[test]
    fn past_the_limit_the_oldest_half_goes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let chunk = "x".repeat(64 * 1024);
        for turn in 1..=40 {
            append(&path, &text(turn, &chunk)).unwrap();
        }
        let size = fs::metadata(&path).unwrap().len();
        assert!(size <= MAX_BYTES, "{size} bytes");
        let entries = read(&path);
        assert!(entries.len() < 40);
        assert_eq!(turn_of(entries.last().unwrap()), 40, "the newest stays");
        assert!(
            entries
                .iter()
                .all(|e| matches!(e, AgentLogEntry::Text { .. })),
            "the cut falls between lines"
        );
        assert!(turn_of(&entries[0]) > 1, "the oldest went");
    }

    #[test]
    fn an_entry_larger_than_half_the_limit_survives_the_trim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let chunk = "x".repeat(64 * 1024);
        for turn in 1..=16 {
            append(&path, &text(turn, &chunk)).unwrap();
        }
        // Longer than everything before it, so the middle of the file falls inside it.
        let big = "y".repeat(MAX_BYTES as usize / 2 + 128 * 1024);
        append(&path, &text(17, &big)).unwrap();
        let entries = read(&path);
        assert_eq!(entries.last(), Some(&text(17, &big)), "the newest stays");
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
    }

    #[test]
    fn a_check_turn_cut_by_the_trim_keeps_its_check_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let chunk = "x".repeat(64 * 1024);
        append(
            &path,
            &AgentLogEntry::Check {
                at: 1,
                turn: 1,
                kind: clusia_core::CheckKind::Security,
                state: "running".into(),
            },
        )
        .unwrap();
        for _ in 0..40 {
            append(
                &path,
                &AgentLogEntry::Permission {
                    at: 1,
                    turn: 1,
                    tool: "Bash".into(),
                    summary: chunk.clone(),
                    outcome: clusia_protocol::PermissionOutcome::Allowed,
                },
            )
            .unwrap();
        }
        let entries = read(&path);
        assert!(entries.len() < 41, "the log was trimmed");
        assert!(
            matches!(entries[0], AgentLogEntry::Check { turn: 1, .. }),
            "the check line moves to the top: {:?}",
            entries.first().map(turn_of)
        );
        assert_eq!(check_turns(&entries), [1].into_iter().collect());
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
    }

    #[test]
    fn a_check_turn_is_every_turn_with_a_check_entry() {
        let permission = |turn| AgentLogEntry::Permission {
            at: 1,
            turn,
            tool: "Bash".into(),
            summary: "make".into(),
            outcome: clusia_protocol::PermissionOutcome::Allowed,
        };
        let entries = [
            text(1, "chat"),
            permission(1),
            AgentLogEntry::Check {
                at: 1,
                turn: 2,
                kind: clusia_core::CheckKind::Security,
                state: "running".into(),
            },
            permission(2),
        ];
        assert_eq!(check_turns(&entries), [2].into_iter().collect());
        assert!(check_turns(&[]).is_empty());
    }

    #[test]
    fn a_check_turn_ends_with_done_failed_or_stopped() {
        let check = |turn: u64, state: &str| AgentLogEntry::Check {
            at: 1,
            turn,
            kind: clusia_core::CheckKind::Audit,
            state: state.into(),
        };
        let entries = [
            check(1, "waiting"),
            check(1, "running"),
            check(1, "done"),
            check(2, "running"),
            check(2, "failed"),
            check(3, "running"),
            check(3, "stopped"),
            check(4, "waiting"),
            check(4, "running"),
        ];
        assert_eq!(unfinished_turns(&entries), [4]);
        assert_eq!(turn_of(&entries[0]), 1);
        assert_eq!(last_turn_of(&entries), 4);
    }

    fn last_turn_of(entries: &[AgentLogEntry]) -> u64 {
        entries.iter().map(turn_of).max().unwrap_or(0)
    }

    #[test]
    fn unfinished_turns_are_those_without_an_ending() {
        let entry = |turn| AgentLogEntry::User {
            at: 1,
            turn,
            text: "q".into(),
        };
        let entries = [
            entry(1),
            text(1, "a"),
            AgentLogEntry::Done {
                at: 2,
                turn: 1,
                duration_ms: 1,
            },
            entry(2),
            text(2, "partial"),
            entry(3),
            AgentLogEntry::Error {
                at: 3,
                turn: 3,
                kind: clusia_protocol::AgentErrorKind::Crashed,
                message: "x".into(),
            },
            entry(4),
        ];
        assert_eq!(unfinished_turns(&entries), [2, 4]);
        assert!(unfinished_turns(&[]).is_empty());
    }

    #[test]
    fn the_summary_is_the_latest_turn_nobody_typed() {
        let user = |turn| AgentLogEntry::User {
            at: 1,
            turn,
            text: "q".into(),
        };
        let say = |turn, text: &str| AgentLogEntry::Text {
            at: 1,
            turn,
            text: text.into(),
        };
        assert_eq!(last_summary(&[]), None);
        assert_eq!(last_summary(&[user(1), say(1, "answer")]), None);
        let log = [
            say(1, "First summary. "),
            say(1, "Second part."),
            user(2),
            say(2, "An answer."),
            say(3, "Newer summary."),
        ];
        assert_eq!(last_summary(&log).as_deref(), Some("Newer summary."));
        assert_eq!(
            last_summary(&log[..4]).as_deref(),
            Some("First summary. Second part.")
        );
    }

    #[test]
    fn a_check_is_never_the_summary() {
        let say = |turn, text: &str| AgentLogEntry::Text {
            at: 1,
            turn,
            text: text.into(),
        };
        let log = [
            say(1, "The summary."),
            AgentLogEntry::Check {
                at: 2,
                turn: 2,
                kind: clusia_core::CheckKind::Audit,
                state: "running".into(),
            },
            say(2, "A finding block."),
        ];
        assert_eq!(last_summary(&log).as_deref(), Some("The summary."));
        assert_eq!(last_summary(&log[1..]), None);
    }
}
