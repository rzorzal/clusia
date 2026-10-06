//! Highlight classes: the few colors a theme gives code, and how capture names map to them.

/// What a piece of code is, as far as coloring goes. The app maps each class to a swatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    Keyword,
    String,
    Comment,
    Type,
    Function,
    Number,
    Constant,
    Operator,
    Punctuation,
    Property,
    Tag,
    Attribute,
    Variable,
    Plain,
}

impl Class {
    pub const ALL: [Class; 14] = [
        Class::Keyword,
        Class::String,
        Class::Comment,
        Class::Type,
        Class::Function,
        Class::Number,
        Class::Constant,
        Class::Operator,
        Class::Punctuation,
        Class::Property,
        Class::Tag,
        Class::Attribute,
        Class::Variable,
        Class::Plain,
    ];

    /// Position in `ALL` (for per-class color tables).
    pub fn index(self) -> usize {
        self as usize
    }
}

/// Capture names the highlighter recognizes. tree-sitter-highlight's `configure` matches a
/// capture to an entry when every part of the entry is among the capture's dot-separated parts
/// (in any position); the entry with the most parts wins and a tie goes to the first one. So
/// `function.method.call` lands on `function.method`, and `keyword.function` needs its own
/// entry: otherwise `function` (listed before `keyword`) would win the one-part tie. Whatever a
/// capture lands on must have the same `class_for` as the capture itself (tested). Captures that
/// match nothing produce no event and stay in the enclosing class: `none`, `spell` and
/// `embedded` are left out on purpose, since a markdown code block marks its content `@none`
/// around the injected language's own highlights.
pub(crate) const NAMES: &[&str] = &[
    "attribute",
    "boolean",
    "character",
    "comment",
    "conditional",
    "constant",
    "constant.builtin",
    "constructor",
    "delimiter",
    "escape",
    "exception",
    "field",
    "float",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "include",
    "keyword",
    "keyword.function",
    "label",
    "markup.heading",
    "markup.link",
    "markup.raw",
    "method",
    "module",
    "namespace",
    "number",
    "operator",
    "parameter",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "repeat",
    "storageclass",
    "string",
    "string.special",
    "tag",
    "text.literal",
    "text.reference",
    "text.title",
    "text.uri",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// The class of a capture name, by its leading parts. Covers the names the bundled queries
/// use, including the nvim-style ones (`conditional`, `repeat`, `include`, `namespace`, …).
pub fn class_for(capture: &str) -> Class {
    let mut parts = capture.split('.');
    let head = parts.next().unwrap_or("");
    let second = parts.next();
    match head {
        "keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass" => {
            Class::Keyword
        }
        "string" | "character" | "escape" => Class::String,
        "comment" => Class::Comment,
        "type" | "constructor" | "namespace" | "module" => Class::Type,
        "function" | "method" => Class::Function,
        "number" | "float" => Class::Number,
        "constant" | "boolean" => Class::Constant,
        "operator" => Class::Operator,
        "punctuation" | "delimiter" => Class::Punctuation,
        "property" | "field" | "label" => Class::Property,
        "tag" => Class::Tag,
        "attribute" => Class::Attribute,
        "variable" if second == Some("builtin") => Class::Keyword,
        "variable" | "parameter" => Class::Variable,
        "text" | "markup" => match second {
            Some("title" | "heading") => Class::Keyword,
            Some("literal" | "raw") => Class::String,
            Some("uri" | "link" | "reference") => Class::Tag,
            _ => Class::Plain,
        },
        _ => Class::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_names_map_by_prefix() {
        let cases = [
            ("keyword", Class::Keyword),
            ("keyword.function", Class::Keyword),
            ("conditional", Class::Keyword),
            ("repeat", Class::Keyword),
            ("include", Class::Keyword),
            ("exception", Class::Keyword),
            ("string", Class::String),
            ("string.special.url", Class::String),
            ("escape", Class::String),
            ("comment", Class::Comment),
            ("comment.documentation", Class::Comment),
            ("type", Class::Type),
            ("type.builtin", Class::Type),
            ("constructor", Class::Type),
            ("namespace", Class::Type),
            ("module", Class::Type),
            ("function", Class::Function),
            ("function.method.call", Class::Function),
            ("method", Class::Function),
            ("number", Class::Number),
            ("float", Class::Number),
            ("constant", Class::Constant),
            ("constant.builtin", Class::Constant),
            ("boolean", Class::Constant),
            ("operator", Class::Operator),
            ("punctuation.bracket", Class::Punctuation),
            ("delimiter", Class::Punctuation),
            ("property", Class::Property),
            ("field", Class::Property),
            ("tag", Class::Tag),
            ("attribute", Class::Attribute),
            ("variable", Class::Variable),
            ("variable.parameter", Class::Variable),
            ("parameter", Class::Variable),
            ("variable.builtin", Class::Keyword),
            ("text.title", Class::Keyword),
            ("markup.heading.1", Class::Keyword),
            ("text.literal", Class::String),
            ("text.uri", Class::Tag),
            ("text.emphasis", Class::Plain),
            ("spell", Class::Plain),
            ("none", Class::Plain),
            ("glimmer", Class::Plain),
            ("embedded", Class::Plain),
            ("", Class::Plain),
        ];
        for (capture, want) in cases {
            assert_eq!(class_for(capture), want, "{capture}");
        }
    }

    #[test]
    fn index_follows_all() {
        for (i, class) in Class::ALL.iter().enumerate() {
            assert_eq!(class.index(), i);
        }
    }
}
