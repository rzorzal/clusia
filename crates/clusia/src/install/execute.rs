//! Carries an install plan out, and takes it back. Every action on the machine goes through
//! `InstallOs`, so the order and the choices are tested against a fake.

use std::path::{Path, PathBuf};

use clusia_core::launch_agent;

use super::plan::{
    BINARIES, BUNDLE_ID, Content, Plan, SIGNING_NAME, SIGNING_NAME_ASCII, Source,
    render_info_plist, render_launch_agent,
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
    /// The code-signing identity named `name` in the keychain: its SHA-1.
    fn find_identity(&mut self, name: &str) -> Option<String>;
    /// Creates a self-signed code-signing identity named `name`; returns its SHA-1.
    fn create_identity(&mut self, name: &str) -> Result<String, InstallError>;
    /// Signs the bundle with `identity` (`-` is ad hoc) and checks the signature.
    fn codesign(&mut self, bundle: &Path, identity: &str) -> Result<(), InstallError>;
    fn launchctl(&mut self, args: &[&str]) -> Result<(), InstallError>;
    /// Asks a running daemon to stop; `true` when one was running.
    fn stop_daemon(&mut self) -> bool;
    /// Starts the daemon through `clusia` (the command inside the new bundle).
    fn start_daemon(&mut self, clusia: &Path);
}

/// How the bundle was signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signed {
    Identity(String),
    AdHoc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub app: PathBuf,
    pub signed: Signed,
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
            "signed_with": match &self.signed {
                Signed::Identity(name) => name.as_str(),
                Signed::AdHoc => "ad-hoc",
            },
            "launch_agent": self.launch_agent,
            "cli_link": self.cli_link,
            "warnings": self.warnings,
        })
    }

    pub fn describe(&self) -> String {
        let mut out = format!("Installed {}\n", self.app.display());
        out.push_str(&match &self.signed {
            Signed::Identity(name) => format!("  signed with the local identity \"{name}\"\n"),
            Signed::AdHoc => "  signed ad hoc (no local identity could be used)\n".into(),
        });
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
/// stays until the new one is in place, and comes back if the swap fails.
pub fn execute(plan: &Plan, os: &mut dyn InstallOs) -> Result<Report, InstallError> {
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

    refuse_foreign(plan, os)?;

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

    let mut warnings = Vec::new();
    let signed = sign(os, &plan.staging, &mut warnings);

    let mut daemon_was_running = false;
    if plan.launchctl {
        // The old daemon runs from the old bundle: unload it and stop it before the swap.
        let _ = os.launchctl(&["bootout", &plan.service_target()]);
        daemon_was_running = os.stop_daemon();
    }
    swap(plan, os)?;

    if let Some(dir) = plan.launch_agent.parent() {
        os.create_dir_all(dir)?;
    }
    os.write_file(
        &plan.launch_agent,
        render_launch_agent(plan, plan.start_at_login).as_bytes(),
        0o644,
    )?;
    if plan.launchctl {
        let agent = plan.launch_agent.display().to_string();
        if let Err(e) = os.launchctl(&["bootstrap", &plan.domain, &agent]) {
            warnings.push(format!("launchd did not load the login agent: {e}"));
        }
        // A daemon that was running comes back from the new bundle, whether or not the agent
        // starts at login.
        if daemon_was_running {
            os.start_daemon(&plan.cli.target);
        }
    }

    let cli_link = link_cli(plan, os, &mut warnings)?;
    Ok(Report {
        app: plan.app.clone(),
        signed,
        launch_agent: plan.launch_agent.clone(),
        cli_link,
        warnings,
        notes: plan.notes.clone(),
    })
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

/// The local identity when it can be found or made and used, else ad hoc. A stable identity
/// keeps macOS from treating each rebuild as a new app.
fn sign(os: &mut dyn InstallOs, bundle: &Path, warnings: &mut Vec<String>) -> Signed {
    let identity = [SIGNING_NAME, SIGNING_NAME_ASCII]
        .into_iter()
        .find_map(|name| {
            os.find_identity(name)
                .or_else(|| os.create_identity(name).ok())
                .map(|sha| (name, sha))
        });
    if let Some((name, sha)) = identity {
        match os.codesign(bundle, &sha) {
            Ok(()) => return Signed::Identity(name.to_string()),
            Err(e) => warnings.push(format!(
                "signing with \"{name}\" failed ({e}); signed ad hoc"
            )),
        }
    }
    match os.codesign(bundle, "-") {
        Ok(()) => Signed::AdHoc,
        Err(e) => {
            warnings.push(format!("the bundle could not be signed: {e}"));
            Signed::AdHoc
        }
    }
}

fn swap(plan: &Plan, os: &mut dyn InstallOs) -> Result<(), InstallError> {
    let had_old = os.exists(&plan.app);
    if had_old {
        if os.exists(&plan.backup) {
            os.remove_dir_all(&plan.backup)?;
        }
        os.rename(&plan.app, &plan.backup)?;
    }
    if let Err(e) = os.rename(&plan.staging, &plan.app) {
        if had_old {
            let _ = os.rename(&plan.backup, &plan.app);
        }
        return Err(e);
    }
    if had_old {
        os.remove_dir_all(&plan.backup)?;
    }
    Ok(())
}

/// The `clusia` command: our link is replaced, anything else is left alone.
fn link_cli(
    plan: &Plan,
    os: &mut dyn InstallOs,
    warnings: &mut Vec<String>,
) -> Result<Option<PathBuf>, InstallError> {
    let link = &plan.cli.link;
    match os.link_state(link) {
        LinkState::Link(to) if to == plan.cli.target => return Ok(Some(link.clone())),
        LinkState::Other => {
            warnings.push(format!(
                "{} exists and is not a link, so it was left alone",
                link.display()
            ));
            return Ok(None);
        }
        LinkState::Link(_) => os.remove_file(link)?,
        LinkState::Missing => {}
    }
    if let Some(dir) = link.parent() {
        os.create_dir_all(dir)?;
    }
    os.symlink(&plan.cli.target, link)?;
    Ok(Some(link.clone()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub removed: Vec<PathBuf>,
    pub kept: Vec<String>,
}

impl Removed {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "removed": self.removed, "kept": self.kept })
    }

    pub fn describe(&self) -> String {
        let mut out = String::from("Uninstalled Clúsia\n");
        for path in &self.removed {
            out.push_str(&format!("  removed {}\n", path.display()));
        }
        for line in &self.kept {
            out.push_str(&format!("  kept {line}\n"));
        }
        out
    }
}

/// Undoes `execute`: the agent, the bundle and our link. Your data and the signing identity
/// stay.
pub fn uninstall(plan: &Plan, os: &mut dyn InstallOs) -> Result<Removed, InstallError> {
    let mut removed = Vec::new();
    let mut kept = vec![
        "your reviews and settings in ~/Library/Application Support/Clusia".to_string(),
        format!("the \"{SIGNING_NAME}\" signing identity in your login keychain"),
    ];
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
    let ours = [
        (&plan.app, bundle_is_ours),
        (&plan.staging, true),
        (&plan.backup, true),
    ];
    for (dir, remove) in ours {
        if remove && os.exists(dir) {
            os.remove_dir_all(dir)?;
            removed.push(dir.clone());
        }
    }
    Ok(Removed { removed, kept })
}

/// The identities `security find-identity -p codesigning` lists, as `(sha1, name)`.
pub fn parse_identities(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let rest = line.trim().split_once(") ")?.1;
            let (sha, name) = rest.split_once(' ')?;
            let name = name.trim().strip_prefix('"')?.split('"').next()?;
            (sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| (sha.to_string(), name.to_string()))
        })
        .collect()
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
        identities: Vec<(String, String)>,
        can_create_identity: bool,
        codesign_refuses: Vec<String>,
        launchctl_fails: bool,
        daemon_running: bool,
        rename_into_place_fails: bool,
        calls: Vec<String>,
    }

    impl Fake {
        fn machine() -> Self {
            let mut fake = Fake {
                can_create_identity: true,
                ..Fake::default()
            };
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
            Ok(())
        }
        fn remove_dir_all(&mut self, dir: &Path) -> Result<(), InstallError> {
            self.calls.push(format!("rmdir {}", dir.display()));
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
            if self.rename_into_place_fails && to.ends_with("Clusia.app") && from != to {
                self.rename_into_place_fails = false;
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
        fn find_identity(&mut self, name: &str) -> Option<String> {
            self.identities
                .iter()
                .find(|(_, n)| n == name)
                .map(|(sha, _)| sha.clone())
        }
        fn create_identity(&mut self, name: &str) -> Result<String, InstallError> {
            self.calls.push(format!("create-identity {name}"));
            if !self.can_create_identity {
                return Err(InstallError::new("keychain locked"));
            }
            let sha = format!("{:040x}", self.identities.len() + 1);
            self.identities.push((sha.clone(), name.to_string()));
            Ok(sha)
        }
        fn codesign(&mut self, bundle: &Path, identity: &str) -> Result<(), InstallError> {
            self.calls
                .push(format!("codesign {} {identity}", bundle.display()));
            if self.codesign_refuses.iter().any(|i| i == identity) {
                return Err(InstallError::new("errSecInternalComponent"));
            }
            Ok(())
        }
        fn launchctl(&mut self, args: &[&str]) -> Result<(), InstallError> {
            self.calls.push(format!("launchctl {}", args.join(" ")));
            if self.launchctl_fails && args[0] == "bootstrap" {
                return Err(InstallError::new("Bootstrap failed: 5"));
            }
            Ok(())
        }
        fn stop_daemon(&mut self) -> bool {
            self.calls.push("stop-daemon".into());
            self.daemon_running
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
        assert_eq!(report.signed, Signed::Identity("Clúsia Local".into()));
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
        assert_eq!(
            os.identities.len(),
            1,
            "the identity is made once and reused"
        );
        assert!(os.calls_starting("create-identity").is_empty());
        assert!(
            os.calls_starting("ln ").is_empty(),
            "our link is already right"
        );
        assert!(!os.exists(&p.backup), "the old bundle is not left behind");
        assert!(!os.exists(&p.staging));
        assert_eq!(report.signed, Signed::Identity("Clúsia Local".into()));
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
    fn signing_falls_back_to_ascii_then_to_ad_hoc() {
        let mut os = Fake::machine();
        os.identities.push(("a".repeat(40), "Clusia Local".into()));
        os.can_create_identity = false;
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(report.signed, Signed::Identity("Clusia Local".into()));

        let mut os = Fake::machine();
        os.can_create_identity = false;
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(report.signed, Signed::AdHoc);
        assert_eq!(
            os.calls_starting("codesign").len(),
            1,
            "ad hoc is signed once"
        );
        assert!(
            os.calls
                .contains(&"codesign /Applications/.Clusia.app.installing -".to_string())
        );
    }

    #[test]
    fn a_refused_identity_falls_back_to_ad_hoc_with_a_warning() {
        let mut os = Fake::machine();
        os.identities.push(("b".repeat(40), "Clúsia Local".into()));
        os.codesign_refuses.push("b".repeat(40));
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert_eq!(report.signed, Signed::AdHoc);
        assert!(report.warnings[0].contains("signed ad hoc"));
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
        os.rename_into_place_fails = true;
        let err = execute(&p, &mut os).unwrap_err();
        assert_eq!(err.0, "disk full");
        assert!(
            os.files
                .contains_key(Path::new("/Applications/Clusia.app/Contents/old-marker")),
            "the previous bundle is back"
        );
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
        let start = "start-daemon /Applications/Clusia.app/Contents/MacOS/clusia".to_string();
        let calls = &os.calls;
        let pos = |needle: &str| calls.iter().position(|c| c.starts_with(needle)).unwrap();
        assert!(calls.contains(&start), "{calls:#?}");
        assert!(pos("launchctl bootstrap") < pos("start-daemon"));
        assert!(pos("stop-daemon") < pos("mv /Applications/.Clusia.app.installing"));

        let mut os = Fake::machine();
        execute(&plan(&from_built(), &off), &mut os).unwrap();
        assert!(
            os.calls_starting("start-daemon").is_empty(),
            "none was running"
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

        let mut os = Fake::machine();
        os.links
            .insert("/usr/local/bin/clusia".into(), "/elsewhere/clusia".into());
        let report = execute(&plan(&from_built(), &env()), &mut os).unwrap();
        assert!(
            report.cli_link.is_some(),
            "a link to somewhere else is ours to move"
        );
    }

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

        let removed = uninstall(&p, &mut os).unwrap();
        assert!(os.under(Path::new("/Applications/Clusia.app")).is_empty());
        assert!(!os.files.contains_key(&p.launch_agent));
        assert!(!os.links.contains_key(Path::new("/usr/local/bin/clusia")));
        assert!(os.files.contains_key(&data), "user data stays");
        assert!(
            os.files
                .contains_key(Path::new("/Applications/Other.app/Contents/x"))
        );
        assert!(removed.removed.contains(&p.app));
        assert!(
            removed
                .kept
                .iter()
                .any(|k| k.contains("Application Support"))
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
        let removed = uninstall(&p, &mut os).unwrap();
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
        uninstall(&p, &mut os).unwrap();
        assert!(os.links.contains_key(Path::new("/usr/local/bin/clusia")));
    }

    #[test]
    fn identities_are_read_from_the_security_listing() {
        let out = r#"  1) 1A2B3C4D5E6F708192A3B4C5D6E7F8091A2B3C4D "Clúsia Local" (CSSMERR_TP_NOT_TRUSTED)
  2) FFEEDDCCBBAA99887766554433221100FFEEDDCC "Apple Development: maria@example.com (ABCD123456)"
     2 identities found

  Valid identities only
  3) not-a-hash "Nope"
"#;
        let found = parse_identities(out);
        assert_eq!(
            found,
            [
                (
                    "1A2B3C4D5E6F708192A3B4C5D6E7F8091A2B3C4D".to_string(),
                    "Clúsia Local".to_string()
                ),
                (
                    "FFEEDDCCBBAA99887766554433221100FFEEDDCC".to_string(),
                    "Apple Development: maria@example.com (ABCD123456)".to_string()
                ),
            ]
        );
    }
}
