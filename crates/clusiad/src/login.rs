//! Start at login: the setting and the LaunchAgent's `RunAtLoad` change together.

use std::path::Path;

use clusia_core::launch_agent::{self, LaunchAgent};
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, Reply};
use clusia_store::atomic::write_atomic;

use crate::handlers::set_config_value;
use crate::state::Shared;

/// Saves `general.start_at_login`, then updates the agent file. launchd reads it at the next
/// login, so nothing is reloaded: unloading a job from inside it would end the daemon.
pub(crate) async fn set_start_at_login(shared: &Shared, on: bool) -> Outcome {
    let saved = set_config_value(shared, "general.start_at_login".into(), on.to_string()).await;
    if matches!(saved, Outcome::Err(_)) {
        return saved;
    }
    let agent = shared.paths.launch_agent();
    match update_agent(&agent, on, std::env::current_exe().ok().as_deref(), shared) {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(message) => Outcome::Err(ProtocolError::new(ErrorCode::Internal, message)),
    }
}

/// Rewrites `RunAtLoad` in an existing agent. Without one, turning the setting on writes a
/// new agent only when this daemon runs from an installed bundle; a daemon started from a
/// build folder has nothing to keep alive.
fn update_agent(agent: &Path, on: bool, exe: Option<&Path>, shared: &Shared) -> Result<(), String> {
    let write = |text: String| {
        if let Some(dir) = agent.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        write_atomic(agent, text.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", agent.display()))
    };
    match std::fs::read_to_string(agent) {
        Ok(existing) => write(
            launch_agent::with_start_at_login(&existing, on)
                .map_err(|e| format!("{}: {e}", agent.display()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let Some(exe) =
                exe.filter(|exe| on && exe.to_string_lossy().contains(".app/Contents/MacOS/"))
            else {
                return Ok(());
            };
            let log = shared.paths.logs_dir().join("daemon.launchd.log");
            write(launch_agent::render(&LaunchAgent {
                daemon: exe,
                log: &log,
                start_at_login: on,
            }))
        }
        Err(e) => Err(format!("cannot read {}: {e}", agent.display())),
    }
}
