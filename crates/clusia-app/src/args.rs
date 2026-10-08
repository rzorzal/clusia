//! Command line: the window contract (`--home`, `--review`, `--config`) plus demo and
//! screenshot switches.

use std::path::PathBuf;

use clap::Parser;
use clusia_core::{PrRef, PrRefError};
use clusia_protocol::WindowTarget;

/// The demo states `--scene` can show.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    Diff,
    Split,
    Comments,
    Finalize,
    WhatsNew,
    Loading,
    Failed,
    Leave,
    Palette,
    Composer,
    ComposerPreview,
    Emoji,
    Gif,
    Rendered,
    FirstRun,
    ConfigGeneral,
    ConfigNotifications,
    ConfigNotificationsBottom,
    ConfigMedia,
    ConfigAbout,
}

#[derive(Parser, Debug, Clone, PartialEq)]
#[command(name = "clusia-app", version, about = "Clúsia window")]
pub struct Args {
    /// Clúsia home (defaults to CLUSIA_HOME or ~/Library/Application Support/Clusia).
    #[arg(long)]
    pub home: Option<PathBuf>,
    /// Open this review: owner/repo#n or a pull request URL.
    #[arg(long, value_name = "PR", conflicts_with = "config")]
    pub review: Option<String>,
    /// Open Config.
    #[arg(long)]
    pub config: bool,
    /// Open Config on one page (`general`, `git`, `notifications`, …).
    #[arg(long, value_name = "PAGE", conflicts_with_all = ["config", "review"])]
    pub config_page: Option<String>,
    /// Show demo data: no daemon, nothing is saved.
    #[arg(long)]
    pub demo: bool,
    /// With --demo: start in the dark theme.
    #[arg(long, requires = "demo")]
    pub dark: bool,
    /// With --demo: start in the light theme, whatever the macOS appearance is.
    #[arg(long, requires = "demo", conflicts_with = "dark")]
    pub light: bool,
    /// With --demo: stage the demo review in one state (screenshots).
    #[arg(long, value_enum, requires = "demo", hide = true)]
    pub scene: Option<Scene>,
    /// Render the first frames to this PNG, then exit.
    #[arg(long, value_name = "PNG")]
    pub screenshot: Option<PathBuf>,
    /// With --screenshot: save this many consecutive frames (`<name>-0.png`, `<name>-1.png`, …).
    #[arg(long, requires = "screenshot", default_value_t = 1, hide = true)]
    pub frames: u32,
    /// With --screenshot: advance the clock this many milliseconds per frame (animations).
    #[arg(long, value_name = "MS", requires = "screenshot", hide = true)]
    pub frame_ms: Option<u64>,
}

impl Args {
    pub fn target(&self) -> Result<WindowTarget, PrRefError> {
        if let Some(pr) = &self.review {
            return Ok(WindowTarget::Review {
                pr: pr.parse::<PrRef>()?,
            });
        }
        if let Some(page) = &self.config_page {
            return Ok(WindowTarget::ConfigPage { page: page.clone() });
        }
        Ok(if self.config {
            WindowTarget::Config
        } else {
            WindowTarget::Home
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("clusia-app").chain(args.iter().copied()))
    }

    #[test]
    fn targets() {
        assert_eq!(parse(&[]).unwrap().target().unwrap(), WindowTarget::Home);
        assert_eq!(
            parse(&["--config"]).unwrap().target().unwrap(),
            WindowTarget::Config
        );
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        for form in ["acme/widgets#7", "https://github.com/acme/widgets/pull/7"] {
            assert_eq!(
                parse(&["--review", form]).unwrap().target().unwrap(),
                WindowTarget::Review { pr: pr.clone() }
            );
        }
        assert!(parse(&["--review", "nope"]).unwrap().target().is_err());
    }

    #[test]
    fn conflicting_and_dependent_flags() {
        assert!(parse(&["--review", "a/b#1", "--config"]).is_err());
        assert!(parse(&["--dark"]).is_err(), "--dark needs --demo");
        let a = parse(&[
            "--demo",
            "--dark",
            "--screenshot",
            "/tmp/x.png",
            "--home",
            "/h",
        ])
        .unwrap();
        assert!(a.demo && a.dark);
        assert_eq!(a.screenshot, Some(PathBuf::from("/tmp/x.png")));
        assert_eq!(a.home, Some(PathBuf::from("/h")));
        assert_eq!((a.frames, a.frame_ms), (1, None));
        assert!(
            parse(&["--frames", "8"]).is_err(),
            "--frames needs --screenshot"
        );
        let a = parse(&[
            "--screenshot",
            "/tmp/x.png",
            "--frames",
            "8",
            "--frame-ms",
            "150",
        ])
        .unwrap();
        assert_eq!((a.frames, a.frame_ms), (8, Some(150)));
    }

    #[test]
    fn scenes_need_demo() {
        assert!(parse(&["--scene", "diff"]).is_err());
        assert_eq!(
            parse(&["--demo", "--scene", "whats-new"]).unwrap().scene,
            Some(Scene::WhatsNew)
        );
        assert_eq!(parse(&["--demo"]).unwrap().scene, None);
        assert!(parse(&["--demo", "--scene", "nope"]).is_err());
    }

    #[test]
    fn composer_media_and_first_run_scenes_parse() {
        for (name, scene) in [
            ("composer", Scene::Composer),
            ("composer-preview", Scene::ComposerPreview),
            ("emoji", Scene::Emoji),
            ("gif", Scene::Gif),
            ("rendered", Scene::Rendered),
            ("first-run", Scene::FirstRun),
            ("config-general", Scene::ConfigGeneral),
            ("config-notifications", Scene::ConfigNotifications),
            (
                "config-notifications-bottom",
                Scene::ConfigNotificationsBottom,
            ),
            ("config-media", Scene::ConfigMedia),
            ("config-about", Scene::ConfigAbout),
        ] {
            assert_eq!(
                parse(&["--demo", "--scene", name]).unwrap().scene,
                Some(scene),
                "{name}"
            );
        }
    }

    #[test]
    fn light_is_the_counterpart_of_dark() {
        assert!(parse(&["--demo", "--light"]).unwrap().light);
        assert!(parse(&["--light"]).is_err(), "--light needs --demo");
        assert!(parse(&["--demo", "--dark", "--light"]).is_err());
    }

    #[test]
    fn a_config_page_is_a_window_target() {
        assert_eq!(
            parse(&["--config-page", "git"]).unwrap().target().unwrap(),
            WindowTarget::ConfigPage { page: "git".into() }
        );
        assert!(parse(&["--config-page", "git", "--config"]).is_err());
        assert!(parse(&["--config-page", "git", "--review", "a/b#1"]).is_err());
    }
}
