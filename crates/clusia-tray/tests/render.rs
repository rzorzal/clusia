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
        assert!(h >= 400 * scale && h <= 700 * scale, "height {h}");
    }
}
