//! The Homebrew formula builds the four programs from source and leaves every change to the
//! system (the app bundle, launchd, the Keychain) to `clusia install`.

const FORMULA: &str = include_str!("../../../packaging/homebrew/clusia.rb");
const WORKSPACE: &str = include_str!("../../../Cargo.toml");

fn caveats() -> &'static str {
    FORMULA
        .split("def caveats")
        .nth(1)
        .and_then(|rest| rest.split("\n  end").next())
        .expect("the formula has caveats")
}

#[test]
fn builds_the_four_programs_from_source() {
    assert!(FORMULA.contains(r#"depends_on "rust" => :build"#));
    assert!(FORMULA.contains("%w[clusia clusiad clusia-tray clusia-app]"));
    assert!(FORMULA.contains("std_cargo_args(root: libexec"));
    assert!(FORMULA.contains(r#"bin.install_symlink libexec/"bin/clusia""#));
    assert!(
        !FORMULA.contains("\n  url "),
        "no tagged release exists, so there is nothing to checksum"
    );
}

#[test]
fn the_repository_is_the_workspace_repository() {
    let repository = WORKSPACE
        .lines()
        .find_map(|l| l.strip_prefix("repository = "))
        .expect("workspace repository")
        .trim_matches('"');
    assert!(FORMULA.contains(&format!("homepage \"{repository}\"")));
    assert!(FORMULA.contains(&format!("head \"{repository}.git\"")));
}

#[test]
fn launchd_and_the_system_belong_to_clusia_install() {
    for forbidden in ["service do", "launchctl", "sudo", "/Library/LaunchAgents"] {
        assert!(!FORMULA.contains(forbidden), "{forbidden} in the formula");
    }
    let caveats = caveats();
    assert!(caveats.contains("clusia install --from #{opt_libexec}/bin"));
    assert!(caveats.contains("/Applications"));
    assert!(caveats.contains("brew upgrade"));
    assert!(caveats.contains("clusia uninstall"));
    assert!(caveats.contains("notifications"));
}

#[test]
fn the_formula_test_changes_nothing() {
    let test = FORMULA
        .split("  test do")
        .nth(1)
        .expect("the formula has a test");
    assert!(test.contains("--dry-run"));
    assert!(test.contains("Clusia.app"));
}
