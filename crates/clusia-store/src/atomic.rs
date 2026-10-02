//! Crash-safe file replacement.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path` so readers see either the old or the new content, never a mix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Moves an unreadable file aside as `<name>.corrupt-<unix_ts>` and returns its new path.
pub fn quarantine(path: &Path, unix_ts: i64) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let target = path.with_file_name(format!("{name}.corrupt-{unix_ts}"));
    fs::rename(path, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a/b/file.json");
        write_atomic(&target, b"hello").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello");
    }

    #[test]
    fn replaces_existing_content_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        write_atomic(&target, b"one").unwrap();
        write_atomic(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("file.txt")]);
    }

    #[test]
    fn quarantine_renames_with_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        fs::write(&target, "garbage").unwrap();
        let moved = quarantine(&target, 1_700_000_000).unwrap();
        assert_eq!(moved, dir.path().join("config.toml.corrupt-1700000000"));
        assert!(!target.exists());
        assert_eq!(fs::read_to_string(moved).unwrap(), "garbage");
    }
}
