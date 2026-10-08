//! `clusia install`: assemble `Clusia.app`, sign it, register the LaunchAgent and link the CLI.

mod assets;
mod plan;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use clusia_core::Paths;

pub use plan::{InstallEnv, InstallOptions, plan};

/// The machine as it is now: home, user id, writable folders and the `$PATH`.
pub fn system_env(paths: &Paths, start_at_login: bool) -> Result<InstallEnv, String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    Ok(InstallEnv {
        version: env!("CARGO_PKG_VERSION").to_string(),
        // SAFETY: getuid has no preconditions and cannot fail.
        uid: unsafe { libc::getuid() },
        current_dir: std::env::current_dir().map_err(|e| e.to_string())?,
        start_at_login,
        launch_agent: paths.launch_agent(),
        logs_dir: paths.logs_dir().to_path_buf(),
        applications_writable: writable(Path::new("/Applications")),
        usr_local_bin_writable: writable(Path::new("/usr/local/bin")),
        path: std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default(),
        home,
    })
}

/// Whether this process may create files in `dir` (which must exist).
fn writable(dir: &Path) -> bool {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path for the whole call.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}
