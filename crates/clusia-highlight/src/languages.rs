//! The 22 languages (plus inline markdown, an injection target only): grammar, queries, names.
//! Queries are combined as each grammar's upstream `tree-sitter.json` does (C++ over C,
//! TypeScript and TSX over JavaScript).

use std::sync::OnceLock;

use tree_sitter::Language;
use tree_sitter_highlight::HighlightConfiguration;

use crate::classes::NAMES;

/// Language names, in `spec` order. The last one is reachable only through injections.
pub(crate) const LANGUAGES: [&str; 23] = [
    "rust",
    "go",
    "c",
    "cpp",
    "csharp",
    "swift",
    "kotlin",
    "java",
    "typescript",
    "tsx",
    "javascript",
    "html",
    "css",
    "python",
    "ruby",
    "php",
    "bash",
    "json",
    "toml",
    "yaml",
    "sql",
    "markdown",
    "markdown_inline",
];

/// Other names an injection query (or a fenced code block) may use for a language.
const ALIASES: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("golang", "go"),
    ("h", "c"),
    ("c++", "cpp"),
    ("cc", "cpp"),
    ("hpp", "cpp"),
    ("c#", "csharp"),
    ("cs", "csharp"),
    ("c_sharp", "csharp"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("ts", "typescript"),
    ("js", "javascript"),
    ("jsx", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("htm", "html"),
    ("py", "python"),
    ("rb", "ruby"),
    ("sh", "bash"),
    ("shell", "bash"),
    ("zsh", "bash"),
    ("yml", "yaml"),
    ("md", "markdown"),
];

struct Spec {
    language: Language,
    highlights: String,
    injections: &'static str,
    locals: String,
}

fn spec(
    language: Language,
    highlights: &[&str],
    injections: &'static str,
    locals: &[&str],
) -> Spec {
    Spec {
        language,
        highlights: highlights.concat(),
        injections,
        locals: locals.concat(),
    }
}

fn spec_for(name: &str) -> Option<Spec> {
    let s = match name {
        "rust" => spec(
            tree_sitter_rust::LANGUAGE.into(),
            &[tree_sitter_rust::HIGHLIGHTS_QUERY],
            tree_sitter_rust::INJECTIONS_QUERY,
            &[],
        ),
        "go" => spec(
            tree_sitter_go::LANGUAGE.into(),
            &[tree_sitter_go::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "c" => spec(
            tree_sitter_c::LANGUAGE.into(),
            &[tree_sitter_c::HIGHLIGHT_QUERY],
            "",
            &[],
        ),
        "cpp" => spec(
            tree_sitter_cpp::LANGUAGE.into(),
            &[
                tree_sitter_c::HIGHLIGHT_QUERY,
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            ],
            "",
            &[],
        ),
        "csharp" => spec(
            tree_sitter_c_sharp::LANGUAGE.into(),
            &[tree_sitter_c_sharp::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "swift" => spec(
            tree_sitter_swift::LANGUAGE.into(),
            &[tree_sitter_swift::HIGHLIGHTS_QUERY],
            tree_sitter_swift::INJECTIONS_QUERY,
            &[tree_sitter_swift::LOCALS_QUERY],
        ),
        "kotlin" => spec(
            tree_sitter_kotlin_sg::LANGUAGE.into(),
            &[tree_sitter_kotlin_sg::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "java" => spec(
            tree_sitter_java::LANGUAGE.into(),
            &[tree_sitter_java::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "typescript" => spec(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            &[
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY,
            ],
            tree_sitter_javascript::INJECTIONS_QUERY,
            &[
                tree_sitter_typescript::LOCALS_QUERY,
                tree_sitter_javascript::LOCALS_QUERY,
            ],
        ),
        "tsx" => spec(
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            &[
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY,
            ],
            tree_sitter_javascript::INJECTIONS_QUERY,
            &[
                tree_sitter_typescript::LOCALS_QUERY,
                tree_sitter_javascript::LOCALS_QUERY,
            ],
        ),
        "javascript" => spec(
            tree_sitter_javascript::LANGUAGE.into(),
            &[
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY,
            ],
            tree_sitter_javascript::INJECTIONS_QUERY,
            &[tree_sitter_javascript::LOCALS_QUERY],
        ),
        "html" => spec(
            tree_sitter_html::LANGUAGE.into(),
            &[tree_sitter_html::HIGHLIGHTS_QUERY],
            tree_sitter_html::INJECTIONS_QUERY,
            &[],
        ),
        "css" => spec(
            tree_sitter_css::LANGUAGE.into(),
            &[tree_sitter_css::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "python" => spec(
            tree_sitter_python::LANGUAGE.into(),
            &[tree_sitter_python::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "ruby" => spec(
            tree_sitter_ruby::LANGUAGE.into(),
            &[tree_sitter_ruby::HIGHLIGHTS_QUERY],
            "",
            &[tree_sitter_ruby::LOCALS_QUERY],
        ),
        "php" => spec(
            tree_sitter_php::LANGUAGE_PHP.into(),
            &[tree_sitter_php::HIGHLIGHTS_QUERY],
            tree_sitter_php::INJECTIONS_QUERY,
            &[],
        ),
        "bash" => spec(
            tree_sitter_bash::LANGUAGE.into(),
            &[tree_sitter_bash::HIGHLIGHT_QUERY],
            "",
            &[],
        ),
        "json" => spec(
            tree_sitter_json::LANGUAGE.into(),
            &[tree_sitter_json::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "toml" => spec(
            tree_sitter_toml_ng::LANGUAGE.into(),
            &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "yaml" => spec(
            tree_sitter_yaml::LANGUAGE.into(),
            &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "sql" => spec(
            tree_sitter_sequel::LANGUAGE.into(),
            &[tree_sitter_sequel::HIGHLIGHTS_QUERY],
            "",
            &[],
        ),
        "markdown" => spec(
            tree_sitter_md::LANGUAGE.into(),
            &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            &[],
        ),
        "markdown_inline" => spec(
            tree_sitter_md::INLINE_LANGUAGE.into(),
            &[tree_sitter_md::HIGHLIGHT_QUERY_INLINE],
            tree_sitter_md::INJECTION_QUERY_INLINE,
            &[],
        ),
        _ => return None,
    };
    Some(s)
}

/// The canonical language name for a language name or alias (case-insensitive).
pub(crate) fn canonical(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .copied()
        .find(|l| *l == lower)
        .or_else(|| ALIASES.iter().find(|(a, _)| *a == lower).map(|(_, l)| *l))
}

static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; LANGUAGES.len()] =
    [const { OnceLock::new() }; LANGUAGES.len()];

/// The highlight configuration of a language (by name or alias), built on first use.
/// `None` for unknown names and for a grammar whose queries fail to compile.
pub(crate) fn config(name: &str) -> Option<&'static HighlightConfiguration> {
    let name = canonical(name)?;
    let index = LANGUAGES.iter().position(|l| *l == name)?;
    CONFIGS[index]
        .get_or_init(|| {
            let s = spec_for(name)?;
            let mut config = HighlightConfiguration::new(
                s.language,
                name,
                &s.highlights,
                s.injections,
                &s.locals,
            )
            .ok()?;
            config.configure(NAMES);
            Some(config)
        })
        .as_ref()
}

/// The language of a file, from its name or extension. `None`: shown plain.
pub fn language_for(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => return Some("bash"),
        "Cargo.lock" => return Some("toml"),
        "Gemfile" | "Rakefile" => return Some("ruby"),
        _ => {}
    }
    let (_, ext) = name.rsplit_once('.')?;
    let lang = match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "java" => "java",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "html" | "htm" => "html",
        "css" => "css",
        "py" | "pyi" => "python",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "json" => "json",
        "toml" => "toml",
        "yml" | "yaml" => "yaml",
        "sql" => "sql",
        "md" | "markdown" => "markdown",
        _ => return None,
    };
    Some(lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_from_paths() {
        let cases = [
            ("src/main.rs", Some("rust")),
            ("cmd/x/main.go", Some("go")),
            ("lib/a.c", Some("c")),
            ("include/a.h", Some("c")),
            ("a.hpp", Some("cpp")),
            ("a.cc", Some("cpp")),
            ("a.cpp", Some("cpp")),
            ("a.cxx", Some("cpp")),
            ("App/Program.cs", Some("csharp")),
            ("Sources/A.swift", Some("swift")),
            ("app/Main.kt", Some("kotlin")),
            ("build.gradle.kts", Some("kotlin")),
            ("src/A.java", Some("java")),
            ("index.ts", Some("typescript")),
            ("types/index.d.ts", Some("typescript")),
            ("App.tsx", Some("tsx")),
            ("app.js", Some("javascript")),
            ("App.jsx", Some("javascript")),
            ("index.html", Some("html")),
            ("site.css", Some("css")),
            ("tool.py", Some("python")),
            ("app.rb", Some("ruby")),
            ("Gemfile", Some("ruby")),
            ("index.php", Some("php")),
            ("run.sh", Some("bash")),
            ("x.zsh", Some("bash")),
            (".zshrc", Some("bash")),
            ("package.json", Some("json")),
            ("Cargo.toml", Some("toml")),
            ("Cargo.lock", Some("toml")),
            (".github/workflows/ci.yml", Some("yaml")),
            ("a.yaml", Some("yaml")),
            ("db/schema.sql", Some("sql")),
            ("README.md", Some("markdown")),
            ("NOTES.markdown", Some("markdown")),
            ("SRC/MAIN.RS", Some("rust")),
            ("Dockerfile", None),
            ("Makefile", None),
            ("LICENSE", None),
            ("image.png", None),
        ];
        for (path, want) in cases {
            assert_eq!(language_for(path), want, "{path}");
        }
    }

    #[test]
    fn aliases_resolve_case_insensitively() {
        assert_eq!(canonical("Rust"), Some("rust"));
        assert_eq!(canonical("JS"), Some("javascript"));
        assert_eq!(canonical("markdown_inline"), Some("markdown_inline"));
        assert_eq!(canonical("c#"), Some("csharp"));
        assert_eq!(canonical("brainfuck"), None);
    }

    #[test]
    fn every_language_builds_its_configuration() {
        for name in LANGUAGES {
            assert!(config(name).is_some(), "{name}");
        }
    }
}
