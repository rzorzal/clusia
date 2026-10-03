//! `activity.jsonl`: append-only log of what the user did.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};

use clusia_core::{Activity, Paths};

pub fn append_activity(paths: &Paths, activity: &Activity) -> io::Result<()> {
    let path = paths.activity_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(activity).map_err(io::Error::other)?;
    line.push(b'\n');
    // One write per line with O_APPEND keeps concurrent appends from interleaving.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)
}

pub fn read_activity(paths: &Paths) -> io::Result<(Vec<Activity>, usize)> {
    let text = match fs::read(paths.activity_file()) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let mut skipped = 0;
    let mut out = Vec::new();
    for line in text
        .split(|&b| b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
    {
        match serde_json::from_slice::<Activity>(line) {
            Ok(a) => out.push(a),
            Err(_) => skipped += 1,
        }
    }
    Ok((out, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::ActivityKind;

    fn activity(ts: i64, kind: ActivityKind) -> Activity {
        Activity {
            ts,
            kind,
            pr: "acme/widgets#7".parse().unwrap(),
            client: "test".into(),
            url: None,
            note: None,
        }
    }

    #[test]
    fn append_then_read_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        assert_eq!(read_activity(&p).unwrap(), (vec![], 0));
        append_activity(&p, &activity(1, ActivityKind::ReviewOpened)).unwrap();
        append_activity(&p, &activity(2, ActivityKind::ReviewPublished)).unwrap();
        let (got, skipped) = read_activity(&p).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(got.iter().map(|a| a.ts).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn bad_activity_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        append_activity(&p, &activity(1, ActivityKind::ItemAdded)).unwrap();
        let mut f = OpenOptions::new()
            .append(true)
            .open(p.activity_file())
            .unwrap();
        f.write_all(b"garbage\n\n{\"ts\":\"x\"}\n").unwrap();
        append_activity(&p, &activity(3, ActivityKind::ItemAdded)).unwrap();
        let (got, skipped) = read_activity(&p).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(skipped, 2, "blank lines are not counted");
    }

    #[test]
    fn non_utf8_activity_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        append_activity(&p, &activity(1, ActivityKind::ItemAdded)).unwrap();
        let mut f = OpenOptions::new()
            .append(true)
            .open(p.activity_file())
            .unwrap();
        f.write_all(b"\xff\xfe garbage\n").unwrap();
        append_activity(&p, &activity(3, ActivityKind::ItemAdded)).unwrap();
        let (got, skipped) = read_activity(&p).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(skipped, 1);
    }
}
