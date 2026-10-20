//! Resolution of calls to functions by name: the call's own file, the files its imports point
//! at, the qualified name, then the bare name. Calls into a dependency or the standard library
//! stay outside the repo, and a name that fits more than a few places stays unresolved.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::MAX_CANDIDATES;
use crate::index::{Call, CallIndex, Function, FunctionId, Import};
use crate::packs::pack_for;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Target {
    /// Between one and `MAX_CANDIDATES` functions; more than one means the call is ambiguous.
    Local {
        ids: Vec<FunctionId>,
    },
    Outside {
        dependency: String,
        name: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// `(caller, target, line)` in the index's call order; unresolved calls have no edge.
    pub edges: Vec<(FunctionId, Target, u32)>,
    pub unresolved: usize,
}

/// Which functions a call can mean, as a test on a function that has the call's name.
enum Shape {
    /// An unqualified call: a free function.
    Free,
    /// A method of some other value.
    Method,
    /// This exact qualified name.
    Exact(String),
    /// `Q::name`: this qualified name, or a free function in a file named like `Q`.
    Path { exact: String, module: String },
    /// A call that cannot name a function (`this.x()` in a free function).
    Nothing,
}

enum Scope<'a> {
    File(&'a str),
    Files(Vec<&'a str>),
    All,
}

struct Lookup<'a> {
    index: &'a CallIndex,
    by_name: HashMap<&'a str, Vec<usize>>,
    by_file_name: HashMap<(&'a str, &'a str), Vec<usize>>,
    by_qualified: HashMap<&'a str, Vec<usize>>,
    by_id: HashMap<&'a FunctionId, usize>,
    imports: HashMap<&'a str, Vec<&'a Import>>,
    module_keys: HashMap<&'a str, Vec<String>>,
    files_by_key: HashMap<String, Vec<&'a str>>,
    dependencies: HashSet<&'a str>,
}

pub fn resolve(index: &CallIndex) -> Resolved {
    let lookup = Lookup::new(index);
    let mut resolved = Resolved::default();
    for call in &index.calls {
        let Some(&caller) = lookup.by_id.get(&call.from) else {
            resolved.unresolved += 1;
            continue;
        };
        match lookup.target(call, &index.functions[caller]) {
            Some(target) => resolved.edges.push((call.from.clone(), target, call.line)),
            None => resolved.unresolved += 1,
        }
    }
    resolved
}

const SELF_NAMES: [&str; 3] = ["self", "this", "cls"];

/// The first segment of a path or module string; a scoped npm package keeps its scope.
fn root(path: &str) -> &str {
    if path.starts_with('@') {
        let mut slashes = path.match_indices('/').map(|(i, _)| i);
        return match (slashes.next(), slashes.next()) {
            (Some(_), Some(end)) => &path[..end],
            _ => path,
        };
    }
    path.split([':', '.', '/']).next().unwrap_or(path)
}

fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split([':', '.', '/']).filter(|s| !s.is_empty())
}

fn last_segment(path: &str) -> &str {
    segments(path).last().unwrap_or(path)
}

/// A path-like module string (`./util.js`, as ESM writes the file it imports) loses a trailing
/// source extension, so it keys on the file's stem rather than on `js`.
fn without_extension(module: &str) -> &str {
    if !module.contains('/') && !module.starts_with('.') {
        return module;
    }
    let file = module.rsplit('/').next().unwrap_or(module);
    match file.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && matches!(
                    ext,
                    "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "py" | "pyi"
                ) =>
        {
            &module[..module.len() - ext.len() - 1]
        }
        _ => module,
    }
}

/// What a module string names once the relative prefixes are gone: its last segment.
fn module_key(module: &str) -> Option<&str> {
    segments(without_extension(module))
        .skip_while(|s| matches!(*s, "crate" | "self" | "super"))
        .last()
}

fn separator(function: &Function) -> &'static str {
    if function.lang == "rust" { "::" } else { "." }
}

impl<'a> Lookup<'a> {
    fn new(index: &'a CallIndex) -> Self {
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_file_name: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        let mut by_qualified: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_id = HashMap::new();
        let mut module_keys: HashMap<&str, Vec<String>> = HashMap::new();
        let mut files_by_key: HashMap<String, Vec<&str>> = HashMap::new();
        for (i, f) in index.functions.iter().enumerate() {
            by_name.entry(&f.name).or_default().push(i);
            by_file_name.entry((&f.path, &f.name)).or_default().push(i);
            by_qualified.entry(&f.qualified).or_default().push(i);
            by_id.insert(&f.id, i);
            if !module_keys.contains_key(f.path.as_str()) {
                let keys = file_keys(&f.path);
                for key in &keys {
                    files_by_key.entry(key.clone()).or_default().push(&f.path);
                }
                module_keys.insert(&f.path, keys);
            }
        }
        let mut imports: HashMap<&str, Vec<&Import>> = HashMap::new();
        for import in &index.imports {
            imports.entry(&import.path).or_default().push(import);
        }
        Self {
            index,
            by_name,
            by_file_name,
            by_qualified,
            by_id,
            imports,
            module_keys,
            files_by_key,
            dependencies: index.dependencies.iter().map(String::as_str).collect(),
        }
    }

    fn target(&self, call: &Call, caller: &Function) -> Option<Target> {
        if let Some(outside) = self.outside(call, caller) {
            return Some(outside);
        }
        let file = caller.path.as_str();
        let sep = separator(caller);
        let shape = shape_of(call, caller, sep);
        let exact = match (&call.qualifier, call.receiver) {
            (Some(q), _) if !is_self_like(call) => Some(Shape::Exact(format!(
                "{}{sep}{}",
                last_segment(q),
                call.name
            ))),
            _ => None,
        };
        let in_imports = self.import_scope(call, file);
        let steps = [
            Some((&shape, Scope::File(file))),
            in_imports.map(|files| (&shape, Scope::Files(files))),
            exact.as_ref().map(|s| (s, Scope::All)),
            Some((&shape, Scope::All)),
        ];
        for (shape, scope) in steps.into_iter().flatten() {
            if let Some(ids) = self.pick(call, shape, scope) {
                return Some(Target::Local { ids });
            }
        }
        if caller.lang == "python" && call.qualifier.is_none() && !call.receiver {
            let key = format!("{}.__init__", call.name);
            let all = self
                .by_qualified
                .get(key.as_str())
                .map_or(&[][..], Vec::as_slice);
            let same_file: Vec<usize> = all
                .iter()
                .copied()
                .filter(|&i| self.index.functions[i].path == file)
                .collect();
            for found in [same_file.as_slice(), all] {
                if (1..=MAX_CANDIDATES).contains(&found.len()) {
                    return Some(Target::Local {
                        ids: self.sorted_ids(found.iter().copied()),
                    });
                }
            }
        }
        None
    }

    fn sorted_ids(&self, found: impl Iterator<Item = usize>) -> Vec<FunctionId> {
        let mut ids: Vec<FunctionId> = found.map(|i| self.index.functions[i].id.clone()).collect();
        ids.sort();
        ids
    }

    /// The functions of `scope` that fit the call's shape, when that is 1 to `MAX_CANDIDATES`.
    fn pick(&self, call: &Call, shape: &Shape, scope: Scope<'_>) -> Option<Vec<FunctionId>> {
        let empty: &[usize] = &[];
        let buckets: Vec<&[usize]> = match scope {
            Scope::File(file) => {
                vec![
                    self.by_file_name
                        .get(&(file, call.name.as_str()))
                        .map_or(empty, Vec::as_slice),
                ]
            }
            Scope::Files(files) => files
                .into_iter()
                .map(|f| {
                    self.by_file_name
                        .get(&(f, call.name.as_str()))
                        .map_or(empty, Vec::as_slice)
                })
                .collect(),
            Scope::All => vec![
                self.by_name
                    .get(call.name.as_str())
                    .map_or(empty, Vec::as_slice),
            ],
        };
        let found: Vec<usize> = buckets
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| self.fits(shape, &self.index.functions[i]))
            .take(MAX_CANDIDATES + 1)
            .collect();
        (1..=MAX_CANDIDATES)
            .contains(&found.len())
            .then(|| self.sorted_ids(found.into_iter()))
    }

    fn fits(&self, shape: &Shape, f: &Function) -> bool {
        match shape {
            Shape::Free => f.qualified == f.name,
            Shape::Method => f.qualified != f.name,
            Shape::Exact(q) => f.qualified == *q,
            Shape::Path { exact, module } => {
                f.qualified == *exact
                    || (f.qualified == f.name
                        && self
                            .module_keys
                            .get(f.path.as_str())
                            .is_some_and(|k| k.contains(module)))
            }
            Shape::Nothing => false,
        }
    }

    /// The files an import of the call's file points at, when one of them can bind the call.
    fn import_scope(&self, call: &Call, file: &str) -> Option<Vec<&str>> {
        let first = call
            .qualifier
            .as_deref()
            .map_or(call.name.as_str(), |q| root_ident(q));
        let last = call.qualifier.as_deref().map(last_segment);
        let mut files: Vec<&str> = Vec::new();
        for import in self.imports.get(file).into_iter().flatten() {
            let binds = import
                .names
                .iter()
                .any(|n| n == first || n == "*" || last.is_some_and(|l| n == l));
            if !binds {
                continue;
            }
            let Some(key) = module_key(&import.module) else {
                continue;
            };
            files.extend(self.files_by_key.get(key).into_iter().flatten().copied());
        }
        files.sort_unstable();
        files.dedup();
        (!files.is_empty()).then_some(files)
    }

    /// A call through a dependency or the standard library: by its qualifier, or by an import
    /// that brings its first name in from one.
    fn outside(&self, call: &Call, caller: &Function) -> Option<Target> {
        let stdlib = pack_for(&caller.path).map_or(&[][..], |p| p.stdlib);
        let outside_root = |path: &str| -> Option<String> {
            if let Some(node) = path.strip_prefix("node:") {
                return Some(root(node).to_string());
            }
            let r = root(path);
            (self.dependencies.contains(r) || stdlib.contains(&r)).then(|| r.to_string())
        };
        let named = |dependency: String| Target::Outside {
            dependency,
            name: call.name.clone(),
        };
        if let Some(dependency) = call.qualifier.as_deref().and_then(outside_root) {
            return Some(named(dependency));
        }
        let first = call
            .qualifier
            .as_deref()
            .map_or(call.name.as_str(), |q| root_ident(q));
        self.imports
            .get(caller.path.as_str())
            .into_iter()
            .flatten()
            .filter(|i| i.names.iter().any(|n| n == first))
            .find_map(|i| outside_root(&i.module))
            .map(named)
    }
}

/// The first identifier of a qualifier: `crate` in `crate::util`, `os` in `os.path`.
fn root_ident(qualifier: &str) -> &str {
    segments(qualifier).next().unwrap_or(qualifier)
}

fn is_self_like(call: &Call) -> bool {
    match call.qualifier.as_deref() {
        Some(q) if call.receiver => SELF_NAMES.contains(&q),
        Some(q) => q == "Self",
        None => false,
    }
}

fn shape_of(call: &Call, caller: &Function, sep: &str) -> Shape {
    if is_self_like(call) {
        return match caller.qualified.rsplit_once(sep) {
            Some((owner, _)) => Shape::Exact(format!("{owner}{sep}{}", call.name)),
            None => Shape::Nothing,
        };
    }
    match (&call.qualifier, call.receiver) {
        (_, true) => Shape::Method,
        (Some(q), false) => {
            let last = last_segment(q);
            Shape::Path {
                exact: format!("{last}{sep}{}", call.name),
                module: last.to_string(),
            }
        }
        (None, false) => Shape::Free,
    }
}

/// What a module string can be to name this file: its stem, and for an entry file such as
/// `mod.rs` or `__init__.py` the folder it stands for.
fn file_keys(path: &str) -> Vec<String> {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let mut keys = vec![stem.to_string()];
    if matches!(stem, "mod" | "index" | "__init__" | "lib" | "main")
        && let Some(parent) = parts.next()
    {
        keys.push(parent.to_string());
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{cpu_timed, generated_repo, index_of, index_with};
    use serde_json::json;

    fn id(v: &str) -> FunctionId {
        FunctionId(v.to_string())
    }

    fn local(ids: &[&str]) -> Target {
        Target::Local {
            ids: ids.iter().map(|i| id(i)).collect(),
        }
    }

    fn outside(dependency: &str, name: &str) -> Target {
        Target::Outside {
            dependency: dependency.to_string(),
            name: name.to_string(),
        }
    }

    const STORE: &str = r#"use crate::util::norm;
use tokio::spawn;

pub struct Store;

impl Store {
    pub fn token(&self) {
        self.refresh();
        norm(1);
        helper();
        spawn(1);
        std::fs::read("x");
        Store::new();
        other();
        missing();
    }

    pub fn refresh(&self) {}

    pub fn new() -> Store {
        Store
    }
}

fn helper() {}
"#;

    #[test]
    fn calls_resolve_in_order_same_file_imports_qualified_bare_and_outside() {
        let index = index_with(
            &[
                ("src/store.rs", STORE),
                (
                    "src/util.rs",
                    "pub fn norm(x: u32) -> u32 {\n    x\n}\n\npub fn fmt() {}\n",
                ),
                ("src/legacy.rs", "pub fn norm() {}\n"),
                ("src/other.rs", "pub fn other() {}\npub fn helper() {}\n"),
                ("src/more.rs", "pub fn other() {}\n"),
            ],
            &["tokio"],
        );
        let resolved = resolve(&index);
        let token = id("src/store.rs#Store::token#7");
        assert_eq!(
            resolved.edges,
            vec![
                (token.clone(), local(&["src/store.rs#Store::refresh#18"]), 8),
                (token.clone(), local(&["src/util.rs#norm#1"]), 9),
                (token.clone(), local(&["src/store.rs#helper#25"]), 10),
                (token.clone(), outside("tokio", "spawn"), 11),
                (token.clone(), outside("std", "read"), 12),
                (token.clone(), local(&["src/store.rs#Store::new#20"]), 13),
                (
                    token,
                    local(&["src/more.rs#other#1", "src/other.rs#other#1"]),
                    14
                ),
            ]
        );
        assert_eq!(resolved.unresolved, 1);
    }

    #[test]
    fn a_qualified_call_beats_the_bare_name_and_an_unqualified_call_never_means_a_method() {
        let index = index_of(&[
            (
                "src/cache.rs",
                "pub struct Cache;\nimpl Cache {\n    pub fn get() {}\n}\n",
            ),
            (
                "src/pool.rs",
                "pub struct Pool;\nimpl Pool {\n    pub fn get() {}\n}\n",
            ),
            (
                "src/main.rs",
                "fn run(p: u8) {\n    Cache::get();\n    get();\n    p.get();\n}\n",
            ),
        ]);
        let resolved = resolve(&index);
        let run = id("src/main.rs#run#1");
        assert_eq!(
            resolved.edges,
            vec![
                (run.clone(), local(&["src/cache.rs#Cache::get#3"]), 2),
                (
                    run,
                    local(&["src/cache.rs#Cache::get#3", "src/pool.rs#Pool::get#3"]),
                    4
                ),
            ]
        );
        assert_eq!(resolved.unresolved, 1);
    }

    #[test]
    fn a_name_in_more_than_three_places_stays_unresolved_and_in_three_it_is_ambiguous() {
        let call = ("src/main.rs", "fn run() {\n    get();\n}\n");
        let mut four: Vec<(String, &str)> = (0..4)
            .map(|i| (format!("src/a{i}.rs"), "pub fn get() {}\n"))
            .collect();
        let files: Vec<(&str, &str)> = four
            .iter()
            .map(|(p, t)| (p.as_str(), *t))
            .chain([call])
            .collect();
        let resolved = resolve(&index_of(&files));
        assert!(resolved.edges.is_empty());
        assert_eq!(resolved.unresolved, 1);

        four.truncate(3);
        let files: Vec<(&str, &str)> = four
            .iter()
            .map(|(p, t)| (p.as_str(), *t))
            .chain([call])
            .collect();
        let resolved = resolve(&index_of(&files));
        assert_eq!(
            resolved.edges,
            vec![(
                id("src/main.rs#run#1"),
                local(&["src/a0.rs#get#1", "src/a1.rs#get#1", "src/a2.rs#get#1"]),
                2
            )]
        );
    }

    #[test]
    fn a_name_brought_in_from_the_standard_library_is_outside() {
        let src = "use std::collections::HashMap;\nfn f() {\n    HashMap::new();\n}\n";
        let resolved = resolve(&index_of(&[("a.rs", src)]));
        assert_eq!(
            resolved.edges,
            vec![(id("a.rs#f#2"), outside("std", "new"), 3)]
        );
    }

    #[test]
    fn a_recursive_call_resolves_to_its_own_function() {
        let src = "fn f(n: u32) -> u32 {\n    if n == 0 { 0 } else { f(n - 1) }\n}\n";
        let resolved = resolve(&index_of(&[("r.rs", src)]));
        assert_eq!(
            resolved.edges,
            vec![(id("r.rs#f#1"), local(&["r.rs#f#1"]), 2)]
        );
    }

    #[test]
    fn typescript_imports_dependencies_and_unknown_receivers() {
        let a = "import { norm } from \"./util\";\nimport axios from \"axios\";\nexport function run() {\n  norm(1);\n  axios.get(\"u\");\n  this.x();\n}\n";
        let index = index_with(
            &[
                ("src/a.ts", a),
                (
                    "src/util.ts",
                    "export function norm(x: number) { return x; }\n",
                ),
            ],
            &["axios"],
        );
        let resolved = resolve(&index);
        let run = id("src/a.ts#run#3");
        assert_eq!(
            resolved.edges,
            vec![
                (run.clone(), local(&["src/util.ts#norm#1"]), 4),
                (run, outside("axios", "get"), 5)
            ]
        );
        assert_eq!(resolved.unresolved, 1);
    }

    #[test]
    fn an_esm_import_with_an_extension_finds_the_file() {
        let a = "import { load } from \"./util.js\";\nexport function run() {\n  load();\n}\n";
        let index = index_of(&[
            ("src/a.ts", a),
            ("src/util.ts", "export function load() {}\n"),
            ("src/js.ts", "export function load() {}\n"),
        ]);
        assert_eq!(
            resolve(&index).edges,
            vec![(id("src/a.ts#run#2"), local(&["src/util.ts#load#1"]), 3)]
        );
    }

    #[test]
    fn python_imports_stdlib_dependencies_and_constructors() {
        let a = "from .util import norm\nimport requests\nimport os\n\nclass Thing:\n    def __init__(self):\n        pass\n\n\ndef run():\n    norm(1)\n    requests.get(\"u\")\n    os.getcwd()\n    Thing()\n";
        let index = index_with(
            &[
                ("pkg/a.py", a),
                ("pkg/util.py", "def norm(x):\n    return x\n"),
            ],
            &["requests"],
        );
        let resolved = resolve(&index);
        let run = id("pkg/a.py#run#10");
        assert_eq!(
            resolved.edges,
            vec![
                (run.clone(), local(&["pkg/util.py#norm#1"]), 11),
                (run.clone(), outside("requests", "get"), 12),
                (run.clone(), outside("os", "getcwd"), 13),
                (run, local(&["pkg/a.py#Thing.__init__#6"]), 14),
            ]
        );
        assert_eq!(resolved.unresolved, 0);
    }

    #[test]
    fn targets_keep_the_wire_shape() {
        assert_eq!(
            serde_json::to_value(outside("tokio", "spawn")).expect("json"),
            json!({ "kind": "outside", "dependency": "tokio", "name": "spawn" })
        );
        assert_eq!(
            serde_json::to_value(local(&["a#f#1"])).expect("json"),
            json!({ "kind": "local", "ids": ["a#f#1"] })
        );
    }

    #[test]
    fn five_thousand_functions_index_and_resolve_inside_the_budget_and_deterministically() {
        let repo = generated_repo(500, 10);
        let files: Vec<(&str, &str)> = repo.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        let ((index, resolved), took) = cpu_timed(|| {
            let index = index_of(&files);
            let resolved = resolve(&index);
            (index, resolved)
        });
        // ~0.3 s of CPU in a dev build with optimised dependencies.
        assert!(took.as_secs() < 5, "{took:?}");
        assert_eq!(index.functions.len(), 5_000);
        assert_eq!(resolved.edges.len(), 10_000);
        assert_eq!(resolved.unresolved, 0);
        assert_eq!(resolve(&index), resolved);
    }
}
