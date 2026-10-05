//! Paints the demo popover offscreen in both appearances (no menu bar item, no daemon).

use std::process::Command;

fn png_size(png: &[u8]) -> (u32, u32) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let be = |i: usize| u32::from_be_bytes(png[i..i + 4].try_into().unwrap());
    (be(16), be(20))
}

#[test]
fn renders_the_demo_popover_in_both_appearances() {
    for dark in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("popover.png");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clusia-tray"));
        cmd.arg("--render").arg(&out);
        if dark {
            cmd.arg("--dark");
        }
        let o = cmd.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let (w, h) = png_size(&std::fs::read(&out).unwrap());
        assert!(w == 360 || w == 720, "width {w} (1x or 2x of 360 pt)");
        let scale = w / 360;
        assert!(h >= 400 * scale && h <= 720 * scale, "height {h}");
    }
}

#[test]
fn renders_the_status_icon_with_and_without_the_dot() {
    for news in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("icon.png");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clusia-tray"));
        cmd.arg("--render-icon").arg(&out);
        if news {
            cmd.arg("--news");
        }
        let o = cmd.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let (w, h) = png_size(&std::fs::read(&out).unwrap());
        assert!(h == 18 || h == 36, "height {h}");
        assert!(w >= h, "width {w}");
    }
}

#[test]
fn renders_over_a_backdrop() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("popover.png");
    let o = Command::new(env!("CARGO_BIN_EXE_clusia-tray"))
        .args(["--render"])
        .arg(&out)
        .args(["--backdrop", "#1E3A5F"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (w, _) = png_size(&std::fs::read(&out).unwrap());
    assert!(w == 360 || w == 720, "width {w}");
    let bad = Command::new(env!("CARGO_BIN_EXE_clusia-tray"))
        .args(["--render"])
        .arg(&out)
        .args(["--backdrop", "nope"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
}
