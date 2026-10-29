//! Which known tests are near a change: the ones the agent named, the ones in the module of a
//! changed function and the ones in a file the PR touches.

use std::collections::HashSet;

use clusia_core::tests::{KnownTest, NEAR_MAX, NearTest, NearWhy};

/// A function the PR changed, as `clusia-calls` knows it: its path, its qualified name and its
/// language (the name of the language pack: `rust`, `python`, `typescript`, `javascript`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedUnit {
    pub path: String,
    pub qualified: String,
    pub lang: String,
}

/// The known tests near the change. The agent's names come first, then those in the module of
/// a changed function, then those in a touched file; a test appears once, under the first
/// reason that found it, and the list holds at most `NEAR_MAX`. Agent names that no known test
/// carries are dropped when there is a list to check them against.
pub fn near_tests(
    known: &[KnownTest],
    touched: &[String],
    changed: &[ChangedUnit],
    agent: &[String],
) -> Vec<NearTest> {
    let mut found: Vec<NearTest> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |name: &str, why: NearWhy| {
        if found.len() < NEAR_MAX && seen.insert(name.to_string()) {
            found.push(NearTest {
                name: name.to_string(),
                why,
            });
        }
    };
    for name in agent.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if known.is_empty() || known.iter().any(|k| k.name == name) {
            add(name, NearWhy::Agent);
        }
    }
    let wanted: HashSet<String> = changed.iter().filter_map(unit_key).collect();
    for test in known {
        if test_keys(test).iter().any(|key| wanted.contains(key)) {
            add(&test.name, NearWhy::Module);
        }
    }
    for test in known {
        let in_touched = test
            .file
            .as_deref()
            .is_some_and(|file| touched.iter().any(|path| same_path(file, path)));
        if in_touched {
            add(&test.name, NearWhy::File);
        }
    }
    found
}

/// Equal, or one path is the other under a longer prefix: a runner reports a path relative to a
/// crate or a package, or an absolute one, and the PR touches a path relative to the repository.
fn same_path(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// The module of a Rust source path: what follows the last `src` folder, as `a::b`.
/// `mod.rs` is its folder; a top-level `lib.rs` or `main.rs` has none.
fn rust_module(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let at = parts.iter().rposition(|p| *p == "src")?;
    let mut segments: Vec<&str> = parts[at + 1..].to_vec();
    let stem = segments.pop()?.strip_suffix(".rs")?;
    let top_level = segments.is_empty();
    if stem != "mod" && !(top_level && matches!(stem, "lib" | "main")) {
        segments.push(stem);
    }
    (!segments.is_empty()).then(|| segments.join("::"))
}

/// `a::b::tests` as the keys of `a`, `a::b` and `a::b::tests`.
fn rust_prefixes(module: &str) -> Vec<String> {
    let segments: Vec<&str> = module.split("::").collect();
    (1..=segments.len())
        .map(|n| format!("rust:{}", segments[..n].join("::")))
        .collect()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parent_name(path: &str) -> Option<&str> {
    let mut parts = path.rsplit('/');
    parts.next()?;
    parts.next()
}

/// A script file's name up to its first `.`; an `index` file is named after its folder.
fn js_stem(path: &str) -> Option<&str> {
    let name = file_name(path);
    if name.starts_with("index.") {
        return parent_name(path);
    }
    name.split('.').next().filter(|s| !s.is_empty())
}

/// The key of a changed function's module, by language; none for a language the call index
/// does not cover.
fn unit_key(unit: &ChangedUnit) -> Option<String> {
    let name = file_name(&unit.path);
    match unit.lang.as_str() {
        "rust" => rust_module(&unit.path).map(|m| format!("rust:{m}")),
        "python" => {
            let stem = name.strip_suffix(".pyi").or(name.strip_suffix(".py"))?;
            let stem = if stem == "__init__" {
                parent_name(&unit.path)?
            } else {
                stem
            };
            Some(format!("py:{stem}"))
        }
        "typescript" | "javascript" => Some(format!("js:{}", js_stem(&unit.path)?)),
        _ => None,
    }
}

/// The module keys a known test answers to.
fn test_keys(test: &KnownTest) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(file) = &test.file {
        let name = file_name(file);
        let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
        match extension {
            "py" | "pyi" => {
                let stem = name.strip_suffix(&format!(".{extension}")).unwrap_or(name);
                let stem = stem
                    .strip_prefix("test_")
                    .or(stem.strip_suffix("_test"))
                    .unwrap_or(stem);
                keys.push(format!("py:{stem}"));
            }
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => {
                if let Some(stem) = js_stem(file) {
                    keys.push(format!("js:{stem}"));
                }
            }
            "rs" => {
                if let Some(module) = rust_module(file) {
                    keys.extend(rust_prefixes(&module));
                }
            }
            _ => {}
        }
    } else if let Some(module) = &test.module
        && !module.contains('/')
    {
        keys.extend(rust_prefixes(module));
    }
    keys
}

#[cfg(test)]
mod tests {
    use clusia_core::tests::{KnownTest, NEAR_MAX, NearTest, NearWhy};

    use super::*;

    fn test(name: &str, file: Option<&str>, module: Option<&str>) -> KnownTest {
        KnownTest {
            name: name.into(),
            file: file.map(Into::into),
            module: module.map(Into::into),
        }
    }

    fn unit(path: &str, lang: &str) -> ChangedUnit {
        ChangedUnit {
            path: path.into(),
            qualified: "f".into(),
            lang: lang.into(),
        }
    }

    fn near(name: &str, why: NearWhy) -> NearTest {
        NearTest {
            name: name.into(),
            why,
        }
    }

    #[test]
    fn the_agent_comes_first_then_module_then_file() {
        let known = vec![
            test("checks::tests::a", None, Some("checks::tests")),
            test("config::tests::b", None, Some("config::tests")),
            test("tests/it.rs::c", Some("crates/x/tests/it.rs"), None),
        ];
        let got = near_tests(
            &known,
            &["crates/x/tests/it.rs".into()],
            &[unit("crates/x/src/checks.rs", "rust")],
            &["config::tests::b".into()],
        );
        assert_eq!(
            got,
            [
                near("config::tests::b", NearWhy::Agent),
                near("checks::tests::a", NearWhy::Module),
                near("tests/it.rs::c", NearWhy::File),
            ]
        );
    }

    #[test]
    fn a_test_found_by_two_sources_keeps_the_first_reason() {
        let known = vec![test(
            "checks::tests::a",
            Some("src/checks.rs"),
            Some("checks::tests"),
        )];
        let got = near_tests(
            &known,
            &["src/checks.rs".into()],
            &[unit("src/checks.rs", "rust")],
            &["checks::tests::a".into()],
        );
        assert_eq!(got, [near("checks::tests::a", NearWhy::Agent)]);
    }

    #[test]
    fn agent_names_must_be_known_when_there_is_a_list() {
        let known = vec![test("a::t", None, Some("a"))];
        let agent = vec!["a::t".to_string(), "invented".to_string(), "  ".to_string()];
        assert_eq!(
            near_tests(&known, &[], &[], &agent),
            [near("a::t", NearWhy::Agent)]
        );
        assert_eq!(
            near_tests(&[], &[], &[], &agent),
            [
                near("a::t", NearWhy::Agent),
                near("invented", NearWhy::Agent)
            ],
            "with no list there is nothing to check against"
        );
    }

    #[test]
    fn rust_modules_match_by_prefix_and_lib_rs_has_no_module() {
        let known = vec![
            test("a::b::tests::x", None, Some("a::b::tests")),
            test("a::tests::y", None, Some("a::tests")),
            test("other::tests::z", None, Some("other::tests")),
        ];
        let nested = near_tests(&known, &[], &[unit("crates/x/src/a/b.rs", "rust")], &[]);
        assert_eq!(nested, [near("a::b::tests::x", NearWhy::Module)]);
        let folder = near_tests(&known, &[], &[unit("crates/x/src/a/mod.rs", "rust")], &[]);
        assert_eq!(
            folder,
            [
                near("a::b::tests::x", NearWhy::Module),
                near("a::tests::y", NearWhy::Module)
            ]
        );
        assert!(near_tests(&known, &[], &[unit("crates/x/src/lib.rs", "rust")], &[]).is_empty());
    }

    #[test]
    fn python_modules_match_by_the_test_file_stem() {
        let known = vec![
            test(
                "tests/test_pay.py::test_total",
                Some("tests/test_pay.py"),
                None,
            ),
            test("tests/pay_test.py::test_x", Some("tests/pay_test.py"), None),
            test(
                "tests/test_other.py::test_y",
                Some("tests/test_other.py"),
                None,
            ),
        ];
        let got = near_tests(&known, &[], &[unit("src/shop/pay.py", "python")], &[]);
        assert_eq!(
            got,
            [
                near("tests/test_pay.py::test_total", NearWhy::Module),
                near("tests/pay_test.py::test_x", NearWhy::Module),
            ]
        );
        let init = near_tests(&known, &[], &[unit("src/pay/__init__.py", "python")], &[]);
        assert_eq!(
            init.len(),
            2,
            "a package's __init__ is named after its folder"
        );
    }

    #[test]
    fn typescript_and_javascript_modules_match_test_and_spec_files() {
        let known = vec![
            test("cart adds", Some("src/cart.test.ts"), None),
            test("cart removes", Some("src/cart.spec.js"), None),
            test("order adds", Some("src/order.test.ts"), None),
        ];
        let got = near_tests(&known, &[], &[unit("src/cart.ts", "typescript")], &[]);
        assert_eq!(
            got,
            [
                near("cart adds", NearWhy::Module),
                near("cart removes", NearWhy::Module)
            ]
        );
        let index = near_tests(&known, &[], &[unit("src/cart/index.js", "javascript")], &[]);
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn a_language_never_matches_another_languages_module() {
        let known = vec![test(
            "tests/test_pay.py::t",
            Some("tests/test_pay.py"),
            None,
        )];
        assert!(near_tests(&known, &[], &[unit("crates/x/src/pay.rs", "rust")], &[]).is_empty());
    }

    #[test]
    fn other_languages_fall_back_to_the_touched_files() {
        let known = vec![
            test(
                "TestAdd",
                Some("add_test.go"),
                Some("example.com/widgets/pkg/add"),
            ),
            test(
                "TestMul",
                Some("mul_test.go"),
                Some("example.com/widgets/pkg/mul"),
            ),
        ];
        let got = near_tests(
            &known,
            &["pkg/add/add_test.go".into()],
            &[unit("pkg/add/add.go", "go")],
            &[],
        );
        assert_eq!(got, [near("TestAdd", NearWhy::File)]);
    }

    #[test]
    fn a_file_matches_when_one_path_ends_with_the_other() {
        let known = vec![
            test("a", Some("src/a.rs"), None),
            test("b", Some("/work/crates/x/src/b.rs"), None),
            test("c", Some("src/ab.rs"), None),
        ];
        let touched = vec![
            "crates/x/src/a.rs".to_string(),
            "crates/x/src/b.rs".to_string(),
        ];
        assert_eq!(
            near_tests(&known, &touched, &[], &[]),
            [near("a", NearWhy::File), near("b", NearWhy::File)]
        );
    }

    #[test]
    fn the_result_is_unique_and_capped() {
        let known: Vec<_> = (0..NEAR_MAX + 10)
            .map(|n| test(&format!("t{n}"), Some("src/a.rs"), None))
            .collect();
        let agent: Vec<String> = vec!["t0".into(), "t0".into()];
        let got = near_tests(&known, &["src/a.rs".into()], &[], &agent);
        assert_eq!(got.len(), NEAR_MAX);
        assert_eq!(got[0], near("t0", NearWhy::Agent));
        assert_eq!(got[1], near("t1", NearWhy::File));
        let names: std::collections::HashSet<_> = got.iter().map(|n| &n.name).collect();
        assert_eq!(names.len(), NEAR_MAX);
    }

    #[test]
    fn nothing_near_gives_an_empty_list() {
        let known = vec![test("a", Some("src/a.rs"), None)];
        assert!(
            near_tests(
                &known,
                &["src/z.rs".into()],
                &[unit("src/z.rs", "rust")],
                &[]
            )
            .is_empty()
        );
        assert!(near_tests(&[], &[], &[], &[]).is_empty());
    }
}
