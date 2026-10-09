//! `agent/<owner~repo~n>.state.json`: what a review's agent session remembers.

use std::fs;
use std::io;

use clusia_core::{AgentState, Paths, PrRef};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

/// The saved state, or the empty one when there is none. An unreadable file is moved aside
/// and the session starts fresh.
pub fn load_agent_state(paths: &Paths, pr: &PrRef) -> io::Result<AgentState> {
    let path = paths.agent_state(pr);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(AgentState::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => Ok(state),
        Err(e) => {
            let moved = quarantine(&path, now_unix())?;
            tracing::warn!(error = %e, file = %moved.display(), "agent state was unreadable and was quarantined");
            Ok(AgentState::default())
        }
    }
}

pub fn save_agent_state(paths: &Paths, pr: &PrRef, state: &AgentState) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    write_atomic(&paths.agent_state(pr), &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Paths, PrRef) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        (dir, paths, PrRef::new("acme", "widgets", 7).unwrap())
    }

    #[test]
    fn a_missing_file_is_the_empty_state_and_nothing_is_written() {
        let (_d, paths, pr) = setup();
        assert_eq!(
            load_agent_state(&paths, &pr).unwrap(),
            AgentState::default()
        );
        assert!(!paths.agent_dir().exists());
    }

    #[test]
    fn state_round_trips() {
        let (_d, paths, pr) = setup();
        let mut state = AgentState::default();
        state.dismiss("sug-aaa");
        state.accept("sug-bbb");
        state.last_summary_head = Some("a1b2c3d4e5f6".into());
        save_agent_state(&paths, &pr, &state).unwrap();
        assert_eq!(load_agent_state(&paths, &pr).unwrap(), state);
        let other = PrRef::new("acme", "widgets", 8).unwrap();
        assert_eq!(
            load_agent_state(&paths, &other).unwrap(),
            AgentState::default(),
            "each pull request has its own file"
        );
    }

    #[test]
    fn an_unreadable_file_is_quarantined() {
        let (_d, paths, pr) = setup();
        fs::create_dir_all(paths.agent_dir()).unwrap();
        fs::write(paths.agent_state(&pr), "{ not json").unwrap();
        assert_eq!(
            load_agent_state(&paths, &pr).unwrap(),
            AgentState::default()
        );
        assert!(!paths.agent_state(&pr).exists());
        let names: Vec<_> = fs::read_dir(paths.agent_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(
            names[0].starts_with("acme~widgets~7.state.json.corrupt-"),
            "{names:?}"
        );
    }
}
