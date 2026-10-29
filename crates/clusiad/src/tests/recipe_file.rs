//! The recipe file of a worktree: how it is read, whose it is and how it is written. Every
//! access goes through one descriptor of `.clusia`, opened without following a link, so
//! nothing swapped into the clone after a check is followed.
#![allow(dead_code)]

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use clusia_core::tests::RECIPE_MAX_BYTES;
use clusia_testrun::recipe::recipe_hash;
use clusia_testrun::scan::SHIPPED_CLUSIA;

const DIR: &str = ".clusia";
const FILE: &str = "recipe.toml";
const FILE_PATH: &str = ".clusia/recipe.toml";
/// Makes the temporary names of two writers of one process differ.
static TEMPORARIES: AtomicU64 = AtomicU64::new(0);

/// What to do with a recipe the agent just wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Adopt {
    /// Put it in the file: there is none, or the file is the one Clúsia last wrote.
    Write,
    /// Keep it as a proposal: the reviewer's own file stays as it is.
    Propose,
}

fn c_string(text: &[u8]) -> io::Result<CString> {
    CString::new(text).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn descriptor(fd: i32) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by `open` or `openat` and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `.clusia` itself, opened as a directory and never through a link.
fn open_dir(worktree: &Path) -> io::Result<OwnedFd> {
    let path = c_string(worktree.join(DIR).as_os_str().as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `path` is a valid C string that `open` only reads.
    descriptor(unsafe { libc::open(path.as_ptr(), flags) })
}

/// `name` inside `dir`, without following a link.
fn open_at(dir: &OwnedFd, name: &str, flags: i32, mode: libc::c_uint) -> io::Result<File> {
    let name = c_string(name.as_bytes())?;
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open descriptor and `name` a valid C string that `openat` only reads.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) };
    descriptor(fd).map(File::from)
}

/// An error that says the entry is a link or of another kind, not that it is missing.
fn not_ours(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EMLINK)
    )
}

/// The recipe file's bytes. `None` when there is no file. A link, a folder or a file of another
/// kind (a pipe, a device) at `.clusia` or `recipe.toml` is the pull request's and is refused.
/// The file is opened without blocking and without following a link, checked to be a regular
/// file on the descriptor itself, and read up to `RECIPE_MAX_BYTES`.
pub(crate) fn read_recipe(worktree: &Path) -> Result<Option<Vec<u8>>, String> {
    let dir = match open_dir(worktree) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if not_ours(&e) => return Err(SHIPPED_CLUSIA.to_string()),
        Err(e) => return Err(format!("cannot read {DIR}: {e}")),
    };
    let file = match open_at(&dir, FILE, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if not_ours(&e) => return Err(SHIPPED_CLUSIA.to_string()),
        Err(e) => return Err(format!("cannot read {FILE_PATH}: {e}")),
    };
    let unreadable = |e: io::Error| format!("cannot read {FILE_PATH}: {e}");
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err(SHIPPED_CLUSIA.to_string());
    }
    let cap = RECIPE_MAX_BYTES as u64;
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > cap {
        return Err(format!("{FILE_PATH} is larger than {} KiB", cap / 1024));
    }
    Ok(Some(bytes))
}

/// Whether a new recipe may replace the file: when there is no file, or its hash is the one
/// Clúsia wrote (`written`). A file the reviewer edited, or one Clúsia never wrote, is theirs.
pub(crate) fn adopt(file: Option<&[u8]>, written: Option<&str>) -> Adopt {
    match file {
        None => Adopt::Write,
        Some(bytes) if written == Some(recipe_hash(bytes).as_str()) => Adopt::Write,
        Some(_) => Adopt::Propose,
    }
}

/// Writes `text` as the recipe file, whole or not at all, and returns its hash. The folder is
/// the one descriptor of `.clusia`; the temporary file in it is new, has a name no other writer
/// shares and never follows a link; `renameat` puts it over `recipe.toml`, replacing whatever is
/// there (a link included) without writing through it.
pub(crate) fn write_recipe(worktree: &Path, text: &str) -> io::Result<String> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    let cap = RECIPE_MAX_BYTES as u64;
    if text.len() as u64 > cap {
        return Err(invalid(format!(
            "the recipe is larger than {} KiB",
            cap / 1024
        )));
    }
    let dir = match open_dir(worktree) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Another writer may have made it in the meantime; opening it below still refuses
            // a link or a file of another kind.
            match std::fs::create_dir(worktree.join(DIR)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            match open_dir(worktree) {
                Err(e) if not_ours(&e) => return Err(invalid(SHIPPED_CLUSIA.to_string())),
                opened => opened?,
            }
        }
        Err(e) if not_ours(&e) => return Err(invalid(SHIPPED_CLUSIA.to_string())),
        Err(e) => return Err(e),
    };
    let temp = format!(
        ".{FILE}.{}-{}.tmp",
        std::process::id(),
        TEMPORARIES.fetch_add(1, Ordering::Relaxed)
    );
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
    let written = open_at(&dir, &temp, flags, 0o600)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| {
            let (from, to) = (c_string(temp.as_bytes())?, c_string(FILE.as_bytes())?);
            let fd = dir.as_raw_fd();
            // SAFETY: `dir` is an open descriptor and both names are valid C strings that
            // `renameat` only reads.
            if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    if let Err(e) = written {
        if let Ok(name) = c_string(temp.as_bytes()) {
            // SAFETY: `dir` is an open descriptor and `name` a valid C string.
            unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
        }
        return Err(e);
    }
    Ok(recipe_hash(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use clusia_core::tests::RECIPE_MAX_BYTES;
    use clusia_testrun::recipe::recipe_hash;
    use clusia_testrun::scan::SHIPPED_CLUSIA;

    use super::*;

    fn worktree() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_missing_folder_or_file_is_no_recipe() {
        let dir = worktree();
        assert_eq!(read_recipe(dir.path()), Ok(None));
        fs::create_dir(dir.path().join(".clusia")).unwrap();
        fs::write(dir.path().join(".clusia/review.md"), "x").unwrap();
        assert_eq!(read_recipe(dir.path()), Ok(None));
    }

    #[test]
    fn the_file_is_read_as_it_is() {
        let dir = worktree();
        fs::create_dir(dir.path().join(".clusia")).unwrap();
        fs::write(
            dir.path().join(".clusia/recipe.toml"),
            "runner = \"auto\"\n",
        )
        .unwrap();
        assert_eq!(
            read_recipe(dir.path()),
            Ok(Some(b"runner = \"auto\"\n".to_vec()))
        );
    }

    #[test]
    fn a_symlinked_folder_or_file_is_refused_and_never_followed() {
        let outside = worktree();
        fs::write(outside.path().join("recipe.toml"), "runner = \"local\"\n").unwrap();

        let linked_file = worktree();
        fs::create_dir(linked_file.path().join(".clusia")).unwrap();
        symlink(
            outside.path().join("recipe.toml"),
            linked_file.path().join(".clusia/recipe.toml"),
        )
        .unwrap();
        assert_eq!(
            read_recipe(linked_file.path()),
            Err(SHIPPED_CLUSIA.to_string())
        );

        let linked_dir = worktree();
        symlink(outside.path(), linked_dir.path().join(".clusia")).unwrap();
        assert_eq!(
            read_recipe(linked_dir.path()),
            Err(SHIPPED_CLUSIA.to_string())
        );

        let as_dir = worktree();
        fs::create_dir_all(as_dir.path().join(".clusia/recipe.toml")).unwrap();
        assert_eq!(read_recipe(as_dir.path()), Err(SHIPPED_CLUSIA.to_string()));
    }

    #[test]
    fn a_fifo_in_place_of_the_recipe_is_refused_without_blocking() {
        let dir = worktree();
        fs::create_dir(dir.path().join(".clusia")).unwrap();
        let path = std::ffi::CString::new(dir.path().join(".clusia/recipe.toml").to_str().unwrap())
            .unwrap();
        // SAFETY: `path` is a valid C string; `mkfifo` only creates the named pipe.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert_eq!(read_recipe(dir.path()), Err(SHIPPED_CLUSIA.to_string()));
    }

    #[test]
    fn a_file_over_the_cap_is_refused() {
        let dir = worktree();
        fs::create_dir(dir.path().join(".clusia")).unwrap();
        let path = dir.path().join(".clusia/recipe.toml");
        let max = RECIPE_MAX_BYTES;
        fs::write(&path, vec![b'a'; max]).unwrap();
        assert_eq!(read_recipe(dir.path()).unwrap().unwrap().len(), max);
        fs::write(&path, vec![b'a'; max + 1]).unwrap();
        assert_eq!(
            read_recipe(dir.path()),
            Err(".clusia/recipe.toml is larger than 64 KiB".to_string())
        );
    }

    #[test]
    fn adopt_writes_over_clusias_own_file_and_proposes_over_the_reviewers() {
        let ours = b"runner = \"auto\"\n";
        let written = recipe_hash(ours);
        assert_eq!(adopt(None, None), Adopt::Write);
        assert_eq!(adopt(None, Some(&written)), Adopt::Write);
        assert_eq!(adopt(Some(ours), Some(&written)), Adopt::Write);
        assert_eq!(
            adopt(Some(b"runner = \"local\"\n"), Some(&written)),
            Adopt::Propose,
            "a hand edit"
        );
        assert_eq!(
            adopt(Some(ours), None),
            Adopt::Propose,
            "a file Clúsia never wrote"
        );
    }

    #[test]
    fn writing_makes_the_folder_and_returns_the_hash_of_the_bytes() {
        let dir = worktree();
        let text = "runner = \"auto\"\n";
        let hash = write_recipe(dir.path(), text).unwrap();
        assert_eq!(hash, recipe_hash(text.as_bytes()));
        assert_eq!(
            fs::read_to_string(dir.path().join(".clusia/recipe.toml")).unwrap(),
            text
        );
        let again = write_recipe(dir.path(), "runner = \"local\"\n").unwrap();
        assert_ne!(again, hash);
        assert_eq!(
            fs::read_to_string(dir.path().join(".clusia/recipe.toml")).unwrap(),
            "runner = \"local\"\n"
        );
    }

    #[test]
    fn writing_leaves_no_temporary_file() {
        let dir = worktree();
        write_recipe(dir.path(), "a = 1\n").unwrap();
        let names: Vec<_> = fs::read_dir(dir.path().join(".clusia"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["recipe.toml"]);
    }

    #[test]
    fn writers_at_once_leave_one_whole_file_and_no_temporary_one() {
        let dir = worktree();
        let path = dir.path().to_path_buf();
        let texts: Vec<String> = (0..8).map(|n| format!("n = {n}\n")).collect();
        let writers: Vec<_> = texts
            .iter()
            .cloned()
            .map(|text| {
                let path = path.clone();
                std::thread::spawn(move || write_recipe(&path, &text).unwrap())
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let now = fs::read_to_string(dir.path().join(".clusia/recipe.toml")).unwrap();
        assert!(texts.contains(&now), "{now}");
        let names: Vec<_> = fs::read_dir(dir.path().join(".clusia"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["recipe.toml"]);
    }

    #[test]
    fn writing_replaces_a_symlink_without_touching_its_target() {
        let dir = worktree();
        let outside = worktree();
        let target = outside.path().join("secret.toml");
        fs::write(&target, "keep me").unwrap();
        fs::create_dir(dir.path().join(".clusia")).unwrap();
        symlink(&target, dir.path().join(".clusia/recipe.toml")).unwrap();
        write_recipe(dir.path(), "a = 1\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        let meta = fs::symlink_metadata(dir.path().join(".clusia/recipe.toml")).unwrap();
        assert!(meta.is_file() && !meta.file_type().is_symlink());
    }

    #[test]
    fn writing_refuses_a_symlinked_folder_and_an_oversized_text() {
        let outside = worktree();
        let dir = worktree();
        symlink(outside.path(), dir.path().join(".clusia")).unwrap();
        let error = write_recipe(dir.path(), "a = 1\n").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!outside.path().join("recipe.toml").exists());

        let plain = worktree();
        let big = "a".repeat(RECIPE_MAX_BYTES + 1);
        let error = write_recipe(plain.path(), &big).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!plain.path().join(".clusia").exists());
    }
}
