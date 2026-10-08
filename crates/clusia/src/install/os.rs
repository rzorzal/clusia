//! The real machine behind `InstallOs`: files, `cargo`, `codesign`, `security`, `openssl` and
//! `launchctl`.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clusia_core::Paths;

use super::execute::{InstallError, InstallOs, LinkState, parse_identities};
use super::plan::BUNDLE_ID;

const SECURITY: &str = "/usr/bin/security";
const CODESIGN: &str = "/usr/bin/codesign";
/// LibreSSL, which writes the PKCS#12 flavour `security import` reads. A Homebrew OpenSSL 3 on
/// `$PATH` does not.
const OPENSSL: &str = "/usr/bin/openssl";
const LAUNCHCTL: &str = "/bin/launchctl";
const IDENTITY_PASSWORD: &str = "clusia-local";

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
        fs::write(path, bytes).map_err(|e| io("cannot write", path, e))?;
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

    fn find_identity(&mut self, name: &str) -> Option<String> {
        // Without `-v`: a self-signed identity is listed as not trusted, and codesign still
        // accepts it.
        let out = run(Command::new(SECURITY).args(["find-identity", "-p", "codesigning"])).ok()?;
        parse_identities(&out)
            .into_iter()
            .find(|(_, n)| n == name)
            .map(|(sha, _)| sha)
    }

    fn create_identity(&mut self, name: &str) -> Result<String, InstallError> {
        let dir = std::env::temp_dir().join(format!("clusia-identity-{}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| io("cannot create", &dir, e))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io("cannot protect", &dir, e))?;
        let made = make_identity(&dir, name);
        let _ = fs::remove_dir_all(&dir);
        made?;
        self.find_identity(name)
            .ok_or_else(|| InstallError::new("the new identity is not in the keychain"))
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

    fn stop_daemon(&mut self) -> bool {
        // Without a socket no daemon is listening; skip spawning a process to learn that.
        if !self.paths.socket().exists() {
            return false;
        }
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        // `daemon stop` waits for the socket to go away and fails when no daemon runs.
        self.daemon_command(&exe, "stop")
            .is_ok_and(|status| status.success())
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

/// A self-signed certificate for code signing, imported with its key into the login keychain.
fn make_identity(dir: &Path, name: &str) -> Result<(), InstallError> {
    let config = dir.join("openssl.cnf");
    fs::write(
        &config,
        format!(
            "[req]\ndistinguished_name = dn\nx509_extensions = ext\nprompt = no\nutf8 = yes\nstring_mask = utf8only\n\
             [dn]\nCN = {name}\n\
             [ext]\nbasicConstraints = critical,CA:false\nkeyUsage = critical,digitalSignature\nextendedKeyUsage = critical,codeSigning\n"
        ),
    )
    .map_err(|e| io("cannot write", &config, e))?;
    let (key, cert, bundle) = (
        dir.join("key.pem"),
        dir.join("cert.pem"),
        dir.join("id.p12"),
    );
    run(Command::new(OPENSSL)
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650",
        ])
        .arg("-config")
        .arg(&config)
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert))?;
    run(Command::new(OPENSSL)
        .args(["pkcs12", "-export", "-inkey"])
        .arg(&key)
        .arg("-in")
        .arg(&cert)
        .arg("-out")
        .arg(&bundle)
        .args(["-passout", &format!("pass:{IDENTITY_PASSWORD}")]))?;
    run(Command::new(SECURITY).arg("import").arg(&bundle).args([
        "-P",
        IDENTITY_PASSWORD,
        "-T",
        CODESIGN,
    ]))
    .map(|_| ())
}
