//! Functions, calls and imports from one parsed file, read through the pack's queries.

use std::collections::{BTreeMap, HashMap};

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator, Tree};

use crate::index::{Call, Function, FunctionId, Import};
use crate::packs::{Grammar, compiled};

pub(crate) struct Extracted {
    pub functions: Vec<Function>,
    pub calls: Vec<Call>,
    pub imports: Vec<Import>,
}

fn text_of<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    text.get(node.start_byte()..node.end_byte()).unwrap_or("")
}

fn row(node: Node<'_>) -> u32 {
    node.start_position().row as u32 + 1
}

pub(crate) fn extract(
    path: &str,
    lang: &str,
    grammar: Grammar,
    tree: &Tree,
    text: &str,
) -> Extracted {
    let compiled = compiled(grammar);
    let root = tree.root_node();
    let defs = definitions(path, lang, grammar, &compiled.definitions, root, text);
    let calls = calls(&defs, &compiled.calls, root, text);
    let imports = imports(path, grammar, &compiled.imports, root, text);
    Extracted {
        functions: defs.into_iter().map(|d| d.function).collect(),
        calls,
        imports,
    }
}

struct Def {
    function: Function,
    start_byte: usize,
    end_byte: usize,
}

/// A raw definition match: several patterns reach one function node, and only the one with a
/// qualifier knows its owner.
struct Found<'t> {
    node: Node<'t>,
    name: Node<'t>,
    qualifier: Option<Node<'t>>,
}

fn definitions(
    path: &str,
    lang: &str,
    grammar: Grammar,
    query: &Query,
    root: Node<'_>,
    text: &str,
) -> Vec<Def> {
    let names = query.capture_names();
    let mut by_range: HashMap<(usize, usize), Found<'_>> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, text.as_bytes());
    while let Some(m) = matches.next() {
        let (mut node, mut name, mut qualifier) = (None, None, None);
        for capture in m.captures() {
            match names[capture.index as usize] {
                "def" => node = Some(capture.node),
                "name" => name = Some(capture.node),
                "qualifier" => qualifier = Some(capture.node),
                _ => {}
            }
        }
        let (Some(node), Some(name)) = (node, name) else {
            continue;
        };
        let key = (node.start_byte(), node.end_byte());
        match by_range.get(&key) {
            Some(known) if known.qualifier.is_some() || qualifier.is_none() => {}
            _ => {
                by_range.insert(
                    key,
                    Found {
                        node,
                        name,
                        qualifier,
                    },
                );
            }
        }
    }
    let mut found: Vec<Found<'_>> = by_range.into_values().collect();
    found.sort_by_key(|f| (f.node.start_byte(), f.name.start_byte()));

    let rust = grammar == Grammar::Rust;
    let mut seen: HashMap<String, u32> = HashMap::new();
    found
        .into_iter()
        .map(|f| {
            let name = text_of(f.name, text).to_string();
            let qualified = match f.qualifier {
                Some(q) if rust => format!("{}::{name}", rust_type_name(text_of(q, text))),
                Some(q) => format!("{}.{name}", text_of(q, text)),
                None => name.clone(),
            };
            let start_line = row(f.node);
            let end_line = f.node.end_position().row as u32 + 1;
            let signature_lines = match f.node.child_by_field_name("body") {
                None => 1,
                Some(body) => {
                    let gap = row(body).saturating_sub(start_line);
                    if grammar == Grammar::Python {
                        gap.max(1)
                    } else {
                        gap + 1
                    }
                }
            };
            let base = format!("{path}#{qualified}#{start_line}");
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            let id = if *count == 1 {
                base
            } else {
                format!("{base}+{count}")
            };
            Def {
                start_byte: f.node.start_byte(),
                end_byte: f.node.end_byte(),
                function: Function {
                    id: FunctionId(id),
                    qualified,
                    name,
                    path: path.to_string(),
                    start_line,
                    end_line,
                    signature_lines,
                    lang: lang.to_string(),
                },
            }
        })
        .collect()
}

/// The type an `impl` is for, as a bare name: generics, `&`, `mut` and `dyn` removed and only
/// the last path segment kept.
fn rust_type_name(ty: &str) -> String {
    let mut bare = String::new();
    let mut depth = 0usize;
    for c in ty.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => bare.push(c),
            _ => {}
        }
    }
    let mut rest = bare.trim();
    loop {
        let before = rest;
        rest = rest.trim_start_matches('&').trim_start();
        if rest.starts_with('\'') {
            rest = rest
                .split_once(char::is_whitespace)
                .map_or("", |(_, r)| r)
                .trim_start();
        }
        for word in ["mut ", "dyn "] {
            rest = rest.strip_prefix(word).unwrap_or(rest).trim_start();
        }
        if rest == before {
            break;
        }
    }
    rest.rsplit("::").next().unwrap_or(rest).trim().to_string()
}

fn is_dotted_path(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
        })
}

fn calls(defs: &[Def], query: &Query, root: Node<'_>, text: &str) -> Vec<Call> {
    let names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, text.as_bytes());
    while let Some(m) = matches.next() {
        let (mut call, mut name, mut qualifier, mut receiver, mut constructor) =
            (None, None, None, None, None);
        for capture in m.captures() {
            match names[capture.index as usize] {
                "call" => call = Some(capture.node),
                "name" => name = Some(capture.node),
                "qualifier" => qualifier = Some(capture.node),
                "receiver" => receiver = Some(capture.node),
                "constructor" => constructor = Some(capture.node),
                _ => {}
            }
        }
        let Some(call) = call else { continue };
        let Some(owner) = owner_of(defs, call.start_byte()) else {
            continue;
        };
        let (name, qualifier, receiver) = if let Some(c) = constructor {
            (
                "constructor".to_string(),
                Some(text_of(c, text).to_string()),
                false,
            )
        } else if let Some(n) = name {
            let name = text_of(n, text).to_string();
            match (qualifier, receiver) {
                (Some(q), _) => (name, Some(text_of(q, text).to_string()), false),
                (None, Some(r)) => {
                    let r = text_of(r, text);
                    (name, is_dotted_path(r).then(|| r.to_string()), true)
                }
                (None, None) => (name, None, false),
            }
        } else {
            continue;
        };
        out.push(Call {
            from: owner.function.id.clone(),
            name,
            qualifier,
            receiver,
            line: row(call),
        });
    }
    out
}

/// The innermost function holding a byte offset: of those whose range contains it, the one that
/// starts last. `defs` is sorted by start byte.
fn owner_of(defs: &[Def], byte: usize) -> Option<&Def> {
    let upto = defs.partition_point(|d| d.start_byte <= byte);
    defs[..upto].iter().rev().find(|d| d.end_byte > byte)
}

fn imports(path: &str, grammar: Grammar, query: &Query, root: Node<'_>, text: &str) -> Vec<Import> {
    let names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, text.as_bytes());
    while let Some(m) = matches.next() {
        let (mut import, mut capture_path, mut source) = (None, None, None);
        for capture in m.captures() {
            match names[capture.index as usize] {
                "import" => import = Some(capture.node),
                "path" => capture_path = Some(capture.node),
                "source" => source = Some(capture.node),
                _ => {}
            }
        }
        let Some(import) = import else { continue };
        let rows: Vec<(String, Vec<String>)> = match grammar {
            Grammar::Rust => capture_path
                .map(|p| expand_use(text_of(p, text)))
                .unwrap_or_default(),
            Grammar::Python => python_imports(import, text),
            Grammar::TypeScript | Grammar::Tsx | Grammar::JavaScript => source
                .map(|s| vec![(text_of(s, text).to_string(), script_names(import, text))])
                .unwrap_or_default(),
        };
        out.extend(rows.into_iter().map(|(module, names)| Import {
            path: path.to_string(),
            module,
            names,
        }));
    }
    out
}

fn script_names(import: Node<'_>, text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut walk = import.walk();
    for clause in import
        .children(&mut walk)
        .filter(|c| c.kind() == "import_clause")
    {
        let mut inner = clause.walk();
        for part in clause.children(&mut inner) {
            match part.kind() {
                "identifier" => names.push("default".to_string()),
                "namespace_import" => names.push("*".to_string()),
                "named_imports" => {
                    let mut specs = part.walk();
                    for spec in part
                        .children(&mut specs)
                        .filter(|s| s.kind() == "import_specifier")
                    {
                        if let Some(name) = spec.child_by_field_name("name") {
                            names.push(text_of(name, text).trim_matches(['"', '\'']).to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    names
}

fn python_imports(import: Node<'_>, text: &str) -> Vec<(String, Vec<String>)> {
    let original = |node: Node<'_>| {
        let name = if node.kind() == "aliased_import" {
            node.child_by_field_name("name").unwrap_or(node)
        } else {
            node
        };
        text_of(name, text).to_string()
    };
    if import.kind() == "import_statement" {
        let mut walk = import.walk();
        return import
            .children_by_field_name("name", &mut walk)
            .map(|n| (original(n), Vec::new()))
            .collect();
    }
    let Some(module) = import.child_by_field_name("module_name") else {
        return Vec::new();
    };
    let mut walk = import.walk();
    let mut names: Vec<String> = import
        .children_by_field_name("name", &mut walk)
        .map(original)
        .collect();
    let mut walk = import.walk();
    if import
        .children(&mut walk)
        .any(|c| c.kind() == "wildcard_import")
    {
        names.push("*".to_string());
    }
    vec![(text_of(module, text).to_string(), names)]
}

/// Splits the argument of a Rust `use` into `(module, names)` rows, one per module prefix:
/// `a::{b::{c, d}, e}` is `a` with `e` and `a::b` with `c`, `d`. A `self` entry only keeps its
/// module; an alias is dropped for the original name.
fn expand_use(argument: &str) -> Vec<(String, Vec<String>)> {
    let chars: Vec<char> = argument
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let mut parser = UseParser {
        chars,
        pos: 0,
        rows: BTreeMap::new(),
        order: Vec::new(),
    };
    parser.tree("");
    let UseParser {
        mut rows, order, ..
    } = parser;
    order
        .into_iter()
        .filter_map(|m| rows.remove(&m).map(|n| (m, n)))
        .collect()
}

struct UseParser {
    chars: Vec<char>,
    pos: usize,
    rows: BTreeMap<String, Vec<String>>,
    order: Vec<String>,
}

impl UseParser {
    fn peek(&mut self) -> Option<char> {
        while self.chars.get(self.pos) == Some(&' ') {
            self.pos += 1;
        }
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, token: &str) -> bool {
        self.peek();
        let end = self.pos + token.chars().count();
        let hit = self
            .chars
            .get(self.pos..end)
            .is_some_and(|s| s.iter().copied().eq(token.chars()));
        if hit {
            self.pos = end;
        }
        hit
    }

    fn ident(&mut self) -> String {
        self.peek();
        let start = self.pos;
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_')
        {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn add(&mut self, module: &str, name: Option<&str>) {
        if !self.rows.contains_key(module) {
            self.order.push(module.to_string());
        }
        let names = self.rows.entry(module.to_string()).or_default();
        if let Some(name) = name {
            names.push(name.to_string());
        }
    }

    fn list(&mut self, prefix: &str) {
        while self.peek().is_some_and(|c| c != '}') {
            self.tree(prefix);
            if !self.eat(",") {
                break;
            }
        }
        self.eat("}");
    }

    fn tree(&mut self, prefix: &str) {
        self.eat("::");
        if self.eat("{") {
            return self.list(prefix);
        }
        if self.eat("*") {
            return self.add(prefix, Some("*"));
        }
        let mut segments: Vec<String> = Vec::new();
        loop {
            let segment = self.ident();
            if segment.is_empty() {
                return;
            }
            segments.push(segment);
            if !self.eat("::") {
                break;
            }
            let module = join(prefix, &segments);
            if self.eat("{") {
                return self.list(&module);
            }
            if self.eat("*") {
                return self.add(&module, Some("*"));
            }
        }
        if self.eat("as") {
            self.ident();
        }
        let last = segments.pop().unwrap_or_default();
        let module = join(prefix, &segments);
        match (last.as_str(), module.is_empty()) {
            ("self", _) => self.add(prefix, None),
            (_, true) => self.add(&last, None),
            _ => self.add(&module, Some(&last)),
        }
    }
}

fn join(prefix: &str, segments: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !prefix.is_empty() {
        parts.push(prefix);
    }
    parts.extend(segments.iter().map(String::as_str));
    parts.join("::")
}
