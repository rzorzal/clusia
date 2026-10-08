//! The real machine behind `InstallOs`: files, `cargo`, `codesign` and `launchctl`.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clusia_core::Paths;
use clusia_store::atomic::write_atomic;

use super::execute::{InstallError, InstallOs, LinkState};
use super::plan::BUNDLE_ID;

const CODESIGN: &str = "/usr/bin/codesign";
const LAUNCHCTL: &str = "/bin/launchctl";

pub struct SystemOs {
    paths: Paths,
    /// The `--home` the user gave, passed on to the daemon commands; `None` when the folders
    /// come from the environment, so the daemon resolves them the same way.
    home: Option<PathBuf>,
}

impl SystemOs {
    pub fn new(paths: Paths, home: Option<PathBuf>) -> Self {
        Self { paths, home }
    }

    /// `clusia [--home DIR] daemon <verb>`, without its output.
    fn daemon_command(
        &self,
        clusia: &Path,
        verb: &str,
    ) -> std::io::Result<std::process::ExitStatus> {
        let mut command = Command::new(clusia);
        if let Some(home) = &self.home {
            command.arg("--home").arg(home);
        }
        command
            .args(["daemon", verb])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    }
}

fn io(what: &str, path: &Path, e: std::io::Error) -> InstallError {
    InstallError::new(format!("{what} {}: {e}", path.display()))
}

fn run(command: &mut Command) -> Result<String, InstallError> {
    let name = command.get_program().to_string_lossy().into_owned();
    let out = command
        .output()
        .map_err(|e| InstallError::new(format!("cannot run {name}: {e}")))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(InstallError::new(format!(
        "{name} failed ({}): {}",
        out.status,
        stderr.trim()
    )))
}

impl InstallOs for SystemOs {
    fn build_release(&mut self, workspace: &Path) -> Result<PathBuf, InstallError> {
        let status = Command::new("cargo")
            .current_dir(workspace)
            .args(["build", "--release"])
            .args([
                "-p",
                "clusia",
                "-p",
                "clusiad",
                "-p",
                "clusia-app",
                "-p",
                "clusia-tray",
            ])
            .status()
            .map_err(|e| InstallError::new(format!("cannot run cargo: {e}")))?;
        if !status.success() {
            return Err(InstallError::new(format!("cargo build failed ({status})")));
        }
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.join("target"));
        Ok(target.join("release"))
    }

    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    fn create_dir_all(&mut self, dir: &Path) -> Result<(), InstallError> {
        fs::create_dir_all(dir).map_err(|e| io("cannot create", dir, e))
    }

    fn write_file(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<(), InstallError> {
        // Atomic, so an interrupted install never leaves a half-written login agent behind.
        write_atomic(path, bytes).map_err(|e| io("cannot write", path, e))?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| io("cannot set the mode of", path, e))
    }

    fn copy_file(&mut self, from: &Path, to: &Path, mode: u32) -> Result<(), InstallError> {
        fs::copy(from, to).map_err(|e| io("cannot copy to", to, e))?;
        fs::set_permissions(to, fs::Permissions::from_mode(mode))
            .map_err(|e| io("cannot set the mode of", to, e))
    }

    fn remove_dir_all(&mut self, dir: &Path) -> Result<(), InstallError> {
        fs::remove_dir_all(dir).map_err(|e| io("cannot remove", dir, e))
    }

    fn remove_file(&mut self, path: &Path) -> Result<(), InstallError> {
        fs::remove_file(path).map_err(|e| io("cannot remove", path, e))
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), InstallError> {
        fs::rename(from, to).map_err(|e| io("cannot move to", to, e))
    }

    fn exchange(&mut self, a: &Path, b: &Path) -> Result<bool, InstallError> {
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| {
            std::ffi::CString::new(p.as_os_str().as_bytes())
                .map_err(|e| io("cannot move to", p, std::io::Error::other(e)))
        };
        let (from, to) = (c(a)?, c(b)?);
        // SAFETY: both are valid NUL-terminated paths for the whole call.
        if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOTSUP | libc::EINVAL) => Ok(false),
            _ => Err(io("cannot move to", b, e)),
        }
    }

    fn symlink(&mut self, target: &Path, link: &Path) -> Result<(), InstallError> {
        symlink(target, link).map_err(|e| io("cannot link", link, e))
    }

    fn link_state(&self, path: &Path) -> LinkState {
        match path.symlink_metadata() {
            Err(_) => LinkState::Missing,
            Ok(meta) if meta.file_type().is_symlink() => fs::read_link(path)
                .map(LinkState::Link)
                .unwrap_or(LinkState::Other),
            Ok(_) => LinkState::Other,
        }
    }

    fn files_under(&self, dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(dir) = todo.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => todo.push(entry.path()),
                    Ok(_) => found.push(entry.path()),
                    Err(_) => {}
                }
            }
        }
        found
    }

    fn codesign(&mut self, bundle: &Path, identity: &str) -> Result<(), InstallError> {
        run(Command::new(CODESIGN)
            .args([
                "--force",
                "--deep",
                "--sign",
                identity,
                "--identifier",
                BUNDLE_ID,
            ])
            .arg(bundle))?;
        run(Command::new(CODESIGN)
            .args(["--verify", "--deep", "--strict"])
            .arg(bundle))
        .map(|_| ())
    }

    fn launchctl(&mut self, args: &[&str]) -> Result<(), InstallError> {
        run(Command::new(LAUNCHCTL).args(args)).map(|_| ())
    }

    fn daemon_running(&self) -> bool {
        // The daemon removes its socket when it exits.
        self.paths.socket().exists()
    }

    fn stop_daemon(&mut self) {
        if !self.daemon_running() {
            return;
        }
        // `daemon stop` waits for the socket to go away.
        if let Ok(exe) = std::env::current_exe() {
            let _ = self.daemon_command(&exe, "stop");
        }
    }

    fn start_daemon(&mut self, clusia: &Path) {
        // Opening Clusia.app starts the daemon too, so a failure here is only reported.
        if let Err(e) = self.daemon_command(clusia, "start") {
            eprintln!(
                "clusia: cannot start the daemon with {}: {e}",
                clusia.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_swaps_two_folders() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("new"), "n").unwrap();
        fs::write(b.join("old"), "o").unwrap();
        let mut os = SystemOs::new(Paths::new(dir.path()), None);
        assert!(os.exchange(&a, &b).unwrap(), "APFS exchanges folders");
        assert!(b.join("new").exists() && !b.join("old").exists());
        assert!(a.join("old").exists());
        assert!(os.exchange(&a, &dir.path().join("missing")).is_err());
    }
}
