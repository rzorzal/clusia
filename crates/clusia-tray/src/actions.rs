//! What a click does: the window when it is installed (M5), otherwise the browser.
//! Window contract: `clusia-app --home <root> [--review owner/repo#n | --config]`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clusia_core::{Paths, PrRef};

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

pub fn plan(action: &Action, app: Option<&Path>, paths: &Paths) -> Option<Launch> {
    let home = paths.root().to_string_lossy().into_owned();
    let window = |extra: &[String]| {
        app.map(|a| Launch {
            program: a.to_path_buf(),
            args: [vec!["--home".to_string(), home.clone()], extra.to_vec()].concat(),
        })
    };
    match action {
        Action::OpenReview { pr, url } => {
            window(&["--review".into(), pr.to_string()]).or_else(|| browser(url))
        }
        Action::OpenUrl(url) => browser(url),
        Action::OpenHome => window(&[]),
        Action::OpenConfig => window(&["--config".into()]).or_else(|| {
            let file = paths.config_file();
            let args = if file.is_file() {
                vec!["-t".to_string(), file.to_string_lossy().into_owned()]
            } else {
                vec![home.clone()]
            };
            Some(Launch {
                program: PathBuf::from(OPEN),
                args,
            })
        }),
    }
}

/// Starts the program detached; a reaper thread collects its exit status.
pub fn launch(l: &Launch) {
    match Command::new(&l.program)
        .args(&l.args)
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
}
