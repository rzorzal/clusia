//! Renders the demo screens with the real binary. It opens a window, so it is ignored by default:
//! `cargo test -p clusia-app --test screenshot -- --ignored`. Set `CLUSIA_SCREENSHOTS=<dir>` to
//! keep the PNGs.

use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "opens a window"]
fn demo_screenshots() {
    let dir = tempfile::tempdir().unwrap();
    let base: [(&str, &[&str]); 14] = [
        ("home-light", &["--demo"]),
        ("home-dark", &["--demo", "--dark"]),
        ("config-light", &["--demo", "--config"]),
        ("config-dark", &["--demo", "--dark", "--config"]),
        ("review-light", &["--demo", "--scene", "diff"]),
        ("review-dark", &["--demo", "--dark", "--scene", "diff"]),
        ("split", &["--demo", "--scene", "split"]),
        ("comments", &["--demo", "--scene", "comments"]),
        ("finalize", &["--demo", "--scene", "finalize"]),
        ("whats-new", &["--demo", "--scene", "whats-new"]),
        ("loading", &["--demo", "--scene", "loading"]),
        ("load-failed", &["--demo", "--scene", "failed"]),
        ("leave", &["--demo", "--scene", "leave"]),
        ("palette", &["--demo", "--scene", "palette"]),
    ];
    let mut shots: Vec<(String, Vec<&str>)> = base
        .iter()
        .map(|(name, args)| (name.to_string(), args.to_vec()))
        .collect();
    // The rich-text, media and first-run scenes, each in both themes.
    for scene in [
        "composer",
        "composer-preview",
        "emoji",
        "gif",
        "rendered",
        "first-run",
        "config-media",
        "config-about",
    ] {
        shots.push((format!("{scene}-light"), vec!["--demo", "--scene", scene]));
        shots.push((
            format!("{scene}-dark"),
            vec!["--demo", "--dark", "--scene", scene],
        ));
    }
    for (name, args) in &shots {
        // A "-light" shot must not follow the macOS appearance.
        let light = name.ends_with("-light").then_some("--light");
        let out = dir.path().join(format!("{name}.png"));
        let status = Command::new(env!("CARGO_BIN_EXE_clusia-app"))
            .args(args)
            .args(light)
            .arg("--home")
            .arg(dir.path())
            .arg("--screenshot")
            .arg(&out)
            .env("CLUSIA_TRAY_BIN", "none")
            .status()
            .unwrap();
        assert!(status.success(), "{name}: {status}");
        let size = std::fs::metadata(&out).unwrap().len();
        assert!(size > 20_000, "{name}: only {size} bytes");
        if let Ok(keep) = std::env::var("CLUSIA_SCREENSHOTS") {
            std::fs::copy(&out, Path::new(&keep).join(format!("{name}.png"))).unwrap();
        }
    }
}
