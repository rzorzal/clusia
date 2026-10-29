//! Removing what Clúsia made, by label only, and sizing what it keeps.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_core::printable::printable;
use clusia_core::tests::{DiskItem, DiskKind, DiskReport};

use crate::docker::Docker;
use crate::exec::{Exec, Output};
use crate::names::LABEL_REPO;

pub const CLEANUP_TIMEOUT: Duration = Duration::from_secs(120);
pub const BASE_NOTE: &str = "Base images that builds pulled are not counted or removed; docker image prune can remove the unused ones.";

/// Removes every container, network, image and volume labelled with `repo`. Each removal
/// names only ids from a labelled listing: it never prunes and never names an image, so base
/// images stay. A failure is noted and the rest still runs.
pub async fn remove_repo(exec: &dyn Exec, docker: &Docker, repo: &str) -> Result<(), String> {
    let filter = format!("label={LABEL_REPO}={repo}");
    let pairs: [(&[&str], &[&str]); 4] = [
        (&["ps", "-aq"], &["rm", "-f"]),
        (&["network", "ls", "-q"], &["network", "rm"]),
        (&["image", "ls", "-q"], &["image", "rm", "-f"]),
        (&["volume", "ls", "-q"], &["volume", "rm", "-f"]),
    ];
    let mut notes = Vec::new();
    for (list, remove) in pairs {
        let mut args: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        args.extend(["--filter".to_string(), filter.clone()]);
        let found = match call(exec, docker, &args).await {
            Ok(out) => ids(&out),
            Err(note) => {
                notes.push(note);
                continue;
            }
        };
        if found.is_empty() {
            continue;
        }
        let mut args: Vec<String> = remove.iter().map(|s| s.to_string()).collect();
        args.extend(found);
        if let Err(note) = call(exec, docker, &args).await {
            notes.push(note);
        }
    }
    joined(notes)
}

/// Removes `clusia-` containers that no live run or stack owns: a container of a dead compose
/// project takes its project down once, any other goes into one `rm -f`. `live` holds the
/// names of running run containers and the projects of live stacks.
pub async fn remove_orphans(
    exec: &dyn Exec,
    docker: &Docker,
    live: &[String],
) -> Result<(), String> {
    let format = "{{.Names}} {{.Label \"com.docker.compose.project\"}}";
    let args = [
        "ps",
        "-a",
        "--filter",
        "label=io.clusia.review",
        "--format",
        format,
    ];
    let out = call(exec, docker, &args).await?;
    let mut projects: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_end();
        let (name, project) = line.split_once(' ').unwrap_or((line, ""));
        if !name.starts_with("clusia-") {
            continue;
        }
        let is_live = |s: &str| live.iter().any(|l| l == s);
        if project.starts_with("clusia-") {
            if !is_live(project) && !projects.iter().any(|p| p == project) {
                projects.push(project.to_string());
            }
        } else if !is_live(name) && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    let mut notes = Vec::new();
    for project in projects {
        let args = ["compose", "-p", &project, "down", "-v", "--remove-orphans"];
        if let Err(note) = call(exec, docker, &args).await {
            notes.push(note);
        }
    }
    if !names.is_empty() {
        let mut args = vec!["rm".to_string(), "-f".to_string()];
        args.extend(names);
        if let Err(note) = call(exec, docker, &args).await {
            notes.push(note);
        }
    }
    joined(notes)
}

/// What Clúsia keeps on disk: labelled images and volumes (when an engine is there), then the
/// cache folders and the worktree output folders.
pub async fn disk_report(
    exec: &dyn Exec,
    docker: Option<&Docker>,
    caches: &[PathBuf],
    worktree_dirs: &[PathBuf],
) -> DiskReport {
    let mut items = Vec::new();
    let mut unread = false;
    if let Some(docker) = docker {
        items.extend(images(exec, docker).await);
        let (volumes, sizes_read) = volumes(exec, docker).await;
        items.extend(volumes);
        unread = !sizes_read;
    }
    for (paths, kind) in [
        (caches, DiskKind::Cache),
        (worktree_dirs, DiskKind::WorktreeDir),
    ] {
        for path in paths {
            let repo = (kind == DiskKind::Cache)
                .then(|| path.file_name())
                .flatten()
                .map(|name| name.to_string_lossy().replacen('~', "/", 1));
            let walked = path.clone();
            let bytes = tokio::task::spawn_blocking(move || folder_size(&walked))
                .await
                .unwrap_or(0);
            items.push(DiskItem {
                kind,
                name: path.display().to_string(),
                repo,
                bytes,
            });
        }
    }
    let total_bytes = items.iter().map(|i| i.bytes).sum();
    let note = if unread {
        format!("{BASE_NOTE} Volume sizes could not be read.")
    } else {
        BASE_NOTE.to_string()
    };
    DiskReport {
        items,
        total_bytes,
        note: Some(note),
    }
}

async fn images(exec: &dyn Exec, docker: &Docker) -> Vec<DiskItem> {
    let list = ["image", "ls", "--filter", LABEL_REPO, "--format", "{{.ID}}"];
    let Ok(out) = call(exec, docker, &list).await else {
        return vec![];
    };
    // One image with two tags is listed twice.
    let found = ids(&out);
    if found.is_empty() {
        return vec![];
    }
    let format = format!(
        "{{{{.Size}}}} {{{{index .Config.Labels \"{LABEL_REPO}\"}}}} {{{{json .RepoTags}}}}"
    );
    let mut args = vec![
        "image".to_string(),
        "inspect".into(),
        "--format".into(),
        format,
    ];
    args.extend(found.iter().cloned());
    let Ok(out) = call(exec, docker, &args).await else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .zip(&found)
        .map(|(line, id)| {
            let mut parts = line.splitn(3, ' ');
            let bytes = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let repo = parts.next().filter(|r| !r.is_empty()).map(str::to_string);
            let tags: Vec<String> = parts
                .next()
                .and_then(|t| serde_json::from_str(t).ok())
                .unwrap_or_default();
            DiskItem {
                kind: DiskKind::Image,
                name: tags.into_iter().next().unwrap_or_else(|| id.clone()),
                repo,
                bytes,
            }
        })
        .collect()
}

/// The labelled volumes, and whether every size could be read.
async fn volumes(exec: &dyn Exec, docker: &Docker) -> (Vec<DiskItem>, bool) {
    let format = format!("{{{{.Name}}}} {{{{.Label \"{LABEL_REPO}\"}}}}");
    let list = ["volume", "ls", "--filter", LABEL_REPO, "--format", &format];
    let Ok(out) = call(exec, docker, &list).await else {
        return (vec![], true);
    };
    let listed: Vec<(String, Option<String>)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| match line.split_once(' ') {
            Some((name, repo)) => (
                name.to_string(),
                Some(repo.to_string()).filter(|r| !r.is_empty()),
            ),
            None => (line.to_string(), None),
        })
        .collect();
    if listed.is_empty() {
        return (vec![], true);
    }
    let df = ["system", "df", "-v", "--format", "{{json .Volumes}}"];
    let sizes: Option<BTreeMap<String, String>> = call(exec, docker, &df)
        .await
        .ok()
        .and_then(|out| serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout).ok())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let name = row.get("Name")?.as_str()?;
                    let size = row.get("Size")?.as_str()?;
                    Some((name.to_string(), size.to_string()))
                })
                .collect()
        });
    let mut all_read = true;
    let items = listed
        .into_iter()
        .map(|(name, repo)| {
            let bytes = sizes
                .as_ref()
                .and_then(|s| s.get(&name))
                .and_then(|size| parse_size(size));
            all_read &= bytes.is_some();
            DiskItem {
                kind: DiskKind::Volume,
                name,
                repo,
                bytes: bytes.unwrap_or(0),
            }
        })
        .collect();
    (items, all_read)
}

/// The bytes of the regular files under `path`, never following a symlink.
fn folder_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_file() {
        return meta.len();
    }
    if !meta.file_type().is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| folder_size(&entry.path()))
                .sum()
        })
        .unwrap_or(0)
}

/// Docker's human sizes, in decimal units.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let scale = match unit.trim() {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((number * scale).round() as u64)
}

/// Trimmed non-empty lines, each once, in the order seen.
fn ids(out: &Output) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() && !seen.iter().any(|s| s == line) {
            seen.push(line.to_string());
        }
    }
    seen
}

/// Runs one engine call; a failure is `docker <first arg> failed: <why>`.
async fn call(
    exec: &dyn Exec,
    docker: &Docker,
    args: &[impl AsRef<str>],
) -> Result<Output, String> {
    let first = args
        .first()
        .map(|a| a.as_ref())
        .unwrap_or_default()
        .to_string();
    let first = if first == "compose" {
        "compose down".to_string()
    } else {
        first
    };
    let cmd = docker.command(args.iter().map(|a| a.as_ref()));
    let out = exec
        .output(&cmd, CLEANUP_TIMEOUT)
        .await
        .map_err(|e| format!("docker {first} failed: {e}"))?;
    if out.timed_out {
        return Err(format!("docker {first} failed: it timed out"));
    }
    if out.code != Some(0) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        return Err(format!("docker {first} failed: {}", printable(line)));
    }
    Ok(out)
}

fn joined(notes: Vec<String>) -> Result<(), String> {
    if notes.is_empty() {
        Ok(())
    } else {
        Err(notes.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use crate::testkit::{FakeExec, FakeReply};

    use super::*;

    fn docker() -> Docker {
        Docker {
            cli: "/opt/homebrew/bin/docker".into(),
            env: vec![],
        }
    }

    fn calls(fake: &FakeExec) -> Vec<Vec<String>> {
        fake.calls()
            .iter()
            .map(|c| {
                c.args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn cleanup_touches_only_labelled_objects() {
        let fake = FakeExec::new();
        fake.on("docker", &["ps"], FakeReply::ok("c1\nc2\n"))
            .on("docker", &["rm"], FakeReply::ok(""))
            .on("docker", &["network", "ls"], FakeReply::ok(""))
            .on("docker", &["image", "ls"], FakeReply::ok("i1\ni1\ni2\n"))
            .on("docker", &["image", "rm"], FakeReply::ok(""))
            .on(
                "docker",
                &["volume", "ls"],
                FakeReply::ok("clusia-cache-acme-widgets-abc123-cargo\n"),
            )
            .on("docker", &["volume", "rm"], FakeReply::ok(""));
        assert_eq!(remove_repo(&fake, &docker(), "acme/widgets").await, Ok(()));
        let filter = "label=io.clusia.repo=acme/widgets";
        assert_eq!(
            calls(&fake),
            [
                strings(&["ps", "-aq", "--filter", filter]),
                strings(&["rm", "-f", "c1", "c2"]),
                strings(&["network", "ls", "-q", "--filter", filter]),
                strings(&["image", "ls", "-q", "--filter", filter]),
                strings(&["image", "rm", "-f", "i1", "i2"]),
                strings(&["volume", "ls", "-q", "--filter", filter]),
                strings(&[
                    "volume",
                    "rm",
                    "-f",
                    "clusia-cache-acme-widgets-abc123-cargo"
                ]),
            ]
        );
        for call in calls(&fake) {
            assert!(
                !call
                    .iter()
                    .any(|a| a == "prune" || a == "rmi" || a == "alpine:3.20"),
                "{call:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_removal_is_reported_and_the_rest_still_runs() {
        let fake = FakeExec::new();
        fake.on("docker", &["ps"], FakeReply::ok("c1\n"))
            .on(
                "docker",
                &["rm"],
                FakeReply::fail(1, "Error: container c1 is restarting\n"),
            )
            .on("docker", &["network", "ls"], FakeReply::ok(""))
            .on("docker", &["image", "ls"], FakeReply::ok(""))
            .on("docker", &["volume", "ls"], FakeReply::ok("v1\n"))
            .on("docker", &["volume", "rm"], FakeReply::ok(""));
        assert_eq!(
            remove_repo(&fake, &docker(), "acme/widgets").await,
            Err("docker rm failed: Error: container c1 is restarting".to_string())
        );
        assert_eq!(
            calls(&fake).last().unwrap(),
            &strings(&["volume", "rm", "-f", "v1"])
        );
    }

    #[tokio::test]
    async fn the_sweep_spares_live_runs_and_stacks() {
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["ps"],
            FakeReply::ok(
                "clusia-run-acme-widgets-abc123-7-1 \n\
                 clusia-run-acme-widgets-abc123-7-2\n\
                 clusia-acme-widgets-abc123-7-db-1 clusia-acme-widgets-abc123-7\n\
                 clusia-acme-widgets-abc123-8-db-1 clusia-acme-widgets-abc123-8\n\
                 clusia-acme-widgets-abc123-8-cache-1 clusia-acme-widgets-abc123-8\n\
                 octo-postgres-1 clusia-x\n",
            ),
        )
        .on("docker", &["compose"], FakeReply::ok(""))
        .on("docker", &["rm"], FakeReply::ok(""));
        let live = strings(&[
            "clusia-run-acme-widgets-abc123-7-1",
            "clusia-acme-widgets-abc123-7",
        ]);
        assert_eq!(remove_orphans(&fake, &docker(), &live).await, Ok(()));
        assert_eq!(
            calls(&fake),
            [
                strings(&[
                    "ps",
                    "-a",
                    "--filter",
                    "label=io.clusia.review",
                    "--format",
                    "{{.Names}} {{.Label \"com.docker.compose.project\"}}",
                ]),
                strings(&[
                    "compose",
                    "-p",
                    "clusia-acme-widgets-abc123-8",
                    "down",
                    "-v",
                    "--remove-orphans"
                ]),
                strings(&["rm", "-f", "clusia-run-acme-widgets-abc123-7-2"]),
            ]
        );

        let quiet = FakeExec::new();
        quiet.on(
            "docker",
            &["ps"],
            FakeReply::ok("clusia-run-acme-widgets-abc123-7-1\n"),
        );
        assert_eq!(remove_orphans(&quiet, &docker(), &live).await, Ok(()));
        assert_eq!(
            calls(&quiet).len(),
            1,
            "nothing to remove, nothing else runs"
        );
    }

    #[tokio::test]
    async fn the_disk_report_counts_images_volumes_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        let cache = top.join("tests/cache/acme~widgets");
        std::fs::create_dir_all(cache.join("cargo/home")).unwrap();
        std::fs::write(cache.join("cargo/home/a"), vec![0u8; 100]).unwrap();
        std::fs::write(cache.join("b"), vec![0u8; 200]).unwrap();
        std::fs::write(top.join("big"), vec![0u8; 10_000]).unwrap();
        std::os::unix::fs::symlink(top.join("big"), cache.join("link")).unwrap();
        let modules = top.join("wt/node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(modules.join("x.js"), vec![0u8; 50]).unwrap();

        let fake = FakeExec::new();
        fake.on("docker", &["image", "ls"], FakeReply::ok("aaa\nbbb\naaa\n"))
            .on(
                "docker",
                &["image", "inspect"],
                FakeReply::ok(
                    "1000 acme/widgets [\"clusia/acme-widgets-abc123:0123456789abcdef\"]\n2500 acme/widgets []\n",
                ),
            )
            .on("docker", &["volume", "ls"], FakeReply::ok("clusia-cache-acme-widgets-abc123-cargo acme/widgets\n"))
            .on(
                "docker",
                &["system", "df"],
                FakeReply::ok(r#"[{"Name":"clusia-cache-acme-widgets-abc123-cargo","Size":"1.5GB"},{"Name":"other","Size":"3MB"}]"#),
            );
        let report = disk_report(
            &fake,
            Some(&docker()),
            std::slice::from_ref(&cache),
            std::slice::from_ref(&modules),
        )
        .await;
        let repo = Some("acme/widgets".to_string());
        assert_eq!(
            report.items,
            [
                DiskItem {
                    kind: DiskKind::Image,
                    name: "clusia/acme-widgets-abc123:0123456789abcdef".into(),
                    repo: repo.clone(),
                    bytes: 1000
                },
                DiskItem {
                    kind: DiskKind::Image,
                    name: "bbb".into(),
                    repo: repo.clone(),
                    bytes: 2500
                },
                DiskItem {
                    kind: DiskKind::Volume,
                    name: "clusia-cache-acme-widgets-abc123-cargo".into(),
                    repo: repo.clone(),
                    bytes: 1_500_000_000
                },
                DiskItem {
                    kind: DiskKind::Cache,
                    name: cache.display().to_string(),
                    repo,
                    bytes: 300
                },
                DiskItem {
                    kind: DiskKind::WorktreeDir,
                    name: modules.display().to_string(),
                    repo: None,
                    bytes: 50
                },
            ]
        );
        assert_eq!(report.total_bytes, 1000 + 2500 + 1_500_000_000 + 300 + 50);
        assert_eq!(report.note.as_deref(), Some(BASE_NOTE));
        let calls = calls(&fake);
        assert_eq!(
            calls[1],
            strings(&[
                "image",
                "inspect",
                "--format",
                "{{.Size}} {{index .Config.Labels \"io.clusia.repo\"}} {{json .RepoTags}}",
                "aaa",
                "bbb",
            ])
        );

        let no_engine = disk_report(&fake, None, &[cache], &[]).await;
        assert_eq!(no_engine.items.len(), 1);
        assert_eq!(no_engine.total_bytes, 300);
    }

    #[test]
    fn docker_sizes_are_decimal() {
        assert_eq!(parse_size("1.5GB"), Some(1_500_000_000));
        assert_eq!(parse_size("512kB"), Some(512_000));
        assert_eq!(parse_size("3MB"), Some(3_000_000));
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("2TB"), Some(2_000_000_000_000));
        assert_eq!(parse_size("N/A"), None);
    }
}
