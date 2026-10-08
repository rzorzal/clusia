//! What `clusia install` will do, as plain data. Nothing here touches the machine: the caller
//! describes the machine in an `InstallEnv`.

use std::path::{Path, PathBuf};

use clusia_core::config::SoundId;
use clusia_core::launch_agent::{self, LaunchAgent};
use plist::{Dictionary, Value};
use serde_json::json;

use super::assets::Asset;

pub const BUNDLE_ID: &str = "io.github.rzorzal.clusia";
pub const BUNDLE_DIR: &str = "Clusia.app";
/// The bundle's `CFBundleExecutable`: macOS only accepts notifications from this process.
pub const MAIN_EXECUTABLE: &str = "clusia-tray";
/// Everything that goes into `Contents/MacOS`.
pub const BINARIES: [&str; 4] = ["clusia-tray", "clusiad", "clusia-app", "clusia"];
/// The common name of the local code-signing identity.
pub const SIGNING_NAME: &str = "Clúsia Local";

/// What the user asked for on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallOptions {
    /// Where `Clusia.app` goes; default `/Applications`, else `~/Applications`.
    pub applications: Option<PathBuf>,
    /// Where the `clusia` link goes; default `/usr/local/bin`, else `~/.local/bin`.
    pub bin_dir: Option<PathBuf>,
    /// Where the LaunchAgent plist goes; default the user's `~/Library/LaunchAgents`.
    pub agents_dir: Option<PathBuf>,
    /// Binaries already built: a folder holding the four executables.
    pub from: Option<PathBuf>,
    /// The workspace to build when `from` is not given; default the current folder.
    pub workspace: Option<PathBuf>,
    /// Skip `launchctl` and stopping the daemon (isolated installs and tests).
    pub no_launchctl: bool,
}

/// What the machine looks like. The CLI fills it from the real system; tests write it by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallEnv {
    pub home: PathBuf,
    pub uid: u32,
    pub version: String,
    pub current_dir: PathBuf,
    /// `general.start_at_login`: the LaunchAgent's `RunAtLoad`.
    pub start_at_login: bool,
    /// The default LaunchAgent plist path.
    pub launch_agent: PathBuf,
    /// Where launchd writes the daemon's output.
    pub logs_dir: PathBuf,
    pub applications_writable: bool,
    pub usr_local_bin_writable: bool,
    /// The folders of `$PATH`, to say whether the CLI link is already reachable.
    pub path: Vec<PathBuf>,
}

/// Where the binaries come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `cargo build --release` in this workspace; the executables are in `target/release`.
    Build { workspace: PathBuf },
    /// A folder that already holds them.
    Dir(PathBuf),
}

/// What goes into one file of the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// A built executable, by file name.
    Binary(&'static str),
    Asset(Asset),
    /// `Info.plist`.
    InfoPlist,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    /// Relative to the bundle folder.
    pub path: PathBuf,
    pub content: Content,
}

/// The `clusia` command on `$PATH`: a symlink into the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliLink {
    pub link: PathBuf,
    pub target: PathBuf,
    /// The link's folder is on `$PATH`.
    pub on_path: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub version: String,
    pub source: Source,
    pub app: PathBuf,
    /// Where the new bundle is built and signed, next to `app`, before it replaces it.
    pub staging: PathBuf,
    /// Where the old bundle waits while the new one takes its place.
    pub backup: PathBuf,
    pub files: Vec<BundleFile>,
    pub launch_agent: PathBuf,
    pub start_at_login: bool,
    pub log: PathBuf,
    pub cli: CliLink,
    /// `gui/<uid>`, the launchd domain of the logged-in user.
    pub domain: String,
    pub launchctl: bool,
    /// Said when a default folder was not usable.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn daemon(&self) -> PathBuf {
        self.app.join("Contents/MacOS/clusiad")
    }

    pub fn service_target(&self) -> String {
        format!("{}/{}", self.domain, launch_agent::LABEL)
    }
}

pub fn plan(opts: &InstallOptions, env: &InstallEnv) -> Plan {
    let mut notes = Vec::new();
    let applications = match &opts.applications {
        Some(dir) => dir.clone(),
        None if env.applications_writable => PathBuf::from("/Applications"),
        None => {
            notes
                .push("/Applications is not writable here; installing into ~/Applications.".into());
            env.home.join("Applications")
        }
    };
    let app = applications.join(BUNDLE_DIR);
    let staging = applications.join(format!(".{BUNDLE_DIR}.installing"));
    let backup = applications.join(format!(".{BUNDLE_DIR}.previous"));

    let source = match &opts.from {
        Some(dir) => Source::Dir(dir.clone()),
        None => Source::Build {
            workspace: opts
                .workspace
                .clone()
                .unwrap_or_else(|| env.current_dir.clone()),
        },
    };

    let launch_agent = match &opts.agents_dir {
        Some(dir) => dir.join(format!("{}.plist", launch_agent::LABEL)),
        None => env.launch_agent.clone(),
    };

    Plan {
        version: env.version.clone(),
        source,
        files: bundle_files(),
        staging,
        backup,
        cli: cli_link(opts, env, &app, &mut notes),
        app,
        launch_agent,
        start_at_login: env.start_at_login,
        log: env.logs_dir.join("daemon.launchd.log"),
        domain: format!("gui/{}", env.uid),
        launchctl: !opts.no_launchctl,
        notes,
    }
}

fn bundle_files() -> Vec<BundleFile> {
    let file = |path: String, content| BundleFile {
        path: PathBuf::from(path),
        content,
    };
    let mut files = vec![file("Contents/Info.plist".into(), Content::InfoPlist)];
    for name in BINARIES {
        files.push(file(
            format!("Contents/MacOS/{name}"),
            Content::Binary(name),
        ));
    }
    files.push(file(
        "Contents/Resources/Clusia.icns".into(),
        Content::Asset(Asset::Icon),
    ));
    for id in SoundId::ALL {
        files.push(file(
            format!("Contents/Resources/{}.aiff", id.as_str()),
            Content::Asset(Asset::Sound(id)),
        ));
    }
    files
}

fn cli_link(
    opts: &InstallOptions,
    env: &InstallEnv,
    app: &Path,
    notes: &mut Vec<String>,
) -> CliLink {
    let dir = match &opts.bin_dir {
        Some(dir) => dir.clone(),
        None if env.usr_local_bin_writable => PathBuf::from("/usr/local/bin"),
        None => {
            notes.push(
                "/usr/local/bin is not writable here; linking clusia into ~/.local/bin.".into(),
            );
            env.home.join(".local/bin")
        }
    };
    let on_path = env.path.iter().any(|p| p == &dir);
    if !on_path {
        notes.push(format!(
            "{} is not on your PATH. Add it: export PATH=\"{}:$PATH\"",
            dir.display(),
            dir.display()
        ));
    }
    CliLink {
        link: dir.join("clusia"),
        target: app.join("Contents/MacOS/clusia"),
        on_path,
    }
}

/// `Contents/Info.plist`: the tray is the main executable and the app has no Dock icon of its
/// own (the window process sets one at run time).
pub fn render_info_plist(plan: &Plan) -> String {
    let text = |s: &str| Value::String(s.into());
    let mut d = Dictionary::new();
    d.insert("CFBundleIdentifier".into(), text(BUNDLE_ID));
    d.insert("CFBundleName".into(), text("Clusia"));
    d.insert("CFBundleDisplayName".into(), text("Clúsia"));
    d.insert("CFBundleExecutable".into(), text(MAIN_EXECUTABLE));
    d.insert("CFBundleIconFile".into(), text("Clusia"));
    d.insert("CFBundlePackageType".into(), text("APPL"));
    d.insert("CFBundleInfoDictionaryVersion".into(), text("6.0"));
    d.insert("CFBundleShortVersionString".into(), text(&plan.version));
    d.insert("CFBundleVersion".into(), text(&plan.version));
    d.insert("LSMinimumSystemVersion".into(), text("13.0"));
    d.insert("LSUIElement".into(), Value::Boolean(true));
    d.insert("NSHighResolutionCapable".into(), Value::Boolean(true));
    let mut out = Vec::new();
    match plist::to_writer_xml(&mut out, &Value::Dictionary(d)) {
        Ok(()) => String::from_utf8(out).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// The LaunchAgent for this install; `start_at_login` becomes `RunAtLoad`.
pub fn render_launch_agent(plan: &Plan, start_at_login: bool) -> String {
    launch_agent::render(&LaunchAgent {
        daemon: &plan.daemon(),
        log: &plan.log,
        start_at_login,
    })
}

impl Plan {
    /// What `--dry-run` prints.
    pub fn describe(&self) -> String {
        let mut out = format!("Clúsia {} install plan\n", self.version);
        out.push_str(&format!(
            "  bundle        {} (id {BUNDLE_ID}, main executable {MAIN_EXECUTABLE})\n",
            self.app.display()
        ));
        match &self.source {
            Source::Build { workspace } => out.push_str(&format!(
                "  binaries      cargo build --release in {}\n",
                workspace.display()
            )),
            Source::Dir(dir) => out.push_str(&format!("  binaries      from {}\n", dir.display())),
        }
        out.push_str("  contents\n");
        for f in &self.files {
            match &f.content {
                Content::Asset(asset) => out.push_str(&format!(
                    "    {}  ({} KB)\n",
                    f.path.display(),
                    asset.bytes().len().div_ceil(1024)
                )),
                Content::Binary(_) | Content::InfoPlist => {
                    out.push_str(&format!("    {}\n", f.path.display()));
                }
            }
        }
        out.push_str(&format!(
            "  staging       {} (the old bundle waits in {})\n",
            self.staging.display(),
            self.backup.display()
        ));
        out.push_str(&format!(
            "  signing       local identity \"{SIGNING_NAME}\" (made once, reused); ad-hoc if that fails\n"
        ));
        out.push_str(&format!(
            "  launch agent  {} (RunAtLoad = {}, restarts after a crash)\n",
            self.launch_agent.display(),
            self.start_at_login
        ));
        if self.launchctl {
            out.push_str(&format!(
                "  launchctl     bootout then bootstrap {}\n",
                self.service_target()
            ));
        } else {
            out.push_str("  launchctl     skipped\n");
        }
        out.push_str(&format!(
            "  cli           {} -> {}\n",
            self.cli.link.display(),
            self.cli.target.display()
        ));
        for note in &self.notes {
            out.push_str(&format!("  note          {note}\n"));
        }
        out
    }

    /// `describe` followed by the two property lists exactly as they would be written.
    pub fn describe_full(&self) -> String {
        format!(
            "{}\nInfo.plist\n{}\nLaunchAgent\n{}",
            self.describe(),
            render_info_plist(self),
            render_launch_agent(self, self.start_at_login)
        )
    }

    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "version": self.version,
            "bundle": self.app,
            "bundle_id": BUNDLE_ID,
            "main_executable": MAIN_EXECUTABLE,
            "files": self.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
            "launch_agent": self.launch_agent,
            "run_at_load": self.start_at_login,
            "cli_link": self.cli.link,
            "cli_target": self.cli.target,
            "cli_on_path": self.cli.on_path,
            "staging": self.staging,
            "backup": self.backup,
            "domain": self.domain,
            "launchctl": self.launchctl,
            "info_plist": render_info_plist(self),
            "launch_agent_plist": render_launch_agent(self, self.start_at_login),
            "notes": self.notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn env() -> InstallEnv {
        InstallEnv {
            home: "/Users/maria".into(),
            uid: 501,
            version: "0.1.0".into(),
            current_dir: "/Users/maria/Repos/clusia".into(),
            start_at_login: true,
            launch_agent: "/Users/maria/Library/LaunchAgents/io.github.rzorzal.clusia.daemon.plist"
                .into(),
            logs_dir: "/Users/maria/Library/Logs/Clusia".into(),
            applications_writable: true,
            usr_local_bin_writable: true,
            path: vec!["/usr/local/bin".into(), "/usr/bin".into()],
        }
    }

    fn paths(plan: &Plan) -> Vec<String> {
        plan.files
            .iter()
            .map(|f| f.path.display().to_string())
            .collect()
    }

    #[test]
    fn the_bundle_holds_every_binary_the_icon_and_the_sounds() {
        let p = plan(&InstallOptions::default(), &env());
        assert_eq!(p.app, PathBuf::from("/Applications/Clusia.app"));
        let files = paths(&p);
        for name in ["clusia-tray", "clusiad", "clusia-app", "clusia"] {
            assert!(files.contains(&format!("Contents/MacOS/{name}")), "{name}");
        }
        for name in ["leaf", "drop", "chime", "tick"] {
            assert!(
                files.contains(&format!("Contents/Resources/{name}.aiff")),
                "{name}"
            );
        }
        assert!(files.contains(&"Contents/Info.plist".to_string()));
        assert!(files.contains(&"Contents/Resources/Clusia.icns".to_string()));
        assert_eq!(files.len(), 10);
    }

    #[test]
    fn info_plist_makes_the_tray_the_main_executable() {
        let p = plan(&InstallOptions::default(), &env());
        let Value::Dictionary(d) =
            Value::from_reader_xml(render_info_plist(&p).as_bytes()).unwrap()
        else {
            panic!("not a dictionary");
        };
        let s = |k: &str| d.get(k).and_then(Value::as_string).map(str::to_string);
        assert_eq!(
            s("CFBundleIdentifier").as_deref(),
            Some("io.github.rzorzal.clusia")
        );
        assert_eq!(s("CFBundleExecutable").as_deref(), Some("clusia-tray"));
        assert_eq!(s("CFBundleIconFile").as_deref(), Some("Clusia"));
        assert_eq!(s("CFBundleShortVersionString").as_deref(), Some("0.1.0"));
        assert_eq!(d.get("LSUIElement").and_then(Value::as_boolean), Some(true));
    }

    #[test]
    fn the_launch_agent_points_into_the_bundle_and_follows_start_at_login() {
        let p = plan(&InstallOptions::default(), &env());
        let on = render_launch_agent(&p, true);
        assert_eq!(launch_agent::start_at_login(&on), Some(true));
        assert!(on.contains("/Applications/Clusia.app/Contents/MacOS/clusiad"));
        assert!(on.contains("/Users/maria/Library/Logs/Clusia/daemon.launchd.log"));
        assert_eq!(
            launch_agent::start_at_login(&render_launch_agent(&p, false)),
            Some(false)
        );
        assert_eq!(
            p.service_target(),
            "gui/501/io.github.rzorzal.clusia.daemon"
        );
    }

    #[test]
    fn applications_falls_back_to_the_home_folder() {
        let mut e = env();
        e.applications_writable = false;
        let p = plan(&InstallOptions::default(), &e);
        assert_eq!(p.app, PathBuf::from("/Users/maria/Applications/Clusia.app"));
        assert!(p.notes.iter().any(|n| n.contains("~/Applications")));
        assert_eq!(
            p.staging,
            PathBuf::from("/Users/maria/Applications/.Clusia.app.installing")
        );
    }

    #[test]
    fn the_cli_link_prefers_usr_local_bin_then_the_home_folder() {
        let p = plan(&InstallOptions::default(), &env());
        assert_eq!(p.cli.link, PathBuf::from("/usr/local/bin/clusia"));
        assert_eq!(
            p.cli.target,
            PathBuf::from("/Applications/Clusia.app/Contents/MacOS/clusia")
        );
        assert!(p.cli.on_path);
        assert!(p.notes.is_empty());

        let mut e = env();
        e.usr_local_bin_writable = false;
        let p = plan(&InstallOptions::default(), &e);
        assert_eq!(p.cli.link, PathBuf::from("/Users/maria/.local/bin/clusia"));
        assert!(!p.cli.on_path);
        assert!(
            p.notes
                .iter()
                .any(|n| n.contains("export PATH=\"/Users/maria/.local/bin:$PATH\""))
        );
    }

    #[test]
    fn explicit_folders_win_over_the_defaults() {
        let opts = InstallOptions {
            applications: Some("/tmp/apps".into()),
            bin_dir: Some("/tmp/bin".into()),
            agents_dir: Some("/tmp/agents".into()),
            from: Some("/tmp/built".into()),
            no_launchctl: true,
            ..InstallOptions::default()
        };
        let p = plan(&opts, &env());
        assert_eq!(p.app, PathBuf::from("/tmp/apps/Clusia.app"));
        assert_eq!(p.cli.link, PathBuf::from("/tmp/bin/clusia"));
        assert_eq!(
            p.launch_agent,
            PathBuf::from("/tmp/agents/io.github.rzorzal.clusia.daemon.plist")
        );
        assert_eq!(p.source, Source::Dir("/tmp/built".into()));
        assert!(!p.launchctl);
    }

    #[test]
    fn without_from_the_binaries_are_built_in_the_workspace() {
        let p = plan(&InstallOptions::default(), &env());
        assert_eq!(
            p.source,
            Source::Build {
                workspace: "/Users/maria/Repos/clusia".into()
            }
        );
        let opts = InstallOptions {
            workspace: Some("/src/clusia".into()),
            ..InstallOptions::default()
        };
        assert_eq!(
            plan(&opts, &env()).source,
            Source::Build {
                workspace: "/src/clusia".into()
            }
        );
    }

    #[test]
    fn the_dry_run_text_lists_the_plan() {
        let text = plan(&InstallOptions::default(), &env()).describe();
        for line in [
            "Clúsia 0.1.0 install plan",
            "/Applications/Clusia.app (id io.github.rzorzal.clusia, main executable clusia-tray)",
            "cargo build --release in /Users/maria/Repos/clusia",
            "Contents/MacOS/clusia-tray",
            "Contents/Resources/leaf.aiff",
            "local identity \"Clúsia Local\"",
            "RunAtLoad = true",
            "bootout then bootstrap gui/501/io.github.rzorzal.clusia.daemon",
            "/usr/local/bin/clusia -> /Applications/Clusia.app/Contents/MacOS/clusia",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
    }

    #[test]
    fn the_json_plan_names_the_bundle_and_the_link() {
        let v = plan(&InstallOptions::default(), &env()).to_json();
        assert_eq!(v["bundle"], "/Applications/Clusia.app");
        assert_eq!(v["cli_link"], "/usr/local/bin/clusia");
        assert_eq!(v["run_at_load"], true);
        assert_eq!(v["files"].as_array().unwrap().len(), 10);
    }
}
