//! Renders the demo screens with the real binary. It opens a window, so it is ignored by default:
//! `cargo test -p clusia-app --test screenshot -- --ignored`. Set `CLUSIA_SCREENSHOTS=<dir>` to
//! keep the PNGs. Set `CLUSIA_SCREENSHOT_ONLY=diff-calls,diff-story` to render only those scenes
//! (both themes).

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
        "config-general",
        "config-notifications",
        "config-notifications-bottom",
        "agent-chat",
        "config-harness",
        "permission",
        "security",
        "audits",
        "revalidated",
        "revalidated-banner",
        "config-harness-codex",
        "agent-chat-codex",
        "permission-codex",
        "first-run-both",
        "home-footer",
        "config-harness-custom",
        "agent-chat-custom",
        "permission-other",
        "first-run-custom",
        "diagrams",
        "diagrams-deps",
        "diagrams-sequence",
        "diagrams-er",
        "diagrams-usecase",
        "diagrams-dataflow",
        "diagrams-states",
        "diagrams-classes",
        "diagram-node",
        "diagrams-drawing",
        "diagrams-compare",
        "diagrams-keywords",
        "diagrams-keywords-usecase",
        "keywords-card",
        "keywords-xy",
        "diff-story",
        "diff-story-static",
        "diff-story-outdated",
        "diff-story-compare",
        "diff-calls",
        "diff-calls-unsupported",
    ] {
        shots.push((format!("{scene}-light"), vec!["--demo", "--scene", scene]));
        shots.push((
            format!("{scene}-dark"),
            vec!["--demo", "--dark", "--scene", scene],
        ));
    }
    // The Tests tab and Config › Tests: six scenes in both themes, five in light only.
    for scene in [
        "tests-empty",
        "tests-auto",
        "tests-setup",
        "config-tests",
        "tests-running",
        "tests-failed",
    ] {
        shots.push((format!("{scene}-light"), vec!["--demo", "--scene", scene]));
        shots.push((
            format!("{scene}-dark"),
            vec!["--demo", "--dark", "--scene", scene],
        ));
    }
    for scene in [
        "tests-generating",
        "tests-refused",
        "tests-stale",
        "tests-proposal",
        "tests-functions",
    ] {
        shots.push((format!("{scene}-light"), vec!["--demo", "--scene", scene]));
    }
    let only = std::env::var("CLUSIA_SCREENSHOT_ONLY").ok();
    let mut rendered = 0;
    for (name, args) in &shots {
        if !wanted(name, only.as_deref()) {
            continue;
        }
        // Only a shot that forces --dark may differ from light, so none follows the macOS appearance.
        let light = (!args.contains(&"--dark")).then_some("--light");
        let out = dir.path().join(format!("{name}.png"));
        let status = Command::new(env!("CARGO_BIN_EXE_clusia-app"))
            .args(args)
            .args(light)
            .arg("--home")
            .arg(dir.path())
            .arg("--screenshot")
            .arg(&out)
            .env("CLUSIA_TRAY_BIN", "none")
            .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
            .env("CLUSIA_CODEX_BIN", "/nonexistent/codex")
            .env("CLUSIA_DOCKER_BIN", "/nonexistent/docker")
            .env("CLUSIA_SANDBOX_EXEC_BIN", "/nonexistent/sandbox-exec")
            .status()
            .unwrap();
        assert!(status.success(), "{name}: {status}");
        let size = std::fs::metadata(&out).unwrap().len();
        assert!(size > 20_000, "{name}: only {size} bytes");
        if let Err(message) = clusia_app::render_check::check_png(&out) {
            panic!("{name}: {message}");
        }
        rendered += 1;
        if name.starts_with("diagrams-keywords") || name.starts_with("keywords-") {
            let share = non_background_share(&out, (0.45, 0.40, 0.75, 0.75));
            assert!(share > 0.02, "{name}: the 3D area is empty ({share})");
        }
        if name.starts_with("diff-calls-") && !name.starts_with("diff-calls-unsupported") {
            let share = non_background_share(&out, (0.28, 0.25, 0.72, 0.85));
            assert!(share > 0.02, "{name}: the call graph is empty");
        }
        if name.starts_with("tests-auto-") || name.starts_with("tests-running-") {
            let share = non_background_share(&out, (0.03, 0.47, 0.52, 0.85));
            assert!(share > 0.02, "{name}: the test list is empty ({share})");
        }
        if let Ok(keep) = std::env::var("CLUSIA_SCREENSHOTS") {
            std::fs::copy(&out, Path::new(&keep).join(format!("{name}.png"))).unwrap();
        }
    }
    assert!(
        rendered > 0,
        "CLUSIA_SCREENSHOT_ONLY={only:?} matched no scene"
    );
}

/// Whether the shot `name` is wanted under `only` (`CLUSIA_SCREENSHOT_ONLY`: scene names
/// separated by commas). A shot's scene is its name without `-light` or `-dark`; none, or an
/// empty list, wants every shot.
fn wanted(name: &str, only: Option<&str>) -> bool {
    let Some(only) = only.map(str::trim).filter(|o| !o.is_empty()) else {
        return true;
    };
    let scene = name
        .strip_suffix("-light")
        .or_else(|| name.strip_suffix("-dark"))
        .unwrap_or(name);
    only.split(',').map(str::trim).any(|s| s == scene)
}

/// The share of pixels in `crop` (left, top, right, bottom as fractions of the image) that
/// differ from the crop's most common colour.
fn non_background_share(png: &Path, crop: (f32, f32, f32, f32)) -> f32 {
    let img = image::open(png).unwrap().to_rgb8();
    let (w, h) = img.dimensions();
    let (x0, x1) = ((w as f32 * crop.0) as u32, (w as f32 * crop.2) as u32);
    let (y0, y1) = ((h as f32 * crop.1) as u32, (h as f32 * crop.3) as u32);
    let mut counts: std::collections::HashMap<[u8; 3], u32> = std::collections::HashMap::new();
    for y in y0..y1 {
        for x in x0..x1 {
            *counts.entry(img.get_pixel(x, y).0).or_default() += 1;
        }
    }
    let (bg, _) = counts.iter().max_by_key(|(_, n)| **n).expect("a crop");
    let far = |c: &[u8; 3]| {
        c.iter()
            .zip(bg)
            .map(|(a, b)| a.abs_diff(*b) as u32)
            .sum::<u32>()
            > 24
    };
    let total: u32 = counts.values().sum();
    let other: u32 = counts.iter().filter(|(c, _)| far(c)).map(|(_, n)| *n).sum();
    other as f32 / total as f32
}

#[test]
fn the_filter_keeps_every_shot_without_a_list() {
    for only in [None, Some(""), Some("  ")] {
        for name in ["home-light", "split", "diff-calls-dark"] {
            assert!(wanted(name, only), "{name} under {only:?}");
        }
    }
}

#[test]
fn the_filter_matches_a_scene_in_both_themes_and_nothing_else() {
    assert!(wanted("diff-calls-light", Some("diff-calls")));
    assert!(wanted("diff-calls-dark", Some("diff-story, diff-calls")));
    assert!(
        !wanted("diff-calls-unsupported-light", Some("diff-calls")),
        "a longer scene name is another scene"
    );
    assert!(!wanted("diff-story-light", Some("diff-calls")));
    assert!(
        wanted("split", Some("split")),
        "a base shot has no theme suffix"
    );
    assert!(wanted("review-dark", Some("review")));
    assert!(!wanted("review-dark", Some("diff")));
}
