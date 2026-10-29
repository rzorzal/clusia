//! The generated image: its base, its exec-form `RUN` lines and its hash.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use clusia_core::printable::printable;

use crate::names::{LabelKind, Labels, base_ref, image_ref, sha256_hex};
use crate::recipe::{ImageSource, Recipe, StepInto};
use crate::scan::open_regular;

/// The cap on a Dockerfile the build reads.
const FILE_MAX: u64 = 1024 * 1024;
/// The cap on a devcontainer file.
const DEVCONTAINER_MAX: u64 = 256 * 1024;

/// The devcontainer keys Clúsia ignores, in the order their notes are given.
const UNSUPPORTED: [&str; 11] = [
    "features",
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
    "initializeCommand",
    "mounts",
    "runArgs",
    "containerEnv",
    "remoteEnv",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devcontainer {
    pub image: Option<String>,
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    pub target: Option<String>,
    pub args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlan {
    /// The `docker build` arguments of a repo or devcontainer base, built first.
    pub base_build: Option<Vec<String>>,
    pub dockerfile: String,
    pub tag: String,
    pub hash: String,
    pub notes: Vec<String>,
    /// Whether an image step needs the network; without one the build's `RUN`s get none.
    pub network: bool,
}

/// What the generated Dockerfile starts `FROM`.
struct Base {
    name: String,
    build: Option<Vec<String>>,
    hash: String,
    notes: Vec<String>,
}

pub fn plan_image(
    recipe: &Recipe,
    worktree: &Path,
    slug: &str,
    labels: &Labels,
    inputs_hash: &str,
) -> Result<ImagePlan, String> {
    let Some(source) = &recipe.image else {
        return Err(crate::plan::NO_IMAGE.to_string());
    };
    let wt = worktree
        .canonicalize()
        .map_err(|e| format!("Clúsia can't read the worktree {}: {e}", worktree.display()))?;
    let base = match source {
        ImageSource::Base { image } => pulled_base("base", "", image)?,
        ImageSource::Dockerfile { path, target } => {
            let (file, text) = read_inside(&wt, &wt, path, "Dockerfile", FILE_MAX)?;
            let build = BaseBuild {
                source: "dockerfile",
                text: &text,
                dockerfile_text: None,
                file: &file,
                context: &wt,
                target: target.as_deref(),
                args: &BTreeMap::new(),
            };
            built_base(&wt, slug, labels, inputs_hash, build)?
        }
        ImageSource::Devcontainer => devcontainer_base(&wt, slug, labels, inputs_hash)?,
    };

    let steps: Vec<&str> = recipe
        .setup
        .iter()
        .filter(|s| s.into == StepInto::Image)
        .map(|s| s.run.as_str())
        .collect();
    let mut dockerfile = format!("FROM {}\n", base.name);
    let mut fields = Vec::new();
    field(&mut fields, "base", base.hash.as_bytes());
    for run in &steps {
        // The exec form with a JSON string: a line break in `run` stays inside the string, so
        // a step can never add a Dockerfile instruction.
        let quoted = serde_json::to_string(run).expect("a string serializes");
        dockerfile.push_str(&format!("RUN [\"/bin/sh\",\"-c\",{quoted}]\n"));
        field(&mut fields, "step", run.as_bytes());
    }
    let hash = sha256_hex(&fields);
    Ok(ImagePlan {
        base_build: base.build,
        dockerfile,
        tag: image_ref(slug, &hash),
        hash,
        notes: base.notes,
        network: recipe
            .setup
            .iter()
            .any(|s| s.into == StepInto::Image && s.network),
    })
}

/// A base pulled by name: no repo file and no inputs in its hash.
fn pulled_base(source: &str, text: &str, image: &str) -> Result<Base, String> {
    if !valid_image_name(image) {
        return Err(format!("{} is not a valid image name", printable(image)));
    }
    let mut fields = Vec::new();
    field(&mut fields, "source", source.as_bytes());
    field(&mut fields, "text", text.as_bytes());
    field(&mut fields, "image", image.as_bytes());
    field(&mut fields, "target", b"");
    field(&mut fields, "context", b"");
    Ok(Base {
        name: image.to_string(),
        build: None,
        hash: sha256_hex(&fields),
        notes: vec![],
    })
}

struct BaseBuild<'a> {
    source: &'a str,
    /// The Dockerfile, or the devcontainer file for a devcontainer build.
    text: &'a str,
    /// A devcontainer build's Dockerfile, which `text` does not hold.
    dockerfile_text: Option<&'a str>,
    file: &'a Path,
    context: &'a Path,
    target: Option<&'a str>,
    args: &'a BTreeMap<String, String>,
}

/// A base built from a repo Dockerfile. Its hash holds the review's inputs, so a later pull
/// request never reuses a base built from an earlier one's copied files.
fn built_base(
    wt: &Path,
    slug: &str,
    labels: &Labels,
    inputs_hash: &str,
    build: BaseBuild<'_>,
) -> Result<Base, String> {
    for (key, value) in build.args {
        if !valid_arg_key(key) {
            return Err(format!("{} is not a valid build arg name", printable(key)));
        }
        if value.contains('\0') {
            return Err(format!("build arg {key} holds a NUL character"));
        }
    }
    let context = match build.context.strip_prefix(wt) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => build.context.display().to_string(),
    };
    let mut fields = Vec::new();
    field(&mut fields, "source", build.source.as_bytes());
    field(&mut fields, "text", build.text.as_bytes());
    if let Some(text) = build.dockerfile_text {
        field(&mut fields, "dockerfile", text.as_bytes());
    }
    field(&mut fields, "image", b"");
    field(&mut fields, "target", build.target.unwrap_or("").as_bytes());
    for (key, value) in build.args {
        field(&mut fields, "arg", format!("{key}={value}").as_bytes());
    }
    field(&mut fields, "context", context.as_bytes());
    field(&mut fields, "inputs", inputs_hash.as_bytes());
    let hash = sha256_hex(&fields);
    let name = base_ref(slug, &hash);

    let mut args = vec![
        "build".to_string(),
        "-f".into(),
        build.file.display().to_string(),
    ];
    if let Some(target) = build.target {
        args.extend(["--target".to_string(), target.to_string()]);
    }
    for (key, value) in build.args {
        args.extend(["--build-arg".to_string(), format!("{key}={value}")]);
    }
    let image_labels = Labels {
        kind: LabelKind::Image,
        ..labels.clone()
    };
    args.extend(image_labels.args());
    args.extend(["-t".to_string(), name.clone()]);
    args.push(build.context.display().to_string());
    Ok(Base {
        name,
        build: Some(args),
        hash,
        notes: vec![],
    })
}

fn devcontainer_base(
    wt: &Path,
    slug: &str,
    labels: &Labels,
    inputs_hash: &str,
) -> Result<Base, String> {
    let rel = [".devcontainer/devcontainer.json", ".devcontainer.json"]
        .into_iter()
        .find(|rel| std::fs::symlink_metadata(wt.join(rel)).is_ok())
        .ok_or("The worktree has no .devcontainer/devcontainer.json or .devcontainer.json")?;
    let (file, text) = read_inside(wt, wt, rel, "devcontainer file", DEVCONTAINER_MAX)?;
    let (dc, notes) = reduce_devcontainer(&text)?;
    // Devcontainer paths are relative to the folder that holds the file.
    let dir = file.parent().unwrap_or(wt).to_path_buf();
    let mut base = if let Some(dockerfile) = &dc.dockerfile {
        let context_rel = dc.context.as_deref().unwrap_or(".");
        let context = resolve_inside(wt, &dir, context_rel, "devcontainer build context")?;
        if !context.is_dir() {
            return Err(format!(
                "The devcontainer build context {} is not a folder",
                printable(context_rel)
            ));
        }
        let (file, dockerfile_text) =
            read_inside(wt, &dir, dockerfile, "devcontainer Dockerfile", FILE_MAX)?;
        let build = BaseBuild {
            source: "devcontainer-build",
            text: &text,
            dockerfile_text: Some(&dockerfile_text),
            file: &file,
            context: &context,
            target: dc.target.as_deref(),
            args: &dc.args,
        };
        built_base(wt, slug, labels, inputs_hash, build)?
    } else {
        let image = dc.image.as_deref().unwrap_or_default();
        pulled_base("devcontainer-image", &text, image)?
    };
    base.notes = notes;
    Ok(base)
}

/// `base/rel`, resolved through every symlink and `..`, which must stay inside `wt`.
fn resolve_inside(wt: &Path, base: &Path, rel: &str, what: &str) -> Result<PathBuf, String> {
    let outside = || {
        format!(
            "The {what} {} resolves outside the worktree",
            printable(rel)
        )
    };
    let path = base.join(rel).canonicalize().map_err(|_| outside())?;
    if path.starts_with(wt) {
        Ok(path)
    } else {
        Err(outside())
    }
}

/// A file inside `wt`, read up to `max` bytes.
fn read_inside(
    wt: &Path,
    base: &Path,
    rel: &str,
    what: &str,
    max: u64,
) -> Result<(PathBuf, String), String> {
    let path = resolve_inside(wt, base, rel, what)?;
    let shown = printable(rel);
    let file = open_regular(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidInput => format!("The {what} {shown} is not a file"),
        _ => format!("The {what} {shown} can't be read: {e}"),
    })?;
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("The {what} {shown} can't be read: {e}"))?;
    if bytes.len() as u64 > max {
        return Err(format!("The {what} {shown} is over {} KiB", max / 1024));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("The {what} {shown} is not UTF-8"))?;
    Ok((path, text))
}

/// Appends one hash field as `<name>\n<byte length>\n<bytes>\n`, so no two field lists share
/// a text.
fn field(out: &mut Vec<u8>, name: &str, value: &[u8]) {
    out.extend_from_slice(format!("{name}\n{}\n", value.len()).as_bytes());
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn valid_arg_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reads what Clúsia can build from a devcontainer file; the notes name what it ignored.
pub fn reduce_devcontainer(jsonc: &str) -> Result<(Devcontainer, Vec<String>), String> {
    let value: serde_json::Value = serde_json::from_str(&strip_jsonc(jsonc))
        .map_err(|e| format!("devcontainer.json is not valid JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Err("devcontainer.json is not valid JSON: it is not an object".to_string());
    };
    if obj.contains_key("dockerComposeFile") {
        return Err("Compose-based devcontainers are not supported yet".to_string());
    }
    let mut dc = Devcontainer {
        image: string_at(obj, "image", "image")?,
        ..Devcontainer::default()
    };
    let mut notes = Vec::new();
    if let Some(build) = obj.get("build") {
        let build = build
            .as_object()
            .ok_or("devcontainer.json `build` must be an object")?;
        dc.dockerfile = string_at(build, "dockerfile", "build.dockerfile")?;
        dc.target = string_at(build, "target", "build.target")?;
        dc.context = Some(string_at(build, "context", "build.context")?.unwrap_or(".".into()));
        if let Some(args) = build.get("args") {
            let args = args.as_object().ok_or("build.args must be an object")?;
            for (key, value) in args {
                let value = value
                    .as_str()
                    .ok_or_else(|| format!("build.args.{} must be a string", printable(key)))?;
                dc.args.insert(key.clone(), value.to_string());
            }
        }
        if dc.image.take().is_some() {
            notes
                .push("devcontainer.json sets both image and build; Clúsia used build".to_string());
        }
    }
    if dc.image.is_none() && dc.dockerfile.is_none() {
        return Err("devcontainer.json names no image and no build.dockerfile".to_string());
    }
    for key in UNSUPPORTED {
        if obj.contains_key(key) {
            notes.push(format!(
                "devcontainer.json `{key}` is not supported yet; Clúsia ignored it"
            ));
        }
    }
    Ok((dc, notes))
}

fn string_at(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    shown: &str,
) -> Result<Option<String>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{shown} must be a string")),
    }
}

/// JSON with comments to JSON: each `//` or `/* */` comment outside a string becomes one
/// space, then each `,` followed only by whitespace up to `}` or `]` is dropped.
fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            plain.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    plain.push(next);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
        } else if c == '"' {
            in_string = true;
            plain.push(c);
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            plain.push(' ');
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
            plain.push(' ');
        } else {
            plain.push(c);
            i += 1;
        }
    }

    let chars: Vec<char> = plain.chars().collect();
    let mut out = String::with_capacity(plain.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == ','
            && matches!(
                chars[i + 1..].iter().find(|c| !c.is_whitespace()),
                Some('}' | ']')
            )
        {
            continue;
        }
        out.push(c);
    }
    out
}

/// 1–255 bytes of `[A-Za-z0-9._/:@-]`, not starting with `-`, so a name is one argument and
/// one `FROM` word.
pub fn valid_image_name(name: &str) -> bool {
    (1..=255).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/:@-".contains(&b))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use clusia_core::tests::{ResultsFormat, RunnerChoice};

    use crate::names::LabelKind;
    use crate::recipe::{ImageSource, SetupStep, StepInto, TestsSpec};

    use super::*;

    const SLUG: &str = "acme-widgets-abc123";
    const INPUTS: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn labels() -> Labels {
        Labels {
            review: "acme~widgets~7".into(),
            repo: "acme/widgets".into(),
            kind: LabelKind::Image,
        }
    }

    fn step(run: &str, image: bool) -> SetupStep {
        SetupStep {
            name: run.into(),
            run: run.into(),
            into: if image {
                StepInto::Image
            } else {
                StepInto::Work
            },
            network: true,
        }
    }

    fn recipe(image: ImageSource, setup: Vec<SetupStep>) -> Recipe {
        Recipe {
            runner: RunnerChoice::Auto,
            image: Some(image),
            setup,
            services: None,
            tests: TestsSpec {
                all: "cargo test".into(),
                one: None,
                some: None,
                list: None,
                results: ResultsFormat::Libtest,
                env: BTreeMap::new(),
            },
        }
    }

    fn base(name: &str) -> ImageSource {
        ImageSource::Base { image: name.into() }
    }

    fn worktree() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().canonicalize().unwrap().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        (dir, wt)
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_base_image_gets_one_exec_form_run_per_image_step() {
        let (_dir, wt) = worktree();
        let plan = plan_image(
            &recipe(
                base("rust:1.86"),
                vec![
                    step(
                        "apt-get update && apt-get install -y protobuf-compiler",
                        true,
                    ),
                    step("cargo fetch", false),
                    step("echo 'one\nFROM evil' > /x", true),
                ],
            ),
            &wt,
            SLUG,
            &labels(),
            INPUTS,
        )
        .unwrap();
        assert_eq!(
            plan.dockerfile,
            "FROM rust:1.86\n\
             RUN [\"/bin/sh\",\"-c\",\"apt-get update && apt-get install -y protobuf-compiler\"]\n\
             RUN [\"/bin/sh\",\"-c\",\"echo 'one\\nFROM evil' > /x\"]\n"
        );
        assert_eq!(
            plan.dockerfile.lines().count(),
            3,
            "a line break can't add an instruction"
        );
        assert_eq!(plan.base_build, None);
        assert!(plan.network, "the first image step uses the network");
        assert_eq!(plan.hash.len(), 64);
        assert_eq!(plan.tag, format!("clusia/{SLUG}:{}", &plan.hash[..16]));
        assert!(plan.notes.is_empty());
    }

    #[test]
    fn the_hash_follows_the_image_inputs_only() {
        let (_a, wt) = worktree();
        let (_b, other_wt) = worktree();
        let hash = |r: &Recipe, wt: &Path| plan_image(r, wt, SLUG, &labels(), INPUTS).unwrap().hash;
        let start = recipe(
            base("rust:1.86"),
            vec![
                step("apt-get install -y jq", true),
                step("cargo fetch", false),
            ],
        );
        let h = hash(&start, &wt);
        assert_eq!(
            hash(&start, &other_wt),
            h,
            "two worktrees of the repo share the image"
        );
        let mut work_changed = start.clone();
        work_changed.setup[1].run = "cargo fetch --locked".into();
        assert_eq!(
            hash(&work_changed, &wt),
            h,
            "work steps are not in the image"
        );
        let mut step_changed = start.clone();
        step_changed.setup[0].run = "apt-get install -y yq".into();
        assert_ne!(hash(&step_changed, &wt), h);
        let base_changed = recipe(base("rust:1.87"), start.setup.clone());
        assert_ne!(hash(&base_changed, &wt), h);
        let other_inputs =
            plan_image(&start, &wt, SLUG, &labels(), "b".repeat(64).as_str()).unwrap();
        assert_eq!(
            other_inputs.hash, h,
            "a pulled base does not depend on the repo's inputs"
        );
    }

    #[test]
    fn a_repo_dockerfile_is_built_first_as_the_base() {
        let (_dir, wt) = worktree();
        put(
            &wt.join("ci/Dockerfile"),
            "FROM alpine:3.20\nRUN apk add git\n",
        );
        let source = ImageSource::Dockerfile {
            path: "ci/Dockerfile".into(),
            target: Some("test".into()),
        };
        let plan = plan_image(&recipe(source, vec![]), &wt, SLUG, &labels(), INPUTS).unwrap();
        let build = plan.base_build.clone().unwrap();
        let tag_at = build.iter().position(|a| a == "-t").unwrap();
        let base_tag = build[tag_at + 1].clone();
        assert!(
            base_tag.starts_with(&format!("clusia/{SLUG}-base:")),
            "{base_tag}"
        );
        assert_eq!(
            build,
            [
                "build".to_string(),
                "-f".into(),
                wt.join("ci/Dockerfile").display().to_string(),
                "--target".into(),
                "test".into(),
                "--label".into(),
                "io.clusia.review=acme~widgets~7".into(),
                "--label".into(),
                "io.clusia.repo=acme/widgets".into(),
                "--label".into(),
                "io.clusia.kind=image".into(),
                "-t".into(),
                base_tag.clone(),
                wt.display().to_string(),
            ]
        );
        assert_eq!(plan.dockerfile, format!("FROM {base_tag}\n"));

        let before = plan.hash.clone();
        put(
            &wt.join("ci/Dockerfile"),
            "FROM alpine:3.20\nRUN apk add git curl\n",
        );
        let after = plan_image(
            &recipe(
                ImageSource::Dockerfile {
                    path: "ci/Dockerfile".into(),
                    target: Some("test".into()),
                },
                vec![],
            ),
            &wt,
            SLUG,
            &labels(),
            INPUTS,
        )
        .unwrap();
        assert_ne!(after.hash, before, "the Dockerfile text is in the hash");
        let other_inputs = plan_image(
            &recipe(
                ImageSource::Dockerfile {
                    path: "ci/Dockerfile".into(),
                    target: Some("test".into()),
                },
                vec![],
            ),
            &wt,
            SLUG,
            &labels(),
            "b".repeat(64).as_str(),
        )
        .unwrap();
        assert_ne!(
            other_inputs.hash, after.hash,
            "a built base follows the repo's inputs"
        );
    }

    #[test]
    fn a_dockerfile_that_resolves_outside_the_worktree_is_refused() {
        let (dir, wt) = worktree();
        let outside = dir.path().canonicalize().unwrap().join("Dockerfile");
        std::fs::write(&outside, "FROM alpine:3.20\n").unwrap();
        std::fs::create_dir_all(wt.join("ci")).unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("ci/Dockerfile")).unwrap();
        let source = ImageSource::Dockerfile {
            path: "ci/Dockerfile".into(),
            target: None,
        };
        assert_eq!(
            plan_image(&recipe(source, vec![]), &wt, SLUG, &labels(), INPUTS),
            Err("The Dockerfile ci/Dockerfile resolves outside the worktree".to_string())
        );
    }

    #[test]
    fn a_fifo_named_as_the_dockerfile_is_refused_without_blocking() {
        let (dir, wt) = worktree();
        let fifo = std::ffi::CString::new(wt.join("Dockerfile").to_str().unwrap()).unwrap();
        // SAFETY: `fifo` is a NUL-terminated path that lives across the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let source = ImageSource::Dockerfile {
                path: "Dockerfile".into(),
                target: None,
            };
            let _ = tx.send(plan_image(
                &recipe(source, vec![]),
                &wt,
                SLUG,
                &labels(),
                INPUTS,
            ));
        });
        // A FIFO with no writer would block an open for reading forever.
        let planned = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert_eq!(
            planned.expect("plan_image returned"),
            Err("The Dockerfile Dockerfile is not a file".to_string())
        );
        drop(dir);
    }

    #[test]
    fn a_devcontainer_build_reads_its_dockerfile_context_and_args() {
        let (_dir, wt) = worktree();
        put(&wt.join(".devcontainer/Dockerfile"), "FROM node:22\n");
        put(
            &wt.join(".devcontainer/devcontainer.json"),
            r#"{
  // the image used for development
  "name": "widgets",
  "build": {
    "dockerfile": "Dockerfile",
    "context": "..",
    "args": { "VARIANT": "bookworm", "NODE_VERSION": "22" },
    "target": "dev",
  },
  "features": { "ghcr.io/devcontainers/features/node:1": {} },
  "postCreateCommand": "npm ci", /* runs after create */
}"#,
        );
        let plan = plan_image(
            &recipe(ImageSource::Devcontainer, vec![]),
            &wt,
            SLUG,
            &labels(),
            INPUTS,
        )
        .unwrap();
        let build = plan.base_build.unwrap();
        assert_eq!(
            build[..9],
            [
                "build".to_string(),
                "-f".into(),
                wt.join(".devcontainer/Dockerfile").display().to_string(),
                "--target".into(),
                "dev".into(),
                "--build-arg".into(),
                "NODE_VERSION=22".into(),
                "--build-arg".into(),
                "VARIANT=bookworm".into(),
            ]
        );
        assert_eq!(
            build.last().unwrap(),
            &wt.display().to_string(),
            "context `..` is the worktree"
        );
        assert_eq!(
            plan.notes,
            [
                "devcontainer.json `features` is not supported yet; Clúsia ignored it",
                "devcontainer.json `postCreateCommand` is not supported yet; Clúsia ignored it",
            ]
        );
    }

    #[test]
    fn a_devcontainer_context_outside_the_worktree_is_refused() {
        let (_dir, wt) = worktree();
        put(&wt.join(".devcontainer/Dockerfile"), "FROM node:22\n");
        put(
            &wt.join(".devcontainer/devcontainer.json"),
            r#"{"build": {"dockerfile": "Dockerfile", "context": "../.."}}"#,
        );
        assert_eq!(
            plan_image(
                &recipe(ImageSource::Devcontainer, vec![]),
                &wt,
                SLUG,
                &labels(),
                INPUTS
            ),
            Err("The devcontainer build context ../.. resolves outside the worktree".to_string())
        );
    }

    #[test]
    fn a_devcontainer_image_is_the_base() {
        let (_dir, wt) = worktree();
        put(
            &wt.join(".devcontainer.json"),
            r#"{"image": "mcr.microsoft.com/devcontainers/rust:1-bookworm"}"#,
        );
        let plan = plan_image(
            &recipe(ImageSource::Devcontainer, vec![]),
            &wt,
            SLUG,
            &labels(),
            INPUTS,
        )
        .unwrap();
        assert_eq!(
            plan.dockerfile,
            "FROM mcr.microsoft.com/devcontainers/rust:1-bookworm\n"
        );
        assert_eq!(plan.base_build, None);
    }

    #[test]
    fn image_names_are_checked() {
        let (_dir, wt) = worktree();
        put(
            &wt.join(".devcontainer.json"),
            r#"{"image": "alpine\nRUN curl x"}"#,
        );
        assert_eq!(
            plan_image(
                &recipe(ImageSource::Devcontainer, vec![]),
                &wt,
                SLUG,
                &labels(),
                INPUTS
            ),
            Err("alpine\\nRUN curl x is not a valid image name".to_string())
        );
        for ok in ["rust:1.86", "ghcr.io/acme/widgets@sha256:abc", "alpine"] {
            assert!(valid_image_name(ok), "{ok}");
        }
        for bad in ["", "-x", "a b", "a\nb", "a;b"] {
            assert!(!valid_image_name(bad), "{bad:?}");
        }
        let mut no_image = recipe(base("rust"), vec![]);
        no_image.image = None;
        assert_eq!(
            plan_image(&no_image, &wt, SLUG, &labels(), INPUTS),
            Err(crate::plan::NO_IMAGE.to_string())
        );
    }

    #[test]
    fn reduce_devcontainer_keeps_strings_and_refuses_what_it_cannot_build() {
        let (dc, notes) = reduce_devcontainer(
            r#"{"image": "registry.example/a//b:1", /* c */ "runArgs": ["--privileged"],}"#,
        )
        .unwrap();
        assert_eq!(dc.image.as_deref(), Some("registry.example/a//b:1"));
        assert_eq!(
            notes,
            ["devcontainer.json `runArgs` is not supported yet; Clúsia ignored it"]
        );
        assert_eq!(
            reduce_devcontainer(r#"{"dockerComposeFile": "c.yml", "service": "app"}"#),
            Err("Compose-based devcontainers are not supported yet".to_string())
        );
        assert_eq!(
            reduce_devcontainer(r#"{"name": "x"}"#),
            Err("devcontainer.json names no image and no build.dockerfile".to_string())
        );
        assert_eq!(
            reduce_devcontainer(r#"{"build": {"dockerfile": "D", "args": {"N": 22}}}"#),
            Err("build.args.N must be a string".to_string())
        );
        let (dc, notes) =
            reduce_devcontainer(r#"{"image": "a", "build": {"dockerfile": "D"}}"#).unwrap();
        assert_eq!(
            (dc.dockerfile.as_deref(), dc.context.as_deref()),
            (Some("D"), Some("."))
        );
        assert_eq!(
            notes,
            ["devcontainer.json sets both image and build; Clúsia used build"]
        );
    }
}
