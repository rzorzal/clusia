//! What a list command printed, as the tests it names.

use clusia_core::tests::{KnownTest, RESULTS_MAX, ResultsFormat};
use serde_json::Value;

use super::strip_ansi;

/// The tests a `list` command printed, in the order it printed them, at most `RESULTS_MAX`.
pub fn parse_list(format: &ResultsFormat, output: &str) -> Vec<KnownTest> {
    let mut tests = match format {
        ResultsFormat::Libtest => libtest_list(output),
        ResultsFormat::GoJson => go_list(output),
        ResultsFormat::Pytest => pytest_list(output),
        ResultsFormat::Jest => jest_list(output),
        ResultsFormat::Junit { .. } | ResultsFormat::Plain => lines(output)
            .map(|name| KnownTest {
                name,
                file: None,
                module: None,
            })
            .collect(),
    };
    tests.truncate(RESULTS_MAX);
    tests
}

/// The non-blank lines of `output`, trimmed and without colour codes.
fn lines(output: &str) -> impl Iterator<Item = String> + '_ {
    output
        .lines()
        .map(|l| strip_ansi(l).trim().to_string())
        .filter(|l| !l.is_empty())
}

fn libtest_list(output: &str) -> Vec<KnownTest> {
    lines(output)
        .filter_map(|line| {
            let name = line.strip_suffix(": test")?.trim().to_string();
            (!name.is_empty()).then_some(KnownTest {
                module: name.rsplit_once("::").map(|(module, _)| module.to_string()),
                name,
                file: None,
            })
        })
        .collect()
}

fn is_go_test(name: &str) -> bool {
    name.starts_with("Test") && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// The package of an `ok  <package>  0.002s` or `FAIL  <package>` line.
fn go_package(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    match words.next() {
        Some("ok" | "FAIL") => words.next(),
        _ => None,
    }
}

fn go_list(output: &str) -> Vec<KnownTest> {
    let mut tests: Vec<KnownTest> = Vec::new();
    // The names printed since the last package line: that line says which package they are in.
    let mut waiting: Vec<usize> = Vec::new();
    for line in lines(output) {
        let (text, package) = match serde_json::from_str::<Value>(&line) {
            Ok(event) if line.starts_with('{') => (
                event["Output"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                event["Package"].as_str().map(str::to_string),
            ),
            _ => (line, None),
        };
        if is_go_test(&text) {
            if package.is_none() {
                waiting.push(tests.len());
            }
            tests.push(KnownTest {
                name: text,
                file: None,
                module: package,
            });
        } else if let Some(package) = go_package(&text) {
            for at in waiting.drain(..) {
                tests[at].module = Some(package.to_string());
            }
        }
    }
    tests
}

fn pytest_list(output: &str) -> Vec<KnownTest> {
    lines(output)
        .filter_map(|id| {
            let file = id.split("::").next().unwrap_or_default().to_string();
            (id.contains("::") && !file.contains(' ') && file.contains('.')).then_some(KnownTest {
                name: id,
                file: Some(file),
                module: None,
            })
        })
        .collect()
}

fn jest_list(output: &str) -> Vec<KnownTest> {
    let text = output.trim();
    if text.starts_with('[')
        && let Ok(Value::Array(items)) = serde_json::from_str(text)
    {
        return items
            .iter()
            .filter_map(|item| {
                Some(KnownTest {
                    name: item["name"].as_str()?.to_string(),
                    file: item["file"].as_str().map(str::to_string),
                    module: None,
                })
            })
            .collect();
    }
    lines(output)
        .map(|path| KnownTest {
            file: Some(path.clone()),
            name: path,
            module: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use clusia_core::tests::{KnownTest, RESULTS_MAX, ResultsFormat};

    use super::*;

    fn known(name: &str, file: Option<&str>, module: Option<&str>) -> KnownTest {
        KnownTest {
            name: name.into(),
            file: file.map(Into::into),
            module: module.map(Into::into),
        }
    }

    #[test]
    fn libtest_lists_its_tests_with_their_module() {
        let output =
            "foo::a: test\nfoo::b: test\nbar: test\nfast: benchmark\n\n3 tests, 1 benchmark\n";
        assert_eq!(
            parse_list(&ResultsFormat::Libtest, output),
            [
                known("foo::a", None, Some("foo")),
                known("foo::b", None, Some("foo")),
                known("bar", None, None),
            ]
        );
    }

    #[test]
    fn go_names_take_the_package_of_the_ok_line_below_them() {
        let output = "TestAdd\nTestSub\nok  \texample.com/widgets/add\t0.002s\n\
                      TestMul\nExampleMul\nok  \texample.com/widgets/mul\t0.003s\n\
                      ?   \texample.com/widgets/none\t[no test files]\n";
        assert_eq!(
            parse_list(&ResultsFormat::GoJson, output),
            [
                known("TestAdd", None, Some("example.com/widgets/add")),
                known("TestSub", None, Some("example.com/widgets/add")),
                known("TestMul", None, Some("example.com/widgets/mul")),
            ]
        );
    }

    #[test]
    fn go_json_events_carry_their_package() {
        let output = r#"{"Action":"output","Package":"example.com/widgets/add","Output":"TestAdd\n"}
{"Action":"output","Package":"example.com/widgets/add","Output":"ok  \texample.com/widgets/add\t0.002s\n"}
"#;
        assert_eq!(
            parse_list(&ResultsFormat::GoJson, output),
            [known("TestAdd", None, Some("example.com/widgets/add"))]
        );
    }

    #[test]
    fn go_names_with_no_package_line_keep_no_module() {
        assert_eq!(
            parse_list(&ResultsFormat::GoJson, "TestAdd\n"),
            [known("TestAdd", None, None)]
        );
    }

    #[test]
    fn pytest_lists_ids_with_their_file() {
        let output = "tests/test_pay.py::test_total\n\
                      tests/test_pay.py::TestCart::test_empty\n\
                      tests/test_pay.py::test_param[a b]\n\
                      \n\
                      3 tests collected in 0.01s\n";
        assert_eq!(
            parse_list(&ResultsFormat::Pytest, output),
            [
                known(
                    "tests/test_pay.py::test_total",
                    Some("tests/test_pay.py"),
                    None
                ),
                known(
                    "tests/test_pay.py::TestCart::test_empty",
                    Some("tests/test_pay.py"),
                    None
                ),
                known(
                    "tests/test_pay.py::test_param[a b]",
                    Some("tests/test_pay.py"),
                    None
                ),
            ]
        );
    }

    #[test]
    fn vitest_json_and_jest_file_lists_are_read() {
        let json =
            r#"[{"name":"cart > adds","file":"/work/src/cart.test.ts"},{"name":"cart > removes"}]"#;
        assert_eq!(
            parse_list(&ResultsFormat::Jest, json),
            [
                known("cart > adds", Some("/work/src/cart.test.ts"), None),
                known("cart > removes", None, None),
            ]
        );
        assert_eq!(
            parse_list(
                &ResultsFormat::Jest,
                "/work/src/a.test.js\n/work/src/b.test.js\n"
            ),
            [
                known("/work/src/a.test.js", Some("/work/src/a.test.js"), None),
                known("/work/src/b.test.js", Some("/work/src/b.test.js"), None),
            ]
        );
    }

    #[test]
    fn junit_and_plain_list_every_non_blank_line() {
        for format in [
            ResultsFormat::Junit { path: "r".into() },
            ResultsFormat::Plain,
        ] {
            assert_eq!(
                parse_list(&format, "one\n\n  two  \n"),
                [known("one", None, None), known("two", None, None)]
            );
        }
    }

    #[test]
    fn a_list_is_capped() {
        let output = (0..RESULTS_MAX + 10)
            .map(|n| format!("t{n}\n"))
            .collect::<String>();
        assert_eq!(
            parse_list(&ResultsFormat::Plain, &output).len(),
            RESULTS_MAX
        );
    }
}
