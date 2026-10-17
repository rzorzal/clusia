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
    ] {
        shots.push((format!("{scene}-light"), vec!["--demo", "--scene", scene]));
        shots.push((
            format!("{scene}-dark"),
            vec!["--demo", "--dark", "--scene", scene],
        ));
    }
    for (name, args) in &shots {
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
            .status()
            .unwrap();
        assert!(status.success(), "{name}: {status}");
        let size = std::fs::metadata(&out).unwrap().len();
        assert!(size > 20_000, "{name}: only {size} bytes");
        if name.starts_with("diagrams-keywords") || name.starts_with("keywords-") {
            let share = non_background_share(&out, (0.45, 0.40, 0.75, 0.75));
            assert!(share > 0.02, "{name}: the 3D area is empty ({share})");
        }
        if let Ok(keep) = std::env::var("CLUSIA_SCREENSHOTS") {
            std::fs::copy(&out, Path::new(&keep).join(format!("{name}.png"))).unwrap();
        }
    }
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
