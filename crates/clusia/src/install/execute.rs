//! Carries an install plan out, and takes it back. Every action on the machine goes through
//! `InstallOs`, so the order and the choices are tested against a fake.

use std::path::{Path, PathBuf};

use clusia_core::launch_agent;

use super::plan::{
    BINARIES, BUNDLE_ID, Content, Plan, Source, render_info_plist, render_launch_agent,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InstallError(pub String);

impl InstallError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// What a path is, for the places where only a link of ours may be touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Missing,
    /// A symbolic link to this target.
    Link(PathBuf),
    /// A regular file or folder.
    Other,
}

/// Everything an install does to the machine.
pub trait InstallOs {
    /// Builds the release binaries; returns the folder holding them.
    fn build_release(&mut self, workspace: &Path) -> Result<PathBuf, InstallError>;
    fn exists(&self, path: &Path) -> bool;
    fn read_file(&self, path: &Path) -> Option<Vec<u8>>;
    fn create_dir_all(&mut self, dir: &Path) -> Result<(), InstallError>;
    fn write_file(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<(), InstallError>;
    fn copy_file(&mut self, from: &Path, to: &Path, mode: u32) -> Result<(), InstallError>;
    fn remove_dir_all(&mut self, dir: &Path) -> Result<(), InstallError>;
    fn remove_file(&mut self, path: &Path) -> Result<(), InstallError>;
    fn rename(&mut self, from: &Path, to: &Path) -> Result<(), InstallError>;
    fn symlink(&mut self, target: &Path, link: &Path) -> Result<(), InstallError>;
    fn link_state(&self, path: &Path) -> LinkState;
    /// Every file below `dir`, at any depth.
    fn files_under(&self, dir: &Path) -> Vec<PathBuf>;
    /// Signs the bundle with `identity` (`-` is ad hoc) and checks the signature.
    fn codesign(&mut self, bundle: &Path, identity: &str) -> Result<(), InstallError>;
    fn launchctl(&mut self, args: &[&str]) -> Result<(), InstallError>;
    /// Whether a daemon is listening now.
    fn daemon_running(&self) -> bool;
    /// Asks a running daemon to stop.
    fn stop_daemon(&mut self);
    /// Starts the daemon through `clusia` (the command inside the new bundle).
    fn start_daemon(&mut self, clusia: &Path);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub app: PathBuf,
    pub launch_agent: PathBuf,
    pub cli_link: Option<PathBuf>,
    /// Things that did not work but did not stop the install.
    pub warnings: Vec<String>,
    /// What to tell the user (folders not usable, `$PATH` to extend).
    pub notes: Vec<String>,
}

impl Report {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "installed": self.app,
            "signed_with": "ad-hoc",
            "launch_agent": self.launch_agent,
            "cli_link": self.cli_link,
            "warnings": self.warnings,
        })
    }

    pub fn describe(&self) -> String {
        let mut out = format!("Installed {}\n", self.app.display());
        out.push_str("  signed ad hoc\n");
        out.push_str(&format!("  login agent {}\n", self.launch_agent.display()));
        if let Some(link) = &self.cli_link {
            out.push_str(&format!("  command {}\n", link.display()));
        }
        for warning in &self.warnings {
            out.push_str(&format!("  warning: {warning}\n"));
        }
        for note in &self.notes {
            out.push_str(&format!("  {note}\n"));
        }
        out.push_str("Open Clusia.app to start; the first notification asks for permission.\n");
        out
    }
}

/// Builds the bundle next to its final place, signs it there, then swaps it in. The old bundle
/// stays until the new one is in place, and comes back if the swap fails. Once launchd and the
/// daemon have been stopped, a failure puts them back as they were.
pub fn execute(plan: &Plan, os: &mut dyn InstallOs) -> Result<Report, InstallError> {
    refuse_foreign(plan, os)?;
    let binaries = match &plan.source {
        Source::Build { workspace } => os.build_release(workspace)?,
        Source::Dir(dir) => dir.clone(),
    };
    let missing: Vec<&str> = BINARIES
        .into_iter()
        .filter(|name| !os.exists(&binaries.join(name)))
        .collect();
    if !missing.is_empty() {
        return Err(InstallError::new(format!(
            "{} is missing {}",
            binaries.display(),
            missing.join(", ")
        )));
    }

    if os.exists(&plan.staging) {
        os.remove_dir_all(&plan.staging)?;
    }
    for file in &plan.files {
        let to = plan.staging.join(&file.path);
        if let Some(dir) = to.parent() {
            os.create_dir_all(dir)?;
        }
        match &file.content {
            Content::Binary(name) => os.copy_file(&binaries.join(name), &to, 0o755)?,
            Content::Asset(asset) => os.write_file(&to, asset.bytes(), 0o644)?,
            Content::InfoPlist => os.write_file(&to, render_info_plist(plan).as_bytes(), 0o644)?,
        }
    }
    if let Err(e) = sign(os, &plan.staging) {
        let _ = os.remove_dir_all(&plan.staging);
        return Err(e);
    }

    let mut warnings = Vec::new();
    // `refuse_foreign` passed, so an agent file here is ours and was loaded from this path.
    let had_agent = os.exists(&plan.launch_agent);
    let mut daemon_was_running = false;
    if plan.launchctl {
        // Asked before the bootout, which stops a daemon that launchd runs.
        daemon_was_running = os.daemon_running();
        // The old daemon runs from the old bundle: unload it and stop it before the swap.
        let _ = os.launchctl(&["bootout", &plan.service_target()]);
        os.stop_daemon();
    }
    let replaced = swap(plan, os, &mut warnings).and_then(|()| write_agent(plan, os));
    let agent_now = replaced.is_ok() || had_agent;
    if plan.launchctl {
        let mut loaded = false;
        if agent_now {
            let agent = plan.launch_agent.display().to_string();
            match os.launchctl(&["bootstrap", &plan.domain, &agent]) {
                Ok(()) => loaded = true,
                Err(e) => warnings.push(format!("launchd did not load the login agent: {e}")),
            }
        }
        // An agent that starts at login also starts as it loads: a second daemon started here
        // would race it for the lock. Otherwise a daemon that was running comes back from the
        // bundle now in place, under launchd when it can, so a crash is restarted.
        let starts_on_load = loaded && agent_starts_at_login(plan, os);
        if daemon_was_running && !starts_on_load {
            let kicked = loaded && os.launchctl(&["kickstart", &plan.service_target()]).is_ok();
            if !kicked {
                os.start_daemon(&plan.cli.target);
            }
        }
    }
    replaced?;

    let cli_link = link_cli(plan, os, &mut warnings);
    Ok(Report {
        app: plan.app.clone(),
        launch_agent: plan.launch_agent.clone(),
        cli_link,
        warnings,
        notes: plan.notes.clone(),
    })
}

/// The `RunAtLoad` of the agent file now in place (the new one, or the old one put back).
fn agent_starts_at_login(plan: &Plan, os: &dyn InstallOs) -> bool {
    os.read_file(&plan.launch_agent)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| launch_agent::start_at_login(&text))
        .unwrap_or(false)
}

fn write_agent(plan: &Plan, os: &mut dyn InstallOs) -> Result<(), InstallError> {
    if let Some(dir) = plan.launch_agent.parent() {
        os.create_dir_all(dir)?;
    }
    os.write_file(
        &plan.launch_agent,
        render_launch_agent(plan, plan.start_at_login).as_bytes(),
        0o644,
    )
}

/// The `CFBundleIdentifier` of the bundle at `app`, `None` when it has no readable one.
fn bundle_id(os: &dyn InstallOs, app: &Path) -> Option<String> {
    let bytes = os.read_file(&app.join("Contents/Info.plist"))?;
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    value
        .as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(str::to_string)
}

/// Neither a bundle at the install path that is not Clúsia nor a login agent that is not ours
/// is replaced: the folder or the file belongs to someone else.
fn refuse_foreign(plan: &Plan, os: &dyn InstallOs) -> Result<(), InstallError> {
    if os.exists(&plan.app) {
        match bundle_id(os, &plan.app) {
            Some(id) if id == BUNDLE_ID => {}
            Some(id) => {
                return Err(InstallError::new(format!(
                    "{} belongs to {id}, not to Clúsia; move it away or pick another --applications folder",
                    plan.app.display()
                )));
            }
            None => {
                return Err(InstallError::new(format!(
                    "{} exists but is not a Clúsia bundle; move it away or pick another --applications folder",
                    plan.app.display()
                )));
            }
        }
    }
    if os.exists(&plan.launch_agent) {
        let ours = os
            .read_file(&plan.launch_agent)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .is_some_and(|text| launch_agent::is_ours(&text));
        if !ours {
            return Err(InstallError::new(format!(
                "{} exists and is not Clúsia's login agent; move it away or pick another --agents-dir",
                plan.launch_agent.display()
            )));
        }
    }
    Ok(())
}

/// Ad hoc: no keychain, so no prompt can stall an unattended install. An unsealed bundle never
/// replaces a working one, because notifications need the seal over its Info.plist.
fn sign(os: &mut dyn InstallOs, bundle: &Path) -> Result<(), InstallError> {
    // A killed codesign leaves `*.cstemp` files, and codesign refuses a bundle holding one.
    for stale in os.files_under(bundle) {
        if stale.extension().is_some_and(|e| e == "cstemp") {
            os.remove_file(&stale)?;
        }
    }
    os.codesign(bundle, "-")
        .map_err(|e| InstallError::new(format!("the bundle could not be signed: {e}")))
}

fn swap(
    plan: &Plan,
    os: &mut dyn InstallOs,
    warnings: &mut Vec<String>,
) -> Result<(), InstallError> {
    let had_old = os.exists(&plan.app);
    if had_old {
        if os.exists(&plan.backup) {
            os.remove_dir_all(&plan.backup)?;
        }
        os.rename(&plan.app, &plan.backup)?;
    }
    if let Err(e) = os.rename(&plan.staging, &plan.app) {
        if had_old && let Err(back) = os.rename(&plan.backup, &plan.app) {
            return Err(InstallError::new(format!(
                "{e}; the previous bundle could not be put back ({back}) and is in {}",
                plan.backup.display()
            )));
        }
        return Err(e);
    }
    if had_old && let Err(e) = os.remove_dir_all(&plan.backup) {
        warnings.push(format!(
            "the previous bundle was left in {}: {e}",
            plan.backup.display()
        ));
    }
    Ok(())
}

/// Where a Clúsia install, wherever its bundle is, points the `clusia` command.
const OUR_LINK_SUFFIX: &str = "Clusia.app/Contents/MacOS/clusia";

/// The `clusia` command: a link to a Clúsia bundle is replaced, anything else is left alone. The
/// install is done by now, so a link that cannot be made is a warning.
fn link_cli(plan: &Plan, os: &mut dyn InstallOs, warnings: &mut Vec<String>) -> Option<PathBuf> {
    let link = &plan.cli.link;
    let made = match os.link_state(link) {
        LinkState::Link(to) if to == plan.cli.target => return Some(link.clone()),
        LinkState::Link(to) if to.ends_with(OUR_LINK_SUFFIX) => os
            .remove_file(link)
            .and_then(|()| os.symlink(&plan.cli.target, link)),
        LinkState::Link(to) => {
            warnings.push(format!(
                "{} links to {}, so it was left alone",
                link.display(),
                to.display()
            ));
            return None;
        }
        LinkState::Other => {
            warnings.push(format!(
                "{} exists and is not a link, so it was left alone",
                link.display()
            ));
            return None;
        }
        LinkState::Missing => link
            .parent()
            .map_or(Ok(()), |dir| os.create_dir_all(dir))
            .and_then(|()| os.symlink(&plan.cli.target, link)),
    };
    match made {
        Ok(()) => Some(link.clone()),
        Err(e) => {
            warnings.push(format!("the clusia command was not linked: {e}"));
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub removed: Vec<PathBuf>,
    pub kept: Vec<String>,
    pub notes: Vec<String>,
}

impl Removed {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "removed": self.removed, "kept": self.kept, "notes": self.notes })
    }

    pub fn describe(&self) -> String {
        let mut out = String::from("Uninstalled Clúsia\n");
        for path in &self.removed {
            out.push_str(&format!("  removed {}\n", path.display()));
        }
        for line in &self.kept {
            out.push_str(&format!("  kept {line}\n"));
        }
        for note in &self.notes {
            out.push_str(&format!("  {note}\n"));
        }
        out
    }
}

/// The identity earlier builds signed with; install no longer makes or uses one.
const OLD_SIGNING_NAME: &str = "Clúsia Local";

/// Undoes `execute`: the agent, the bundle and our link. Your data in `data` stays.
pub fn uninstall(
    plan: &Plan,
    data: &Path,
    os: &mut dyn InstallOs,
) -> Result<Removed, InstallError> {
    let mut removed = Vec::new();
    let mut kept = vec![format!("your reviews and settings in {}", data.display())];
    let notes = vec![format!(
        "an older Clúsia may have left a signing identity in your login keychain; remove it with: security delete-identity -c \"{OLD_SIGNING_NAME}\""
    )];
    let agent_is_ours = os.exists(&plan.launch_agent)
        && os
            .read_file(&plan.launch_agent)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .is_some_and(|text| launch_agent::is_ours(&text));
    let bundle_is_ours =
        os.exists(&plan.app) && bundle_id(os, &plan.app).as_deref() == Some(BUNDLE_ID);
    if os.exists(&plan.launch_agent) && !agent_is_ours {
        kept.push(format!(
            "{} (not Clúsia's login agent)",
            plan.launch_agent.display()
        ));
    }
    if os.exists(&plan.app) && !bundle_is_ours {
        kept.push(format!("{} (not a Clúsia bundle)", plan.app.display()));
    }
    if plan.launchctl && (agent_is_ours || bundle_is_ours) {
        let _ = os.launchctl(&["bootout", &plan.service_target()]);
        os.stop_daemon();
    }
    if agent_is_ours {
        os.remove_file(&plan.launch_agent)?;
        removed.push(plan.launch_agent.clone());
    }
    if os.link_state(&plan.cli.link) == LinkState::Link(plan.cli.target.clone()) {
        os.remove_file(&plan.cli.link)?;
        removed.push(plan.cli.link.clone());
    }
    if bundle_is_ours {
        os.remove_dir_all(&plan.app)?;
        removed.push(plan.app.clone());
    }
    for dir in [&plan.staging, &plan.backup] {
        if !os.exists(dir) {
            continue;
        }
        if bundle_id(os, dir).as_deref() == Some(BUNDLE_ID) {
            os.remove_dir_all(dir)?;
            removed.push(dir.clone());
        } else {
            kept.push(format!("{} (not a Clúsia bundle)", dir.display()));
        }
    }
    Ok(Removed {
        removed,
        kept,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::install::plan::{InstallEnv, InstallOptions, plan};

    /// A machine in memory that records what was done to it.
    #[derive(Default)]
    struct Fake {
        files: BTreeMap<PathBuf, Vec<u8>>,
        dirs: BTreeSet<PathBuf>,
        links: BTreeMap<PathBuf, PathBuf>,
        codesign_fails: bool,
        /// Copies into `Contents/MacOS` leave a `.cstemp` next to the binary, the way a killed
        /// codesign does.
        copies_leave_cstemp: bool,
        launchctl_fails: bool,
        kickstart_fails: bool,
        daemon_running: bool,
        /// How many renames onto `Clusia.app` fail.
        renames_into_place_fail: u32,
        removing_fails: Vec<PathBuf>,
        calls: Vec<String>,
    }

    impl Fake {
        fn machine() -> Self {
            let mut fake = Fake::default();
            for name in BINARIES {
                fake.files
                    .insert(PathBuf::from("/built").join(name), name.as_bytes().to_vec());
            }
            fake
        }

        fn under(&self, root: &Path) -> Vec<PathBuf> {
            self.files
                .keys()
                .chain(self.links.keys())
                .filter(|p| p.starts_with(root))
                .cloned()
                .collect()
        }

        fn calls_starting(&self, prefix: &str) -> Vec<&String> {
            self.calls
                .iter()
                .filter(|c| c.starts_with(prefix))
                .collect()
        }
    }

    impl InstallOs for Fake {
        fn build_release(&mut self, workspace: &Path) -> Result<PathBuf, InstallError> {
            self.calls.push(format!("build {}", workspace.display()));
            Ok(PathBuf::from("/built"))
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path)
                || self.dirs.contains(path)
                || self.links.contains_key(path)
                || self.files.keys().any(|f| f.starts_with(path))
        }
        fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.get(path).cloned()
        }
        fn create_dir_all(&mut self, dir: &Path) -> Result<(), InstallError> {
            self.dirs.insert(dir.to_path_buf());
            Ok(())
        }
        fn write_file(&mut self, path: &Path, bytes: &[u8], _: u32) -> Result<(), InstallError> {
            self.calls.push(format!("write {}", path.display()));
            self.files.insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }
        fn copy_file(&mut self, from: &Path, to: &Path, _: u32) -> Result<(), InstallError> {
            self.calls.push(format!("copy {}", to.display()));
            let bytes = self.files.get(from).cloned().unwrap_or_default();
            self.files.insert(to.to_path_buf(), bytes);
            if self.copies_leave_cstemp && to.parent().is_some_and(|d| d.ends_with("MacOS")) {
                let mut stale = to.as_os_str().to_owned();
                stale.push(".cstemp");
                self.files.insert(stale.into(), Vec::new());
            }
            Ok(())
        }
        fn remove_dir_all(&mut self, dir: &Path) -> Result<(), InstallError> {
            self.calls.push(format!("rmdir {}", dir.display()));
            if self.removing_fails.iter().any(|d| d == dir) {
                return Err(InstallError::new("resource busy"));
            }
            self.files.retain(|p, _| !p.starts_with(dir));
            self.dirs.retain(|p| !p.starts_with(dir));
            Ok(())
        }
        fn remove_file(&mut self, path: &Path) -> Result<(), InstallError> {
            self.calls.push(format!("rm {}", path.display()));
            self.files.remove(path);
            self.links.remove(path);
            Ok(())
        }
        fn rename(&mut self, from: &Path, to: &Path) -> Result<(), InstallError> {
            self.calls
                .push(format!("mv {} {}", from.display(), to.display()));
            if self.renames_into_place_fail > 0 && to.ends_with("Clusia.app") && from != to {
                self.renames_into_place_fail -= 1;
                return Err(InstallError::new("disk full"));
            }
            let moved: Vec<_> = self
                .files
                .keys()
                .filter(|p| p.starts_with(from))
                .cloned()
                .collect();
            for old in moved {
                let bytes = self.files.remove(&old).unwrap();
                let new = to.join(old.strip_prefix(from).unwrap());
                self.files.insert(new, bytes);
            }
            Ok(())
        }
        fn symlink(&mut self, target: &Path, link: &Path) -> Result<(), InstallError> {
            self.calls.push(format!("ln {}", link.display()));
            self.links.insert(link.to_path_buf(), target.to_path_buf());
            Ok(())
        }
        fn link_state(&self, path: &Path) -> LinkState {
            if let Some(target) = self.links.get(path) {
                LinkState::Link(target.clone())
            } else if self.files.contains_key(path) {
                LinkState::Other
            } else {
                LinkState::Missing
            }
        }
        fn files_under(&self, dir: &Path) -> Vec<PathBuf> {
            self.files
                .keys()
                .filter(|p| p.starts_with(dir))
                .cloned()
                .collect()
        }
        fn codesign(&mut self, bundle: &Path, identity: &str) -> Result<(), InstallError> {
            self.calls
                .push(format!("codesign {} {identity}", bundle.display()));
            if self.codesign_fails {
                return Err(InstallError::new("errSecInternalComponent"));
            }
            if self
                .files_under(bundle)
                .iter()
                .any(|f| f.extension().is_some_and(|e| e == "cstemp"))
            {
                return Err(InstallError::new(
                    "invalid or unsupported format for signature",
                ));
            }
            Ok(())
        }
        fn launchctl(&mut self, args: &[&str]) -> Result<(), InstallError> {
            self.calls.push(format!("launchctl {}", args.join(" ")));
            if args[0] == "bootout" {
                // launchd stops the job it unloads.
                self.daemon_running = false;
            }
            if self.launchctl_fails && args[0] == "bootstrap" {
                return Err(InstallError::new("Bootstrap failed: 5"));
            }
            if self.kickstart_fails && args[0] == "kickstart" {
                return Err(InstallError::new("Could not find service"));
            }
            Ok(())
        }
        fn daemon_running(&self) -> bool {
            self.daemon_running
        }
        fn stop_daemon(&mut self) {
            self.calls.push("stop-daemon".into());
            self.daemon_running = false;
        }
        fn start_daemon(&mut self, clusia: &Path) {
            self.calls
                .push(format!("start-daemon {}", clusia.display()));
        }
    }

    fn env() -> InstallEnv {
        InstallEnv {
            home: "/Users/maria".into(),
            uid: 501,
            version: "0.1.0".into(),
            current_dir: "/src/clusia".into(),
            start_at_login: true,
            launch_agent: "/Users/maria/Library/LaunchAgents/io.github.rzorzal.clusia.daemon.plist"
                .into(),
            logs_dir: "/Users/maria/Library/Logs/Clusia".into(),
            applications_writable: true,
            usr_local_bin_writable: true,
            path: vec!["/usr/local/bin".into()],
        }
    }

    fn from_built() -> InstallOptions {
        InstallOptions {
            from: Some("/built".into()),
            ..InstallOptions::default()
        }
    }

    #[test]
    fn install_assembles_signs_swaps_registers_and_links() {
        let mut os = Fake::machine();
        let p = plan(&from_built(), &env());
        let report = execute(&p, &mut os).unwrap();

        let bundle = os.under(Path::new("/Applications/Clusia.app"));
        assert_eq!(bundle.len(), 10, "{bundle:?}");
        assert!(os.files.contains_key(Path::new(
            "/Applications/Clusia.app/Contents/MacOS/clusia-tray"
        )));
        let info = String::from_utf8_lossy(
            &os.files[Path::new("/Applications/Clusia.app/Contents/Info.plist")],
        )
        .into_owned();
        assert!(info.contains("<string>clusia-tray</string>"));
        assert_eq!(report.to_json()["signed_with"], "ad-hoc");
        assert_eq!(
            os.links[Path::new("/usr/local/bin/clusia")],
            PathBuf::from("/Applications/Clusia.app/Contents/MacOS/clusia")
        );
        let agent = String::from_utf8_lossy(
            &os.files[Path::new(
                "/Users/maria/Library/LaunchAgents/io.github.rzorzal.clusia.daemon.plist",
            )],
        )
        .into_owned();
        assert!(agent.contains("/Applications/Clusia.app/Contents/MacOS/clusiad"));

        let order: Vec<&str> = os.calls.iter().map(String::as_str).collect();
        let pos = |needle: &str| {
            order
                .iter()
                .position(|c| c.starts_with(needle))
                .unwrap_or_else(|| panic!("no call {needle} in {order:#?}"))
        };
        assert!(
            pos("codesign /Applications/.Clusia.app.installing")
                < pos("mv /Applications/.Clusia.app.installing")
        );
        assert!(pos("launchctl bootout") < pos("mv /Applications/.Clusia.app.installing"));
        assert!(pos("stop-daemon") < pos("mv /Applications/.Clusia.app.installing"));
        assert!(
            pos("mv /Applications/.Clusia.app.installing") < pos("launchctl bootstrap gui/501 ")
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn install_twice_is_idempotent() {
        let mut os = Fake::machine();
        let p = plan(&from_built(), &env());
        execute(&p, &mut os).unwrap();
        let first_files = os.files.clone();
        let first_links = os.links.clone();
        os.calls.clear();

        let report = execute(&p, &mut os).unwrap();
        assert_eq!(
            os.files, first_files,
            "the same bundle, agent and nothing else"
        );
        assert_eq!(os.links, first_links);
        assert!(
            os.calls_starting("ln ").is_empty(),
            "our link is already right"
        );
        assert!(!os.exists(&p.backup), "the old bundle is not left behind");
        assert!(!os.exists(&p.staging));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        // The only things touched are ours.
        for call in &os.calls {
            let touches_ours = [
                "/Applications/",
                "/Users/maria/Library/LaunchAgents/",
                "/usr/local/bin/clusia",
                "launchctl",
                "stop-daemon",
                "codesign",
            ]
            .iter()
            .any(|ours| call.contains(ours));
            assert!(touches_ours, "unexpected call {call}");
        }
        assert!(
            os.calls.iter().all(|c| !c.contains("Application Support")),
            "user data is never touched"
        );
    }

    #[test]
    fn a_missing_binary_stops_before_anything_is_written() {
        let mut os = Fake::machine();
        os.files.remove(Path::new("/built/clusia-app"));
        let err = execute(&plan(&from_built(), &env()), &mut os).unwrap_err();
        assert_eq!(err.0, "/built is missing clusia-app");
        assert!(os.calls.is_empty());
    }

    #[test]
    fn without_from_the_workspace_is_built_first() {
        let mut os = Fake::machine();
        execute(&plan(&InstallOptions::default(), &env()), &mut os).unwrap();
        assert_eq!(os.calls[0], "build /src/clusia");
    }

    #[test]
    fn ad_hoc_is_the_only_signature() {
        let mut os = Fake::machine();
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(
            os.calls_starting("codesign"),
            ["codesign /Applications/.Clusia.app.installing -"]
        );
        let text = report.describe();
        assert!(text.contains("signed ad hoc\n"), "{text}");
        assert!(!text.contains("identity"), "{text}");
    }

    #[test]
    fn an_unsigned_bundle_never_replaces_the_old_one() {
        let mut os = Fake::machine();
        let p = plan(&from_built(), &env());
        execute(&p, &mut os).unwrap();
        let old = PathBuf::from("/Applications/Clusia.app/Contents/old-marker");
        os.files.insert(old.clone(), b"old".to_vec());
        os.daemon_running = true;
        os.calls.clear();

        os.codesign_fails = true;
        let err = execute(&p, &mut os).unwrap_err();
        assert!(err.0.contains("could not be signed"), "{err}");
        assert!(err.0.contains("errSecInternalComponent"), "{err}");
        assert!(os.files.contains_key(&old), "the old bundle stays");
        assert!(!os.exists(&p.staging), "staging is removed");
        assert!(os.calls_starting("launchctl").is_empty(), "{:#?}", os.calls);
        assert!(os.calls_starting("stop-daemon").is_empty());
        assert!(os.calls_starting("mv ").is_empty());
        assert!(os.daemon_running);
    }

    #[test]
    fn a_stale_cstemp_is_removed_before_signing() {
        let mut os = Fake::machine();
        os.copies_leave_cstemp = true;
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let rm = "rm /Applications/.Clusia.app.installing/Contents/MacOS/clusia.cstemp";
        let pos = |needle: &str| {
            os.calls
                .iter()
                .position(|c| c == needle || c.starts_with(&format!("{needle} ")))
                .unwrap_or_else(|| panic!("no call {needle} in {:#?}", os.calls))
        };
        assert!(pos(rm) < pos("codesign"));
        assert!(
            os.under(Path::new("/Applications/Clusia.app"))
                .iter()
                .all(|f| f.extension().is_none_or(|e| e != "cstemp"))
        );
    }

    #[test]
    fn a_failed_swap_puts_the_old_bundle_back() {
        let mut os = Fake::machine();
        let p = plan(&from_built(), &env());
        execute(&p, &mut os).unwrap();
        os.files.insert(
            PathBuf::from("/Applications/Clusia.app/Contents/old-marker"),
            b"old".to_vec(),
        );
        os.daemon_running = true;
        os.calls.clear();
        os.renames_into_place_fail = 1;
        let err = execute(&p, &mut os).unwrap_err();
        assert_eq!(err.0, "disk full");
        assert!(
            os.files
                .contains_key(Path::new("/Applications/Clusia.app/Contents/old-marker")),
            "the previous bundle is back"
        );
        // launchd and the daemon are back as they were.
        let calls = &os.calls;
        let pos = |needle: &str| {
            calls
                .iter()
                .rposition(|c| c.starts_with(needle))
                .unwrap_or_else(|| panic!("no call {needle} in {calls:#?}"))
        };
        assert!(
            pos(&format!(
                "mv {} /Applications/Clusia.app",
                p.backup.display()
            )) < pos(&format!(
                "launchctl bootstrap gui/501 {}",
                p.launch_agent.display()
            ))
        );
        // The old agent starts at login, so launchd brings the daemon back by itself.
        assert!(os.calls_starting("start-daemon").is_empty(), "{calls:#?}");
        assert!(os.calls_starting("launchctl kickstart").is_empty());
    }

    #[test]
    fn a_failed_rollback_says_where_the_old_bundle_is() {
        let mut os = Fake::machine();
        let p = plan(&from_built(), &env());
        execute(&p, &mut os).unwrap();
        os.renames_into_place_fail = 2;
        let err = execute(&p, &mut os).unwrap_err();
        assert!(err.0.contains("disk full"), "{err}");
        assert!(err.0.contains(&p.backup.display().to_string()), "{err}");
    }

    #[test]
    fn a_backup_that_cannot_be_removed_is_a_warning() {
        let mut os = Fake::machine();
        let p = plan(&from_built(), &env());
        execute(&p, &mut os).unwrap();
        os.daemon_running = true;
        os.calls.clear();
        os.removing_fails.push(p.backup.clone());
        let report = execute(&p, &mut os).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains(&p.backup.display().to_string())),
            "{:?}",
            report.warnings
        );
        assert!(
            os.calls.contains(&format!(
                "launchctl bootstrap gui/501 {}",
                p.launch_agent.display()
            )),
            "{:#?}",
            os.calls
        );
        assert!(os.calls_starting("start-daemon").is_empty());
        assert!(os.links.contains_key(Path::new("/usr/local/bin/clusia")));
    }

    const FOREIGN_BUNDLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.other</string></dict></plist>"#;
    const FOREIGN_AGENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Label</key><string>com.example.other</string></dict></plist>"#;

    #[test]
    fn a_bundle_that_is_not_ours_is_never_replaced() {
        let mut os = Fake::machine();
        let info = PathBuf::from("/Applications/Clusia.app/Contents/Info.plist");
        os.files
            .insert(info.clone(), FOREIGN_BUNDLE.as_bytes().to_vec());
        let err = execute(&plan(&from_built(), &env()), &mut os).unwrap_err();
        assert!(err.0.contains("belongs to com.example.other"), "{err}");
        assert!(os.calls.is_empty(), "nothing was touched: {:?}", os.calls);
        assert_eq!(os.files[&info], FOREIGN_BUNDLE.as_bytes());

        // A folder with no readable Info.plist is not ours either.
        let mut os = Fake::machine();
        os.files.insert(
            "/Applications/Clusia.app/Contents/other".into(),
            b"x".to_vec(),
        );
        let err = execute(&plan(&from_built(), &env()), &mut os).unwrap_err();
        assert!(err.0.contains("is not a Clúsia bundle"), "{err}");
        assert!(os.calls.is_empty());

        // Nothing is built for a bundle that would be refused.
        let mut os = Fake::machine();
        os.files.insert(info, FOREIGN_BUNDLE.as_bytes().to_vec());
        execute(&plan(&InstallOptions::default(), &env()), &mut os).unwrap_err();
        assert!(os.calls.is_empty(), "{:?}", os.calls);
    }

    #[test]
    fn a_login_agent_that_is_not_ours_is_never_overwritten() {
        let mut os = Fake::machine();
        let agent = PathBuf::from(
            "/Users/maria/Library/LaunchAgents/io.github.rzorzal.clusia.daemon.plist",
        );
        os.files
            .insert(agent.clone(), FOREIGN_AGENT.as_bytes().to_vec());
        let err = execute(&plan(&from_built(), &env()), &mut os).unwrap_err();
        assert!(err.0.contains("is not Clúsia's login agent"), "{err}");
        assert!(os.calls.is_empty());
        assert_eq!(os.files[&agent], FOREIGN_AGENT.as_bytes());
    }

    #[test]
    fn a_running_daemon_comes_back_from_the_new_bundle_even_without_start_at_login() {
        let mut off = env();
        off.start_at_login = false;
        let mut os = Fake::machine();
        os.daemon_running = true;
        execute(&plan(&from_built(), &off), &mut os).unwrap();
        let kickstart = "launchctl kickstart gui/501/io.github.rzorzal.clusia.daemon".to_string();
        let calls = &os.calls;
        let pos = |needle: &str| calls.iter().position(|c| c.starts_with(needle)).unwrap();
        assert!(calls.contains(&kickstart), "{calls:#?}");
        assert!(pos("launchctl bootstrap") < pos("launchctl kickstart"));
        assert!(pos("stop-daemon") < pos("mv /Applications/.Clusia.app.installing"));
        assert!(
            os.calls_starting("start-daemon").is_empty(),
            "launchd runs it, so it is restarted after a crash"
        );

        let mut os = Fake::machine();
        execute(&plan(&from_built(), &off), &mut os).unwrap();
        assert!(
            os.calls_starting("launchctl kickstart").is_empty()
                && os.calls_starting("start-daemon").is_empty(),
            "none was running"
        );
    }

    #[test]
    fn with_start_at_login_launchd_alone_starts_the_daemon() {
        let mut os = Fake::machine();
        os.daemon_running = true;
        execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert!(
            os.calls_starting("start-daemon").is_empty()
                && os.calls_starting("launchctl kickstart").is_empty(),
            "a second daemon would race launchd's: {:#?}",
            os.calls
        );
    }

    #[test]
    fn a_daemon_launchd_cannot_start_is_started_directly() {
        let start = "start-daemon /Applications/Clusia.app/Contents/MacOS/clusia".to_string();
        let mut os = Fake::machine();
        os.daemon_running = true;
        os.launchctl_fails = true;
        execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(os.calls_starting("start-daemon"), [&start], "no job loaded");

        let mut off = env();
        off.start_at_login = false;
        let mut os = Fake::machine();
        os.daemon_running = true;
        os.kickstart_fails = true;
        execute(&plan(&from_built(), &off), &mut os).unwrap();
        assert_eq!(
            os.calls_starting("start-daemon"),
            [&start],
            "kickstart refused"
        );
    }

    #[test]
    fn a_launchd_failure_is_a_warning_not_an_error() {
        let mut os = Fake::machine();
        os.launchctl_fails = true;
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert!(report.warnings[0].contains("did not load the login agent"));
        assert!(os.links.contains_key(Path::new("/usr/local/bin/clusia")));
    }

    #[test]
    fn no_launchctl_leaves_launchd_and_the_daemon_alone() {
        let mut os = Fake::machine();
        let opts = InstallOptions {
            no_launchctl: true,
            ..from_built()
        };
        execute(&plan(&opts, &env()), &mut os).unwrap();
        os.daemon_running = true;
        execute(&plan(&opts, &env()), &mut os).unwrap();
        assert!(os.calls_starting("launchctl").is_empty());
        assert!(!os.calls.contains(&"stop-daemon".to_string()));
        assert!(os.calls_starting("start-daemon").is_empty());
    }

    #[test]
    fn a_command_that_is_not_our_link_is_left_alone() {
        let mut os = Fake::machine();
        os.files.insert(
            PathBuf::from("/usr/local/bin/clusia"),
            b"someone else's".to_vec(),
        );
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(report.cli_link, None);
        assert!(report.warnings[0].contains("left alone"));
        assert_eq!(
            os.files[Path::new("/usr/local/bin/clusia")],
            b"someone else's"
        );

        // A link to another program is somebody else's.
        let mut os = Fake::machine();
        os.links.insert(
            "/usr/local/bin/clusia".into(),
            "../Cellar/clusia/1.0/bin/clusia".into(),
        );
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(report.cli_link, None);
        assert!(
            report.warnings[0].contains("left alone"),
            "{:?}",
            report.warnings
        );
        assert_eq!(
            os.links[Path::new("/usr/local/bin/clusia")],
            PathBuf::from("../Cellar/clusia/1.0/bin/clusia")
        );
        assert!(os.calls_starting("rm /usr/local/bin/clusia").is_empty());

        // A link to a Clúsia installed elsewhere is ours to move.
        let mut os = Fake::machine();
        os.links.insert(
            "/usr/local/bin/clusia".into(),
            "/Users/maria/Applications/Clusia.app/Contents/MacOS/clusia".into(),
        );
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(report.cli_link, Some("/usr/local/bin/clusia".into()));
        assert_eq!(
            os.links[Path::new("/usr/local/bin/clusia")],
            PathBuf::from("/Applications/Clusia.app/Contents/MacOS/clusia")
        );
    }

    const DATA: &str = "/Users/maria/.clusia-elsewhere";

    #[test]
    fn uninstall_removes_only_ours() {
        let mut os = Fake::machine();
        let p = plan(&from_built(), &env());
        execute(&p, &mut os).unwrap();
        let data = PathBuf::from("/Users/maria/Library/Application Support/Clusia/config.toml");
        os.files.insert(data.clone(), b"[general]".to_vec());
        os.files
            .insert("/Applications/Other.app/Contents/x".into(), b"x".to_vec());
        os.calls.clear();

        let removed = uninstall(&p, Path::new(DATA), &mut os).unwrap();
        assert!(os.under(Path::new("/Applications/Clusia.app")).is_empty());
        assert!(!os.files.contains_key(&p.launch_agent));
        assert!(!os.links.contains_key(Path::new("/usr/local/bin/clusia")));
        assert!(os.files.contains_key(&data), "user data stays");
        assert!(
            os.files
                .contains_key(Path::new("/Applications/Other.app/Contents/x"))
        );
        assert!(removed.removed.contains(&p.app));
        assert_eq!(
            removed.kept,
            [format!("your reviews and settings in {DATA}")]
        );
        assert!(
            removed
                .notes
                .iter()
                .any(|n| n.contains(r#"security delete-identity -c "Clúsia Local""#)),
            "{removed:?}"
        );
        assert!(
            os.calls
                .contains(&"launchctl bootout gui/501/io.github.rzorzal.clusia.daemon".to_string())
        );

        // A bundle or an agent somebody else put there is left alone, and said so.
        let mut os = Fake::machine();
        os.files.insert(
            "/Applications/Clusia.app/Contents/Info.plist".into(),
            FOREIGN_BUNDLE.as_bytes().to_vec(),
        );
        os.files
            .insert(p.launch_agent.clone(), FOREIGN_AGENT.as_bytes().to_vec());
        let removed = uninstall(&p, Path::new(DATA), &mut os).unwrap();
        assert!(removed.removed.is_empty(), "{removed:?}");
        assert!(
            os.files
                .contains_key(Path::new("/Applications/Clusia.app/Contents/Info.plist"))
        );
        assert!(os.files.contains_key(&p.launch_agent));
        assert!(
            removed
                .kept
                .iter()
                .any(|k| k.contains("not a Clúsia bundle"))
        );
        assert!(
            removed
                .kept
                .iter()
                .any(|k| k.contains("not Clúsia's login agent"))
        );
        assert!(
            os.calls_starting("launchctl").is_empty(),
            "no unloading of someone else's job"
        );

        // A command somebody else put there is not ours to remove.
        let mut os = Fake::machine();
        os.links
            .insert("/usr/local/bin/clusia".into(), "/elsewhere/clusia".into());
        uninstall(&p, Path::new(DATA), &mut os).unwrap();
        assert!(os.links.contains_key(Path::new("/usr/local/bin/clusia")));

        // Staging and backup folders are removed only when they hold a Clúsia bundle.
        let mut os = Fake::machine();
        let ours = p.backup.join("Contents/Info.plist");
        os.files.insert(ours, render_info_plist(&p).into_bytes());
        let theirs = p.staging.join("Contents/Info.plist");
        os.files
            .insert(theirs.clone(), FOREIGN_BUNDLE.as_bytes().to_vec());
        let removed = uninstall(&p, Path::new(DATA), &mut os).unwrap();
        assert_eq!(removed.removed, std::slice::from_ref(&p.backup));
        assert!(os.files.contains_key(&theirs));
        assert!(
            removed
                .kept
                .iter()
                .any(|k| k.contains(".Clusia.app.installing (not a Clúsia bundle)")),
            "{removed:?}"
        );
    }
}
