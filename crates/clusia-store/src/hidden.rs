//! `hidden/<owner~repo~n>.json`: the comments hidden in a pull request. It is not part of the
//! review file, which closing an empty review deletes; hiding must outlast that.

use std::fs;
use std::io;

use clusia_core::{HiddenComments, Paths, PrRef};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

/// The hidden comments, or none when there is no file. An unreadable file is moved aside.
pub fn load_hidden(paths: &Paths, pr: &PrRef) -> io::Result<HiddenComments> {
    let path = paths.hidden_file(pr);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HiddenComments::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(hidden) => Ok(hidden),
        Err(e) => {
            let moved = quarantine(&path, now_unix())?;
            tracing::warn!(error = %e, file = %moved.display(), "hidden comments were unreadable and were quarantined");
            Ok(HiddenComments::default())
        }
    }
}

/// Saves the set; an empty one removes the file.
pub fn save_hidden(paths: &Paths, pr: &PrRef, hidden: &HiddenComments) -> io::Result<()> {
    if hidden.is_empty() {
        return delete_hidden(paths, pr);
    }
    let json = serde_json::to_vec(hidden).map_err(io::Error::other)?;
    write_atomic(&paths.hidden_file(pr), &json)
}

/// Removes the file of `pr`; a missing file is fine.
pub fn delete_hidden(paths: &Paths, pr: &PrRef) -> io::Result<()> {
    match fs::remove_file(paths.hidden_file(pr)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    #[test]
    fn the_set_round_trips_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        assert!(load_hidden(&paths, &pr()).unwrap().is_empty());
        let mut set = HiddenComments::default();
        set.hide("thread:PRRT_1", 2);
        save_hidden(&paths, &pr(), &set).unwrap();
        assert_eq!(load_hidden(&paths, &pr()).unwrap(), set);
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        assert!(load_hidden(&paths, &other).unwrap().is_empty());
    }

    #[test]
    fn saving_an_empty_set_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let mut set = HiddenComments::default();
        set.hide("comment:1", 1);
        save_hidden(&paths, &pr(), &set).unwrap();
        save_hidden(&paths, &pr(), &HiddenComments::default()).unwrap();
        assert!(!paths.hidden_file(&pr()).exists());
    }

    #[test]
    fn an_unreadable_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::create_dir_all(paths.hidden_dir()).unwrap();
        fs::write(paths.hidden_file(&pr()), b"{not json").unwrap();
        assert!(load_hidden(&paths, &pr()).unwrap().is_empty());
        assert!(!paths.hidden_file(&pr()).exists());
    }
}
