//! Helpers for tests that run stand-in programs and wait on them.
//!
//! macOS checks the first execution of every new executable file, which takes a fraction of a
//! second on an idle machine and seconds on a busy one. A test that writes a script and
//! immediately has the code under test spawn it would race its own deadline against that check,
//! so [`write_executable`] pays the check before returning.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Set for the one run that only warms a script; the script ends before its own body.
const WARMUP: &str = "CLUSIA_TEST_WARMUP";

/// Writes `/bin/sh` script `body` at `path`, ready to be executed by the code under test.
///
/// The file is complete, synced, executable and closed (so no `ETXTBSY`) before it appears under
/// `path`, and it has already been run once, so later runs start without the first-run check.
pub fn write_executable(path: &Path, body: &str) {
    let staging = path.with_extension("staging");
    let mut file = File::create(&staging).unwrap();
    write!(file, "#!/bin/sh\n[ -n \"${WARMUP}\" ] && exit 0\n{body}\n").unwrap();
    file.sync_all().unwrap();
    drop(file);
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&staging, path).unwrap();
    let warmed = Command::new(path)
        .env(WARMUP, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(warmed.success(), "{} did not start", path.display());
}

/// The shortest deadline for something that must happen: long enough that only a hang runs it
/// out, however busy the machine. Never use it for a time the test expects to run out.
pub const HANG_BOUND: Duration = Duration::from_secs(60);

/// `base`, but at least [`HANG_BOUND`].
pub fn patient(base: Duration) -> Duration {
    base.max(HANG_BOUND)
}
