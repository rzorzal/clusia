//! `tabs.json`: the review tabs the window had open, so they come back with it.

use std::fs;
use std::io;

use clusia_core::{OpenTabs, Paths};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

/// The saved tabs, or none when there is no file. An unreadable file is moved aside and the
/// window starts with no tabs.
pub fn load_open_tabs(paths: &Paths) -> io::Result<OpenTabs> {
    let path = paths.open_tabs_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(OpenTabs::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(tabs) => Ok(tabs),
        Err(e) => {
            let moved = quarantine(&path, now_unix())?;
            tracing::warn!(error = %e, file = %moved.display(), "open tabs were unreadable and were quarantined");
            Ok(OpenTabs::default())
        }
    }
}

pub fn save_open_tabs(paths: &Paths, tabs: &OpenTabs) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(tabs).map_err(io::Error::other)?;
    write_atomic(&paths.open_tabs_file(), &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_round_trip_and_a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        assert_eq!(load_open_tabs(&paths).unwrap(), OpenTabs::default());
        let tabs = OpenTabs {
            tabs: vec![OpenTabs::tab("acme/widgets#7".parse().unwrap(), "comments")],
            selected: Some("acme/widgets#7".parse().unwrap()),
        };
        save_open_tabs(&paths, &tabs).unwrap();
        assert_eq!(load_open_tabs(&paths).unwrap(), tabs);
    }

    #[test]
    fn an_unreadable_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::write(paths.open_tabs_file(), b"{not json").unwrap();
        assert_eq!(load_open_tabs(&paths).unwrap(), OpenTabs::default());
        assert!(!paths.open_tabs_file().exists());
    }
}
