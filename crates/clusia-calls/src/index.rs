//! The call index: the rows `extract` reads from each file, collected under a time budget and
//! sorted so the result does not depend on the order files arrive in.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tree_sitter::{ParseOptions, Parser};

use crate::extract::extract;
use crate::packs::{Grammar, compiled, grammar_for, language_name, pack_for};
use crate::{MAX_FILE_BYTES, MAX_FILES};

/// `<path>#<qualified>#<start line>`, unique in an index.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FunctionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Function {
    pub id: FunctionId,
    pub qualified: String,
    pub name: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Lines from the first one to the one that opens the body.
    pub signature_lines: u32,
    pub lang: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub from: FunctionId,
    pub name: String,
    pub qualifier: Option<String>,
    /// `x.f()`: the callee is a method of some value rather than a path.
    pub receiver: bool,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    pub path: String,
    pub module: String,
    /// Original names (an alias is dropped); `*` for a glob or namespace, `default` for a default import.
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallIndex {
    pub commit: String,
    pub functions: Vec<Function>,
    pub calls: Vec<Call>,
    pub imports: Vec<Import>,
    pub skipped: Vec<Skipped>,
    /// Source files in a language with no pack.
    pub unsupported: Vec<String>,
    pub dependencies: Vec<String>,
    /// The budget or the file cap stopped the build; the index holds what was read before.
    pub cut: bool,
}

pub struct SourceFile<'a> {
    pub path: &'a str,
    pub text: &'a str,
}

pub struct Indexer {
    commit: String,
    dependencies: Vec<String>,
    deadline: Instant,
    max_files: usize,
    taken: usize,
    parsers: HashMap<Grammar, Parser>,
    functions: Vec<Function>,
    calls: Vec<Call>,
    imports: Vec<Import>,
    skipped: Vec<Skipped>,
    unsupported: Vec<String>,
    cut: bool,
}

impl Indexer {
    pub fn new(commit: &str, dependencies: Vec<String>, deadline: Instant) -> Self {
        Self {
            commit: commit.to_string(),
            dependencies,
            deadline,
            max_files: MAX_FILES,
            taken: 0,
            parsers: HashMap::new(),
            functions: Vec::new(),
            calls: Vec::new(),
            imports: Vec::new(),
            skipped: Vec::new(),
            unsupported: Vec::new(),
            cut: false,
        }
    }

    /// Takes one file. `false` means the index is full or out of time and no further file
    /// should be offered; the file was not read.
    pub fn add(&mut self, file: SourceFile<'_>) -> bool {
        let pack = pack_for(file.path);
        if pack.is_none() && language_name(file.path).is_none() {
            return true;
        }
        if Instant::now() >= self.deadline || self.taken >= self.max_files {
            self.cut = true;
            return false;
        }
        let (Some(pack), Some(grammar)) = (pack, grammar_for(file.path)) else {
            self.unsupported.push(file.path.to_string());
            return true;
        };
        self.taken += 1;
        if file.text.len() as u64 > MAX_FILE_BYTES {
            self.skipped.push(Skipped {
                path: file.path.to_string(),
                reason: format!("larger than {MAX_FILE_BYTES} bytes"),
            });
            return true;
        }
        let deadline = self.deadline;
        let parser = self.parsers.entry(grammar).or_insert_with(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&compiled(grammar).language)
                .unwrap_or_else(|e| panic!("grammar {grammar:?} does not load: {e}"));
            parser
        });
        let mut read = |offset: usize, _| file.text.as_bytes().get(offset..).unwrap_or(&[]);
        let mut stop = |_: &tree_sitter::ParseState| {
            if Instant::now() >= deadline {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let tree = parser.parse_with_options(
            &mut read,
            None,
            Some(ParseOptions::new().progress_callback(&mut stop)),
        );
        let Some(tree) = tree else {
            // A stopped parse leaves the parser mid-file; the next file must start clean.
            parser.reset();
            self.skipped.push(Skipped {
                path: file.path.to_string(),
                reason: "stopped by the time budget".to_string(),
            });
            self.cut = true;
            return false;
        };
        let extracted = extract(file.path, pack.name, grammar, &tree, file.text);
        self.functions.extend(extracted.functions);
        self.calls.extend(extracted.calls);
        self.imports.extend(extracted.imports);
        true
    }

    pub fn finish(self) -> CallIndex {
        let Self {
            commit,
            mut dependencies,
            mut functions,
            mut calls,
            mut imports,
            mut skipped,
            mut unsupported,
            cut,
            ..
        } = self;
        functions.sort_by(|a, b| a.id.cmp(&b.id));
        calls.sort_by(|a, b| {
            (&a.from, a.line, &a.name, &a.qualifier).cmp(&(&b.from, b.line, &b.name, &b.qualifier))
        });
        imports.sort_by(|a, b| (&a.path, &a.module, &a.names).cmp(&(&b.path, &b.module, &b.names)));
        skipped.sort_by(|a, b| (&a.path, &a.reason).cmp(&(&b.path, &b.reason)));
        skipped.dedup();
        unsupported.sort();
        unsupported.dedup();
        dependencies.sort();
        dependencies.dedup();
        CallIndex {
            commit,
            functions,
            calls,
            imports,
            skipped,
            unsupported,
            dependencies,
            cut,
        }
    }
}

/// Dependency names from the manifests among `files`, matched by file name; a file that cannot
/// be read is ignored.
pub fn dependencies_from_manifests(files: &[SourceFile<'_>]) -> Vec<String> {
    let mut names = Vec::new();
    for file in files {
        let name = file.path.rsplit('/').next().unwrap_or(file.path);
        match name {
            "Cargo.toml" => cargo_dependencies(file.text, &mut names),
            "package.json" => package_dependencies(file.text, &mut names),
            "pyproject.toml" => pyproject_dependencies(file.text, &mut names),
            _ if name.starts_with("requirements") && name.ends_with(".txt") => {
                names.extend(file.text.lines().filter_map(requirement_name));
            }
            _ => {}
        }
    }
    names.sort();
    names.dedup();
    names
}

fn cargo_dependencies(text: &str, names: &mut Vec<String>) {
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return;
    };
    let tables = [
        manifest.get("dependencies"),
        manifest.get("dev-dependencies"),
        manifest
            .get("workspace")
            .and_then(|w| w.get("dependencies")),
    ];
    for table in tables.into_iter().flatten().filter_map(|t| t.as_table()) {
        names.extend(table.keys().map(|k| k.replace('-', "_")));
    }
}

fn package_dependencies(text: &str, names: &mut Vec<String>) {
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(table) = manifest.get(section).and_then(|s| s.as_object()) {
            names.extend(table.keys().cloned());
        }
    }
}

fn pyproject_dependencies(text: &str, names: &mut Vec<String>) {
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return;
    };
    let listed = manifest
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array());
    names.extend(
        listed
            .into_iter()
            .flatten()
            .filter_map(|d| d.as_str())
            .filter_map(requirement_name),
    );
}

/// The package name of a Python requirement line, normalised the way imports are written.
fn requirement_name(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
        return None;
    }
    let end = line
        .find(['[', '<', '>', '=', '!', '~', ';', ' ', '\t'])
        .unwrap_or(line.len());
    let name = line[..end].to_lowercase().replace('-', "_");
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::MAX_FILE_BYTES;
    use crate::testkit::{call_rows, cpu_timed, far, import_rows, index_of, rows, shapes};

    fn s(v: &str) -> String {
        v.to_string()
    }

    const RUST: &str = r#"use crate::util::norm;
use std::collections::{HashMap, HashSet};

pub struct Store<T> {
    value: T,
}

impl<T> Store<T> {
    pub fn token(&self) -> u32 {
        self.refresh();
        norm(1)
    }

    fn refresh(
        &self,
        margin: u32,
    ) {
        let _v: Vec<u8> = Vec::new();
        crate::util::norm(2);
        println!("x");
    }
}

trait Saver {
    fn save(&self) {
        helper();
    }
}

mod inner {
    pub fn helper() {}
}

fn main() {
    fn nested() {
        inner::helper();
    }
    nested();
}
"#;

    const TYPESCRIPT: &str = r#"import { norm, fmt as format } from "./util";
import axios from "axios";

export function helper(x: number): number {
  return norm(x);
}

export const run = async (a: string) => {
  helper(1);
  axios.get(a);
};

export class TokenStore {
  private cache = new Map();
  refresh(): void {
    this.token();
    helper(2);
  }
  token(): string {
    return format("x");
  }
  #secret(): void {}
}

export default function main() {
  new TokenStore().refresh();
}
"#;

    const PYTHON: &str = r#"import os
from .util import norm, fmt as format
from . import helpers


class Store:
    class Meta:
        def label(self):
            return os.getcwd()

    @staticmethod
    def token(x):
        return norm(x)

    def refresh(
        self,
        margin,
    ):
        self.token(1)
        Store.token(2)
        helpers.run()


def main():
    s = Store()
    s.refresh(3)
"#;

    #[test]
    fn rust_functions_calls_and_imports() {
        let index = index_of(&[("src/lib.rs", RUST)]);
        assert_eq!(index.commit, "c0ffee");
        assert_eq!(
            shapes(&index),
            vec![
                (s("Saver::save"), 25, 27, 1),
                (s("Store::refresh"), 14, 21, 4),
                (s("Store::token"), 9, 12, 1),
                (s("inner::helper"), 31, 31, 1),
                (s("main"), 34, 39, 1),
                (s("nested"), 35, 37, 1),
            ]
        );
        let token = index
            .functions
            .iter()
            .find(|f| f.qualified == "Store::token")
            .expect("fn");
        assert_eq!(token.id, FunctionId(s("src/lib.rs#Store::token#9")));
        assert_eq!(
            (
                token.name.as_str(),
                token.path.as_str(),
                token.lang.as_str()
            ),
            ("token", "src/lib.rs", "rust")
        );
        assert_eq!(
            call_rows(&index),
            vec![
                (s("Saver::save"), 26, s("helper"), None, false),
                (s("Store::refresh"), 18, s("new"), Some(s("Vec")), false),
                (
                    s("Store::refresh"),
                    19,
                    s("norm"),
                    Some(s("crate::util")),
                    false
                ),
                (s("Store::token"), 10, s("refresh"), Some(s("self")), true),
                (s("Store::token"), 11, s("norm"), None, false),
                (s("main"), 38, s("nested"), None, false),
                (s("nested"), 36, s("helper"), Some(s("inner")), false),
            ]
        );
        assert_eq!(
            import_rows(&index),
            rows(&[
                ("crate::util", &["norm"]),
                ("std::collections", &["HashMap", "HashSet"])
            ])
        );
        assert!(index.imports.iter().all(|i| i.path == "src/lib.rs"));
    }

    #[test]
    fn rust_use_forms_split_into_modules_and_names() {
        let src = "use super::*;\nuse tokio::sync::mpsc as chan;\nuse self::inner::{self, helper};\nuse a::{b::{c, d}, e};\n";
        let index = index_of(&[("src/lib.rs", src)]);
        assert_eq!(
            import_rows(&index),
            rows(&[
                ("a", &["e"]),
                ("a::b", &["c", "d"]),
                ("self::inner", &["helper"]),
                ("super", &["*"]),
                ("tokio::sync", &["mpsc"]),
            ])
        );
    }

    #[test]
    fn typescript_functions_arrows_methods_and_new() {
        let index = index_of(&[("src/store.ts", TYPESCRIPT)]);
        assert_eq!(
            shapes(&index),
            vec![
                (s("TokenStore.#secret"), 22, 22, 1),
                (s("TokenStore.refresh"), 15, 18, 1),
                (s("TokenStore.token"), 19, 21, 1),
                (s("helper"), 4, 6, 1),
                (s("main"), 25, 27, 1),
                (s("run"), 8, 11, 1),
            ]
        );
        assert_eq!(
            call_rows(&index),
            vec![
                (
                    s("TokenStore.refresh"),
                    16,
                    s("token"),
                    Some(s("this")),
                    true
                ),
                (s("TokenStore.refresh"), 17, s("helper"), None, false),
                (s("TokenStore.token"), 20, s("format"), None, false),
                (s("helper"), 5, s("norm"), None, false),
                (
                    s("main"),
                    26,
                    s("constructor"),
                    Some(s("TokenStore")),
                    false
                ),
                (s("main"), 26, s("refresh"), None, true),
                (s("run"), 9, s("helper"), None, false),
                (s("run"), 10, s("get"), Some(s("axios")), true),
            ]
        );
        assert_eq!(
            import_rows(&index),
            rows(&[("./util", &["norm", "fmt"]), ("axios", &["default"])])
        );
        assert!(index.functions.iter().all(|f| f.lang == "typescript"));
    }

    #[test]
    fn javascript_and_tsx_use_their_own_grammar() {
        let js = "export function a() {\n  return b();\n}\nconst b = function () { return 1; };\nmodule.exports = { a, b };\n";
        let index = index_of(&[("lib/a.js", js)]);
        assert_eq!(shapes(&index), vec![(s("a"), 1, 3, 1), (s("b"), 4, 4, 1)]);
        assert_eq!(call_rows(&index), vec![(s("a"), 2, s("b"), None, false)]);
        assert!(index.functions.iter().all(|f| f.lang == "javascript"));

        let tsx = "export function App() { return <div>{greet()}</div>; }\n";
        let index = index_of(&[("ui/app.tsx", tsx)]);
        assert_eq!(shapes(&index), vec![(s("App"), 1, 1, 1)]);
        assert_eq!(
            call_rows(&index),
            vec![(s("App"), 1, s("greet"), None, false)]
        );
        assert_eq!(index.functions[0].lang, "typescript");
    }

    #[test]
    fn python_methods_nested_classes_decorators_and_imports() {
        let index = index_of(&[("pkg/store.py", PYTHON)]);
        assert_eq!(
            shapes(&index),
            vec![
                (s("Meta.label"), 8, 9, 1),
                (s("Store.refresh"), 15, 21, 4),
                (s("Store.token"), 12, 13, 1),
                (s("main"), 24, 26, 1),
            ]
        );
        assert_eq!(
            call_rows(&index),
            vec![
                (s("Meta.label"), 9, s("getcwd"), Some(s("os")), true),
                (s("Store.refresh"), 19, s("token"), Some(s("self")), true),
                (s("Store.refresh"), 20, s("token"), Some(s("Store")), true),
                (s("Store.refresh"), 21, s("run"), Some(s("helpers")), true),
                (s("Store.token"), 13, s("norm"), None, false),
                (s("main"), 25, s("Store"), None, false),
                (s("main"), 26, s("refresh"), Some(s("s")), true),
            ]
        );
        assert_eq!(
            import_rows(&index),
            rows(&[
                (".", &["helpers"]),
                (".util", &["norm", "fmt"]),
                ("os", &[])
            ])
        );
    }

    #[test]
    fn source_without_a_pack_is_listed_and_other_files_are_ignored() {
        let index = index_of(&[
            ("main.go", "package main\n"),
            ("README.md", "# hi\n"),
            ("pkg/x.py", ""),
        ]);
        assert_eq!(index.unsupported, vec![s("main.go")]);
        assert!(index.skipped.is_empty() && index.functions.is_empty());
    }

    #[test]
    fn a_two_megabyte_minified_file_is_skipped_with_a_reason_and_the_rest_is_indexed() {
        let big = format!("var a={};", "1+".repeat(1_000_000) + "1");
        assert!(big.len() > 2_000_000);
        let (index, took) = cpu_timed(|| {
            index_of(&[
                ("dist/app.min.js", big.as_str()),
                ("ok.js", "function f() {}\n"),
            ])
        });
        assert!(took.as_secs() < 5, "{took:?}");
        assert_eq!(
            index.skipped,
            vec![Skipped {
                path: s("dist/app.min.js"),
                reason: format!("larger than {MAX_FILE_BYTES} bytes")
            }]
        );
        assert_eq!(shapes(&index), vec![(s("f"), 1, 1, 1)]);
    }

    #[test]
    fn a_file_with_a_syntax_error_does_not_panic_and_keeps_what_parsed() {
        let index = index_of(&[
            ("a.rs", "fn ok() {}\nfn broken( {{{ ]]\n"),
            ("b.py", "def (:\n  ))\n"),
        ]);
        assert!(
            index
                .functions
                .iter()
                .any(|f| f.qualified == "ok" && f.start_line == 1)
        );
        assert!(index.skipped.is_empty());
    }

    #[test]
    fn a_file_of_only_macros_has_no_functions_and_no_calls() {
        let index = index_of(&[("m.rs", "macro_rules! m {\n    () => { f() };\n}\nm!();\n")]);
        assert!(index.functions.is_empty() && index.calls.is_empty() && index.skipped.is_empty());
    }

    #[test]
    fn a_recursive_function_calls_itself() {
        let src = "fn f(n: u32) -> u32 {\n    if n == 0 { 0 } else { f(n - 1) }\n}\n";
        assert_eq!(
            call_rows(&index_of(&[("r.rs", src)])),
            vec![(s("f"), 2, s("f"), None, false)]
        );
    }

    #[test]
    fn two_functions_with_one_name_keep_unique_ids() {
        let apart = index_of(&[("a.rs", "fn f() {}\n\nfn f() {}\n")]);
        let ids: Vec<&str> = apart.functions.iter().map(|f| f.id.0.as_str()).collect();
        assert_eq!(ids, vec!["a.rs#f#1", "a.rs#f#3"]);
        let same_line = index_of(&[("a.rs", "fn f() {} fn f() {}\n")]);
        let ids: Vec<&str> = same_line
            .functions
            .iter()
            .map(|f| f.id.0.as_str())
            .collect();
        assert_eq!(ids, vec!["a.rs#f#1", "a.rs#f#1+2"]);
    }

    #[test]
    fn the_index_does_not_depend_on_the_order_files_arrive_in() {
        let files = [
            ("src/lib.rs", RUST),
            ("src/store.ts", TYPESCRIPT),
            ("pkg/store.py", PYTHON),
        ];
        let mut reversed = files;
        reversed.reverse();
        assert_eq!(index_of(&files), index_of(&reversed));
    }

    #[test]
    fn a_passed_deadline_refuses_files_and_marks_the_index_cut() {
        let mut indexer = Indexer::new("c", vec![], Instant::now());
        assert!(!indexer.add(SourceFile {
            path: "a.rs",
            text: "fn f() {}\n"
        }));
        let index = indexer.finish();
        assert!(index.cut && index.functions.is_empty());
    }

    #[test]
    fn the_file_count_is_capped() {
        let mut indexer = Indexer::new("c", vec![], far());
        indexer.max_files = 2;
        let results: Vec<bool> = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|p| {
                indexer.add(SourceFile {
                    path: p,
                    text: "fn f() {}\n",
                })
            })
            .collect();
        assert_eq!(results, vec![true, true, false]);
        let index = indexer.finish();
        assert!(index.cut);
        assert_eq!(index.functions.len(), 2);
    }

    #[test]
    fn dependencies_and_commit_are_kept_sorted_and_the_index_round_trips() {
        let index = Indexer::new("c", vec![s("b"), s("a"), s("b")], far()).finish();
        assert_eq!(
            (index.commit.as_str(), index.dependencies.clone()),
            ("c", vec![s("a"), s("b")])
        );
        let full = index_of(&[("src/lib.rs", RUST)]);
        let json = serde_json::to_string(&full).expect("json");
        assert_eq!(
            serde_json::from_str::<CallIndex>(&json).expect("back"),
            full
        );
    }

    #[test]
    fn manifests_give_the_dependency_names_of_every_language() {
        let files = [
            SourceFile {
                path: "Cargo.toml",
                text: "[package]\nname = \"demo\"\n\n[dependencies]\ntokio = \"1\"\nserde-json = { version = \"1\" }\n\n[dev-dependencies]\ntempfile = \"3\"\n\n[workspace.dependencies]\nclap = \"4\"\n",
            },
            SourceFile {
                path: "web/package.json",
                text: r#"{"dependencies":{"react":"18","@scope/pkg":"1"},"devDependencies":{"vitest":"1"},"peerDependencies":{"react-dom":"18"}}"#,
            },
            SourceFile {
                path: "pyproject.toml",
                text: "[project]\nname = \"demo\"\ndependencies = [\"requests>=2.0\", \"Django[argon2] ~=4.0\", \"typing-extensions; python_version<'3.9'\"]\n",
            },
            SourceFile {
                path: "requirements-dev.txt",
                text: "# tools\nflask==2.0\n-r base.txt\npytest>=7  # runner\n\n",
            },
            SourceFile {
                path: "bad/package.json",
                text: "{",
            },
            SourceFile {
                path: "src/main.rs",
                text: "fn main() {}",
            },
        ];
        assert_eq!(
            dependencies_from_manifests(&files),
            [
                "@scope/pkg",
                "clap",
                "django",
                "flask",
                "pytest",
                "react",
                "react-dom",
                "requests",
                "serde_json",
                "tempfile",
                "tokio",
                "typing_extensions",
                "vitest",
            ]
        );
    }
}
