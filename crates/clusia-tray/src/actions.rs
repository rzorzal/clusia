//! What a click does: the window when it is installed, otherwise the browser.
//! Window contract: `clusia-app --home <root> [--review owner/repo#n | --config]`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clusia_core::{Paths, PrRef};
use clusia_protocol::message::OpenTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open the review in the window, or the pull request in the browser without the window.
    OpenReview {
        pr: PrRef,
        url: String,
    },
    OpenUrl(String),
    OpenHome,
    OpenConfig,
    /// Config opened on one page (`git`, `notifications`, …).
    OpenConfigPage(String),
    /// Next sort order for that list (handled inside the tray, saved to the config).
    CycleSort(crate::model::ListId),
    /// Move one page back (-1) or forward (+1).
    Page(crate::model::ListId, i8),
    /// Show one repository (`owner/repo`) or all of them.
    Repository(Option<String>),
    /// Sync with GitHub now.
    Refresh,
    /// Open the Turn off menu (AppKit pops it up).
    TurnOff,
    PauseSync,
    ResumeSync,
    /// Stop the daemon (and with it the tray and the window).
    Quit,
}

impl Action {
    /// Changes the popover itself instead of launching something.
    pub fn is_local(&self) -> bool {
        matches!(
            self,
            Action::CycleSort(_) | Action::Page(..) | Action::Repository(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
}

const OPEN: &str = "/usr/bin/open";

/// `none` disables the window, any other value is its program; otherwise `clusia-app`
/// next to the tray's own executable when it exists.
pub fn app_binary_from(env: Option<String>, exe: Option<PathBuf>) -> Option<PathBuf> {
    match env.as_deref() {
        Some("none") => None,
        Some(program) => Some(PathBuf::from(program)),
        None => exe
            .map(|e| e.with_file_name("clusia-app"))
            .filter(|p| p.is_file()),
    }
}

/// Env: `CLUSIA_APP_BIN`.
pub fn app_binary() -> Option<PathBuf> {
    app_binary_from(
        std::env::var("CLUSIA_APP_BIN")
            .ok()
            .filter(|v| !v.is_empty()),
        std::env::current_exe().ok(),
    )
}

fn browser(url: &str) -> Option<Launch> {
    (url.starts_with("https://") || url.starts_with("http://")).then(|| Launch {
        program: PathBuf::from(OPEN),
        args: vec![url.to_string()],
    })
}

/// Without the window, Config is the settings file (or the folder before it exists).
fn config_fallback(paths: &Paths) -> Option<Launch> {
    let file = paths.config_file();
    let args = if file.is_file() {
        vec!["-t".to_string(), file.to_string_lossy().into_owned()]
    } else {
        vec![paths.root().to_string_lossy().into_owned()]
    };
    Some(Launch {
        program: PathBuf::from(OPEN),
        args,
    })
}

pub fn plan(action: &Action, app: Option<&Path>, paths: &Paths) -> Option<Launch> {
    let home = paths.root().to_string_lossy().into_owned();
    let window = |extra: &[String]| {
        app.map(|a| Launch {
            program: a.to_path_buf(),
            args: [vec!["--home".to_string(), home.clone()], extra.to_vec()].concat(),
        })
    };
    match action {
        Action::CycleSort(_)
        | Action::Page(..)
        | Action::Repository(_)
        | Action::Refresh
        | Action::TurnOff
        | Action::PauseSync
        | Action::ResumeSync
        | Action::Quit => None,
        Action::OpenReview { pr, url } => {
            window(&["--review".into(), pr.to_string()]).or_else(|| browser(url))
        }
        Action::OpenUrl(url) => browser(url),
        Action::OpenHome => window(&[]),
        Action::OpenConfigPage(page) => {
            window(&["--config-page".into(), page.clone()]).or_else(|| config_fallback(paths))
        }
        Action::OpenConfig => window(&["--config".into()]).or_else(|| config_fallback(paths)),
    }
}

/// What a clicked notification opens. `host` is `github.com` or the Enterprise host, for the
/// browser fallback. A Config page opens that page; a review thread opens its review.
pub fn action_for(target: &OpenTarget, host: &str) -> Action {
    match target {
        OpenTarget::Review { pr, .. } => Action::OpenReview {
            pr: pr.clone(),
            url: format!("https://{host}/{}/{}/pull/{}", pr.owner, pr.repo, pr.number),
        },
        OpenTarget::Home { .. } => Action::OpenHome,
        OpenTarget::Config { page } if page.is_empty() => Action::OpenConfig,
        OpenTarget::Config { page } => Action::OpenConfigPage(page.clone()),
    }
}

/// The program with its arguments, without the marker the daemon set on this tray: what the
/// tray starts was not started by the daemon.
fn command(l: &Launch) -> Command {
    let mut cmd = Command::new(&l.program);
    cmd.args(&l.args).env_remove(crate::launch::LAUNCHED_BY);
    cmd
}

/// Starts the program detached; a reaper thread collects its exit status.
pub fn launch(l: &Launch) {
    match command(l)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(program = %l.program.display(), error = %e, "cannot launch"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launched_program_does_not_inherit_the_launch_marker() {
        let cmd = command(&Launch {
            program: PathBuf::from("/bin/clusia-app"),
            args: vec!["--config".into()],
        });
        assert!(
            cmd.get_envs()
                .any(|(key, value)| key == crate::launch::LAUNCHED_BY && value.is_none()),
            "a window the daemon's tray opens is not told the daemon started it"
        );
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--config"]);
    }

    fn pr() -> PrRef {
        PrRef {
            owner: "rzorzal".into(),
            repo: "clusia".into(),
            number: 7,
        }
    }

    fn review(url: &str) -> Action {
        Action::OpenReview {
            pr: pr(),
            url: url.into(),
        }
    }

    fn open(args: &[&str]) -> Option<Launch> {
        Some(Launch {
            program: PathBuf::from(OPEN),
            args: args.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn review_with_the_window_opens_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let app = Path::new("/Applications/Clusia.app/Contents/MacOS/clusia-app");
        let home = dir.path().to_string_lossy().into_owned();
        assert_eq!(
            plan(
                &review("https://github.com/rzorzal/clusia/pull/7"),
                Some(app),
                &paths
            ),
            Some(Launch {
                program: app.into(),
                args: vec![
                    "--home".into(),
                    home.clone(),
                    "--review".into(),
                    "rzorzal/clusia#7".into()
                ],
            })
        );
        assert_eq!(
            plan(&Action::OpenHome, Some(app), &paths),
            Some(Launch {
                program: app.into(),
                args: vec!["--home".into(), home.clone()]
            })
        );
        assert_eq!(
            plan(&Action::OpenConfig, Some(app), &paths),
            Some(Launch {
                program: app.into(),
                args: vec!["--home".into(), home, "--config".into()]
            })
        );
    }

    #[test]
    fn review_without_app_opens_the_browser() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        assert_eq!(
            plan(
                &review("https://ghe.example.com/rzorzal/clusia/pull/7"),
                None,
                &paths
            ),
            open(&["https://ghe.example.com/rzorzal/clusia/pull/7"])
        );
        assert_eq!(plan(&Action::OpenHome, None, &paths), None);
    }

    #[test]
    fn only_web_urls_are_opened() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "-a Calculator",
            "",
        ] {
            assert_eq!(plan(&review(bad), None, &paths), None, "{bad}");
            assert_eq!(
                plan(&Action::OpenUrl(bad.into()), None, &paths),
                None,
                "{bad}"
            );
        }
        assert_eq!(
            plan(
                &Action::OpenUrl("http://localhost:8080/pulls".into()),
                None,
                &paths
            ),
            open(&["http://localhost:8080/pulls"])
        );
    }

    #[test]
    fn a_config_page_opens_the_window_on_that_page() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let app = Path::new("/Applications/Clusia.app/Contents/MacOS/clusia-app");
        let home = dir.path().to_string_lossy().into_owned();
        assert_eq!(
            plan(&Action::OpenConfigPage("git".into()), Some(app), &paths),
            Some(Launch {
                program: app.into(),
                args: vec![
                    "--home".into(),
                    home.clone(),
                    "--config-page".into(),
                    "git".into()
                ]
            })
        );
        // Without the window it falls back like Config does.
        assert_eq!(
            plan(&Action::OpenConfigPage("git".into()), None, &paths),
            open(&[&home])
        );
    }

    #[test]
    fn config_without_the_window_opens_the_file_or_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let home = dir.path().to_string_lossy().into_owned();
        assert_eq!(plan(&Action::OpenConfig, None, &paths), open(&[&home]));
        std::fs::write(paths.config_file(), "").unwrap();
        let file = paths.config_file().to_string_lossy().into_owned();
        assert_eq!(
            plan(&Action::OpenConfig, None, &paths),
            open(&["-t", &file])
        );
    }

    #[test]
    fn app_binary_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("clusia-tray");
        assert_eq!(
            app_binary_from(Some("none".into()), Some(exe.clone())),
            None
        );
        assert_eq!(
            app_binary_from(Some("/x/app".into()), None),
            Some(PathBuf::from("/x/app"))
        );
        assert_eq!(app_binary_from(None, Some(exe.clone())), None);
        std::fs::write(dir.path().join("clusia-app"), "").unwrap();
        assert_eq!(
            app_binary_from(None, Some(exe)),
            Some(dir.path().join("clusia-app"))
        );
    }

    #[test]
    fn a_clicked_notification_opens_its_target() {
        let target = OpenTarget::Review {
            pr: pr(),
            thread: Some("PRRT_1".into()),
        };
        assert_eq!(
            action_for(&target, "ghe.example.com"),
            review("https://ghe.example.com/rzorzal/clusia/pull/7")
        );
        assert_eq!(
            action_for(&OpenTarget::Home { pr: Some(pr()) }, "github.com"),
            Action::OpenHome
        );
        assert_eq!(
            action_for(
                &OpenTarget::Config {
                    page: "notifications".into()
                },
                "github.com"
            ),
            Action::OpenConfigPage("notifications".into())
        );
        assert_eq!(
            action_for(
                &OpenTarget::Config {
                    page: String::new()
                },
                "github.com"
            ),
            Action::OpenConfig,
            "no page is plain Config"
        );
    }

    #[test]
    fn local_actions_launch_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        for a in [
            Action::CycleSort(crate::model::ListId::Assigned),
            Action::Page(crate::model::ListId::Saved, 1),
            Action::Repository(None),
        ] {
            assert!(a.is_local());
            assert_eq!(plan(&a, Some(Path::new("/x/app")), &paths), None);
        }
        assert!(!Action::OpenHome.is_local());
    }
}
