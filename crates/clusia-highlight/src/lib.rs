//! Syntax highlighting for the review window (spec §2.4 of #75): text + path → per-line spans
//! with a highlight `Class`. Pure: no I/O, no state beyond the lazily built grammars. The app
//! maps classes to theme swatches, so colors stay in `theme.rs`.

mod classes;
mod languages;

use tree_sitter_highlight::{HighlightEvent, Highlighter};

pub use classes::{Class, class_for};
pub use languages::language_for;

use classes::NAMES;

/// Larger texts are shown plain: highlighting them would stall the frame.
pub const MAX_BYTES: usize = 512 * 1024;

/// A highlighted piece of one line: byte offsets inside that line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub class: Class,
}

/// One entry per `text.split('\n')` line. A line's spans cover it fully, in order, and
/// adjacent spans never share a class; an empty line has no spans. Unknown language, text over
/// `MAX_BYTES` or a highlighter error: every line is one `Plain` span.
pub fn highlight(path: &str, text: &str) -> Vec<Vec<Span>> {
    language_for(path)
        .filter(|_| text.len() <= MAX_BYTES)
        .and_then(|lang| highlight_as(lang, text))
        .unwrap_or_else(|| plain(text))
}

fn plain(text: &str) -> Vec<Vec<Span>> {
    text.split('\n')
        .map(|line| {
            if line.is_empty() {
                Vec::new()
            } else {
                vec![Span {
                    start: 0,
                    end: line.len(),
                    class: Class::Plain,
                }]
            }
        })
        .collect()
}

fn highlight_as(lang: &str, text: &str) -> Option<Vec<Vec<Span>>> {
    let config = languages::config(lang)?;
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, text.as_bytes(), None, None, |name| {
            languages::config(name)
        })
        .ok()?;
    let mut lines = Lines::new(text);
    let mut stack: Vec<Class> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => {
                stack.push(NAMES.get(h.0).map_or(Class::Plain, |n| class_for(n)));
            }
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                lines.push(start, end, stack.last().copied().unwrap_or(Class::Plain));
            }
        }
    }
    Some(lines.finish())
}

/// Cuts whole-text byte ranges into per-line spans.
struct Lines<'a> {
    text: &'a str,
    out: Vec<Vec<Span>>,
    /// Byte offset where the current line starts.
    line_start: usize,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            out: vec![Vec::new()],
            line_start: 0,
        }
    }

    fn push(&mut self, mut start: usize, end: usize, class: Class) {
        while start < end {
            let newline = self.text[start..end].find('\n').map(|i| start + i);
            let stop = newline.unwrap_or(end);
            if stop > start {
                self.add(start - self.line_start, stop - self.line_start, class);
            }
            match newline {
                Some(n) => {
                    self.out.push(Vec::new());
                    self.line_start = n + 1;
                    start = n + 1;
                }
                None => start = end,
            }
        }
    }

    fn add(&mut self, start: usize, end: usize, class: Class) {
        let line = self.out.last_mut().expect("at least one line");
        match line.last_mut() {
            Some(last) if last.class == class && last.end == start => last.end = end,
            _ => line.push(Span { start, end, class }),
        }
    }

    fn finish(self) -> Vec<Vec<Span>> {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One sample per language; each must produce at least one non-plain span.
    const SAMPLES: [(&str, &str); 22] = [
        (
            "a.rs",
            "/// doc\nfn main() { let x: u32 = 42; println!(\"hi {x}\\n\"); }\n",
        ),
        (
            "a.go",
            "package main\n// c\nfunc main() { x := 1; fmt.Println(\"hi\", x) }\n",
        ),
        (
            "a.c",
            "#include <stdio.h>\n/* c */\nint main(void) { int x = 1; return 0; }\n",
        ),
        (
            "a.cpp",
            "#include <vector>\nnamespace a { template<typename T> class V { public: T x = nullptr; }; }\n",
        ),
        (
            "a.cs",
            "using System;\n// c\nclass P { static void Main() { var x = 1; } }\n",
        ),
        (
            "a.swift",
            "import Foundation\n// c\nstruct A { let x: Int = 1; func f() -> String { return \"hi\" } }\n",
        ),
        (
            "a.kt",
            "package a\n// c\nfun main() { val x: Int = 1; println(\"hi $x\") }\n",
        ),
        (
            "A.java",
            "package a;\n// c\npublic class A { public static void main(String[] a) { int x = 1; } }\n",
        ),
        (
            "a.ts",
            "interface A { x: number }\n// c\nconst f = (a: A): string => `v ${a.x}`;\n",
        ),
        (
            "a.tsx",
            "const C = (p: { n: string }) => <div className=\"a\">{p.n}</div>;\n",
        ),
        (
            "a.js",
            "// c\nclass A { m(x) { return x * 2; } }\nconst el = <b>{1}</b>;\n",
        ),
        (
            "a.html",
            "<!DOCTYPE html>\n<!-- c -->\n<p class=\"x\">hi &amp;</p>\n",
        ),
        (
            "a.css",
            "/* c */\n.a > #b:hover { color: #fff; margin: 1px 2em; }\n",
        ),
        (
            "a.py",
            "# c\n@dec\ndef f(x: int) -> str:\n    return f\"v {x}\" if x else None\n",
        ),
        (
            "a.rb",
            "# c\nclass A\n  def m(x) = \"v #{x}\"\nend\nputs :sym, 1.5\n",
        ),
        (
            "a.php",
            "<h1>t</h1>\n<?php\n// c\nfunction f(int $x): string { return \"v $x\"; }\n",
        ),
        (
            "a.sh",
            "#!/bin/bash\n# c\nfor f in *.txt; do echo \"$f\"; done\n",
        ),
        ("a.json", "{\"a\": [1, 2.5, true, null], \"b\": \"s\\n\"}\n"),
        ("a.toml", "# c\n[package]\nname = \"x\"\nversion = 1\n"),
        ("a.yaml", "# c\nkey: value\nlist:\n  - 1\n  - true\n"),
        (
            "a.sql",
            "-- c\nSELECT a.id, COUNT(*) AS n FROM users a WHERE a.name = 'x';\n",
        ),
        (
            "a.md",
            "# Title\n\nSome *em* and `code` [link](http://x).\n\n- item\n",
        ),
    ];

    fn assert_covers(text: &str, lines: &[Vec<Span>]) {
        let split: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), split.len());
        for (line, spans) in split.iter().zip(lines) {
            let mut at = 0;
            for (i, s) in spans.iter().enumerate() {
                assert_eq!(s.start, at, "{line:?}: {spans:?}");
                assert!(s.end > s.start, "{line:?}: {spans:?}");
                if i > 0 {
                    assert_ne!(spans[i - 1].class, s.class, "merged: {spans:?}");
                }
                at = s.end;
            }
            assert_eq!(at, line.len(), "{line:?}: {spans:?}");
        }
    }

    fn class_of(path: &str, text: &str, line: usize, word: &str) -> Class {
        let lines = highlight(path, text);
        let src = text.split('\n').nth(line).unwrap();
        let at = src.find(word).unwrap();
        lines[line]
            .iter()
            .find(|s| s.start <= at && at < s.end)
            .unwrap()
            .class
    }

    #[test]
    fn every_language_highlights() {
        for (path, text) in SAMPLES {
            let lines = highlight(path, text);
            assert_covers(text, &lines);
            assert!(
                lines.iter().flatten().any(|s| s.class != Class::Plain),
                "{path}: {lines:?}"
            );
        }
    }

    #[test]
    fn rust_classes() {
        let text = "// note\nfn main() { let s = \"hi\"; let n = 42; }";
        assert_eq!(class_of("a.rs", text, 0, "note"), Class::Comment);
        assert_eq!(class_of("a.rs", text, 1, "fn"), Class::Keyword);
        assert_eq!(class_of("a.rs", text, 1, "main"), Class::Function);
        assert_eq!(class_of("a.rs", text, 1, "\"hi\""), Class::String);
        // tree-sitter-rust captures numeric literals as `constant.builtin`.
        assert_eq!(class_of("a.rs", text, 1, "42"), Class::Constant);
        assert_eq!(class_of("a.py", "n = 42", 0, "42"), Class::Number);
    }

    #[test]
    fn multi_line_constructs_color_every_line() {
        let text = "/* one\ntwo */\nlet x = 1;";
        let lines = highlight("a.rs", text);
        assert_eq!(
            lines[0],
            [Span {
                start: 0,
                end: 6,
                class: Class::Comment
            }]
        );
        assert_eq!(
            lines[1],
            [Span {
                start: 0,
                end: 6,
                class: Class::Comment
            }]
        );
        assert_covers(text, &lines);
    }

    #[test]
    fn function_keywords_are_keywords() {
        // `@keyword.function` must not resolve to the recognized name `function`.
        assert_eq!(class_of("a.kt", "fun main() {}", 0, "fun"), Class::Keyword);
        assert_eq!(
            class_of("a.swift", "func f() {}", 0, "func"),
            Class::Keyword
        );
        let deinit = "class A { deinit {} }";
        assert_eq!(class_of("a.swift", deinit, 0, "deinit"), Class::Keyword);
        assert_eq!(class_of("a.rs", "fn main() {}", 0, "fn"), Class::Keyword);
    }

    #[test]
    fn injections_are_highlighted() {
        let md = "# T\n\n```rust\nfn main() {}\n```\n";
        assert_eq!(class_of("a.md", md, 3, "fn"), Class::Keyword);
        let html = "<script>let a = 1;</script>";
        assert_eq!(class_of("a.html", html, 0, "let"), Class::Keyword);
    }

    #[test]
    fn unknown_or_huge_text_is_plain() {
        let plain = |end| {
            vec![Span {
                start: 0,
                end,
                class: Class::Plain,
            }]
        };
        assert_eq!(
            highlight("Makefile", "all:\n\n\tcargo build"),
            vec![plain(4), vec![], plain(12)]
        );
        let huge = "let x = 1;\n".repeat(MAX_BYTES / 11 + 1);
        let lines = highlight("a.rs", &huge);
        assert!(lines.iter().flatten().all(|s| s.class == Class::Plain));
        assert_eq!(lines.len(), huge.split('\n').count());
    }

    #[test]
    fn empty_lines_and_trailing_newline() {
        let lines = highlight("a.rs", "fn a() {}\n\nfn b() {}\n");
        assert_eq!(lines.len(), 4);
        assert!(lines[1].is_empty());
        assert!(lines[3].is_empty());
        assert_eq!(highlight("a.rs", ""), vec![Vec::<Span>::new()]);
    }

    #[test]
    fn unicode_offsets_are_bytes() {
        let text = "let s = \"ação\"; // é";
        let lines = highlight("a.rs", text);
        assert_covers(text, &lines);
        for s in &lines[0] {
            assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
        }
    }
}
