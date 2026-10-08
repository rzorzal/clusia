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
        | AgentLogEntry::Error { turn, .. } => *turn,
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
/// is longer than half the file, everything before it goes and it stays.
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
    file.write_all(&bytes[start..])?;
    fs::rename(&temporary, path)
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

/// The turns that started but have neither a `Done` nor an `Error` entry, oldest first.
pub(crate) fn unfinished_turns(entries: &[AgentLogEntry]) -> Vec<u64> {
    let mut started = Vec::new();
    let mut ended = std::collections::HashSet::new();
    for entry in entries {
        let turn = turn_of(entry);
        if !started.contains(&turn) {
            started.push(turn);
        }
        if matches!(
            entry,
            AgentLogEntry::Done { .. } | AgentLogEntry::Error { .. }
        ) {
            ended.insert(turn);
        }
    }
    started
        .into_iter()
        .filter(|turn| !ended.contains(turn))
        .collect()
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
}
