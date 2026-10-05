//! Renders the demo screens with the real binary. It opens a window, so it is ignored by default:
//! `cargo test -p clusia-app --test screenshot -- --ignored`. Set `CLUSIA_SCREENSHOTS=<dir>` to
//! keep the PNGs.

use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "opens a window"]
fn demo_screenshots() {
    let dir = tempfile::tempdir().unwrap();
    let shots: [(&str, &[&str]); 4] = [
        ("home-light", &["--demo"]),
        ("home-dark", &["--demo", "--dark"]),
        ("config-light", &["--demo", "--config"]),
        ("config-dark", &["--demo", "--dark", "--config"]),
    ];
    for (name, args) in shots {
        let out = dir.path().join(format!("{name}.png"));
        let status = Command::new(env!("CARGO_BIN_EXE_clusia-app"))
            .args(args)
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
