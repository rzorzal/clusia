//! Language packs: which files the call index understands, their tree-sitter grammars and the
//! queries that find definitions, calls and imports in each.

use std::sync::OnceLock;

use tree_sitter::{Language, Query};

pub struct LanguagePack {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    /// Top-level names of the language's standard library; a call through one is outside the repo.
    pub stdlib: &'static [&'static str],
}

const NODE_BUILTINS: &[&str] = &[
    "fs",
    "path",
    "os",
    "http",
    "https",
    "crypto",
    "url",
    "util",
    "events",
    "stream",
    "child_process",
    "buffer",
    "assert",
    "zlib",
    "net",
    "readline",
    "timers",
    "worker_threads",
    "process",
    "querystring",
    "string_decoder",
    "tty",
    "dns",
    "cluster",
    "vm",
    "v8",
    "perf_hooks",
];

const PYTHON_STDLIB: &[&str] = &[
    "os",
    "sys",
    "re",
    "json",
    "typing",
    "pathlib",
    "collections",
    "itertools",
    "functools",
    "subprocess",
    "logging",
    "datetime",
    "math",
    "random",
    "time",
    "io",
    "abc",
    "dataclasses",
    "enum",
    "asyncio",
    "threading",
    "unittest",
    "argparse",
    "shutil",
    "tempfile",
    "hashlib",
    "base64",
    "urllib",
    "http",
    "socket",
    "sqlite3",
    "csv",
    "copy",
    "struct",
    "uuid",
    "contextlib",
    "inspect",
    "traceback",
    "warnings",
    "pickle",
    "secrets",
    "string",
    "operator",
    "heapq",
    "bisect",
    "textwrap",
    "platform",
    "signal",
    "queue",
    "weakref",
    "decimal",
    "fractions",
    "statistics",
    "glob",
    "fnmatch",
    "zipfile",
    "tarfile",
    "gzip",
    "configparser",
    "getpass",
    "pprint",
    "types",
    "importlib",
    "multiprocessing",
    "concurrent",
    "ssl",
    "email",
    "html",
    "xml",
    "codecs",
    "locale",
    "gc",
    "ast",
    "dis",
    "select",
    "selectors",
    "ctypes",
    "errno",
];

static PACKS: [LanguagePack; 4] = [
    LanguagePack {
        name: "rust",
        extensions: &["rs"],
        stdlib: &["std", "core", "alloc"],
    },
    LanguagePack {
        name: "typescript",
        extensions: &["ts", "tsx", "mts", "cts"],
        stdlib: NODE_BUILTINS,
    },
    LanguagePack {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        stdlib: NODE_BUILTINS,
    },
    LanguagePack {
        name: "python",
        extensions: &["py", "pyi"],
        stdlib: PYTHON_STDLIB,
    },
];

pub fn packs() -> &'static [LanguagePack] {
    &PACKS
}

fn extension(path: &str) -> Option<String> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let (_, ext) = file.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

pub fn pack_for(path: &str) -> Option<&'static LanguagePack> {
    let ext = extension(path)?;
    PACKS.iter().find(|p| p.extensions.contains(&ext.as_str()))
}

/// The display name of a source language, including those with no pack, so a caller can tell
/// "source we cannot read" from "not source".
pub fn language_name(path: &str) -> Option<&'static str> {
    let ext = extension(path)?;
    Some(match ext.as_str() {
        "rs" => "Rust",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" | "pyi" => "Python",
        "go" => "Go",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "C++",
        "cs" => "C#",
        "swift" => "Swift",
        "kt" | "kts" => "Kotlin",
        "java" => "Java",
        "rb" => "Ruby",
        "php" => "PHP",
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Grammar {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
}

pub(crate) struct Compiled {
    pub language: Language,
    pub definitions: Query,
    pub calls: Query,
    pub imports: Query,
}

pub(crate) fn grammar_for(path: &str) -> Option<Grammar> {
    match extension(path)?.as_str() {
        "rs" => Some(Grammar::Rust),
        "tsx" => Some(Grammar::Tsx),
        "ts" | "mts" | "cts" => Some(Grammar::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(Grammar::JavaScript),
        "py" | "pyi" => Some(Grammar::Python),
        _ => None,
    }
}

fn build(
    grammar: &str,
    language: Language,
    definitions: &str,
    calls: &str,
    imports: &str,
) -> Compiled {
    let query = |file: &str, source: &str| {
        Query::new(&language, source)
            .unwrap_or_else(|e| panic!("query {grammar}/{file}.scm does not compile: {e}"))
    };
    Compiled {
        definitions: query("definitions", definitions),
        calls: query("calls", calls),
        imports: query("imports", imports),
        language,
    }
}

macro_rules! queries {
    ($dir:literal) => {
        (
            include_str!(concat!("../queries/", $dir, "/definitions.scm")),
            include_str!(concat!("../queries/", $dir, "/calls.scm")),
            include_str!(concat!("../queries/", $dir, "/imports.scm")),
        )
    };
}

/// A grammar's queries, compiled on first use and kept: compiling is the expensive part.
pub(crate) fn compiled(grammar: Grammar) -> &'static Compiled {
    static RUST: OnceLock<Compiled> = OnceLock::new();
    static TYPESCRIPT: OnceLock<Compiled> = OnceLock::new();
    static TSX: OnceLock<Compiled> = OnceLock::new();
    static JAVASCRIPT: OnceLock<Compiled> = OnceLock::new();
    static PYTHON: OnceLock<Compiled> = OnceLock::new();
    match grammar {
        Grammar::Rust => RUST.get_or_init(|| {
            let (d, c, i) = queries!("rust");
            build("rust", tree_sitter_rust::LANGUAGE.into(), d, c, i)
        }),
        Grammar::TypeScript => TYPESCRIPT.get_or_init(|| {
            let (d, c, i) = queries!("typescript");
            build(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                d,
                c,
                i,
            )
        }),
        Grammar::Tsx => TSX.get_or_init(|| {
            let (d, c, i) = queries!("typescript");
            build("tsx", tree_sitter_typescript::LANGUAGE_TSX.into(), d, c, i)
        }),
        Grammar::JavaScript => JAVASCRIPT.get_or_init(|| {
            let (d, c, i) = queries!("javascript");
            build(
                "javascript",
                tree_sitter_javascript::LANGUAGE.into(),
                d,
                c,
                i,
            )
        }),
        Grammar::Python => PYTHON.get_or_init(|| {
            let (d, c, i) = queries!("python");
            build("python", tree_sitter_python::LANGUAGE.into(), d, c, i)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_packs_with_their_extensions() {
        let names: Vec<&str> = packs().iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["rust", "typescript", "javascript", "python"]);
        for (path, want) in [
            ("src/lib.rs", "rust"),
            ("a/b.ts", "typescript"),
            ("a/b.tsx", "typescript"),
            ("a/b.mts", "typescript"),
            ("a/b.cts", "typescript"),
            ("x.js", "javascript"),
            ("x.jsx", "javascript"),
            ("x.mjs", "javascript"),
            ("x.cjs", "javascript"),
            ("t.py", "python"),
            ("t.pyi", "python"),
            ("SRC/LIB.RS", "rust"),
        ] {
            assert_eq!(pack_for(path).map(|p| p.name), Some(want), "{path}");
        }
        for path in [
            "main.go",
            "README.md",
            "Cargo.toml",
            "Makefile",
            "x.d.ts.map",
        ] {
            assert!(pack_for(path).is_none(), "{path}");
        }
        assert_eq!(packs()[0].stdlib, ["std", "core", "alloc"]);
        assert!(packs()[1].stdlib.contains(&"fs") && packs()[3].stdlib.contains(&"os"));
    }

    #[test]
    fn language_names_cover_the_packs_and_the_source_languages_without_one() {
        for (path, want) in [
            ("lib.rs", Some("Rust")),
            ("a.tsx", Some("TypeScript")),
            ("a.jsx", Some("JavaScript")),
            ("x.py", Some("Python")),
            ("main.go", Some("Go")),
            ("x.c", Some("C")),
            ("x.h", Some("C")),
            ("x.cpp", Some("C++")),
            ("x.cs", Some("C#")),
            ("x.swift", Some("Swift")),
            ("x.kt", Some("Kotlin")),
            ("x.java", Some("Java")),
            ("x.rb", Some("Ruby")),
            ("x.php", Some("PHP")),
            ("README.md", None),
            ("Cargo.toml", None),
            ("data.json", None),
            ("noext", None),
        ] {
            assert_eq!(language_name(path), want, "{path}");
        }
    }

    #[test]
    fn every_query_compiles_and_names_the_captures_the_extractor_reads() {
        for grammar in [
            Grammar::Rust,
            Grammar::TypeScript,
            Grammar::Tsx,
            Grammar::JavaScript,
            Grammar::Python,
        ] {
            let c = compiled(grammar);
            let has = |q: &tree_sitter::Query, name: &str| q.capture_names().contains(&name);
            for name in ["def", "name"] {
                assert!(has(&c.definitions, name), "definitions lacks @{name}");
            }
            for name in ["call", "name"] {
                assert!(has(&c.calls, name), "calls lacks @{name}");
            }
            assert!(has(&c.imports, "import"));
        }
        assert!(
            compiled(Grammar::Rust)
                .definitions
                .capture_names()
                .contains(&"qualifier")
        );
        assert!(
            compiled(Grammar::Tsx)
                .calls
                .capture_names()
                .contains(&"constructor")
        );
    }

    #[test]
    fn a_tsx_path_picks_the_tsx_grammar() {
        assert!(matches!(grammar_for("ui/app.tsx"), Some(Grammar::Tsx)));
        assert!(matches!(
            grammar_for("ui/app.ts"),
            Some(Grammar::TypeScript)
        ));
        assert!(matches!(grammar_for("x.jsx"), Some(Grammar::JavaScript)));
        assert!(grammar_for("x.go").is_none());
    }
}
