//! The `docker run` argument list and what a run container may see.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use clusia_core::tests::Ecosystem;

use crate::eco::{eco_env, name};
use crate::exec::CommandSpec;
use crate::names::{Labels, cache_volume};

pub const WORK_DIR: &str = "/work";
pub const CACHE_DIR: &str = "/cache";
pub const PIDS_LIMIT: &str = "4096";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Docker {
    pub cli: PathBuf,
    pub env: Vec<(OsString, OsString)>,
}

impl Docker {
    /// Every engine call: the CLI with only its own thin environment, never the daemon's.
    pub fn command(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> CommandSpec {
        let mut cmd = CommandSpec::new(&self.cli).args(args).clear_env();
        cmd.env = self.env.clone();
        cmd
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerNet {
    None,
    Bridge,
    Internal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMounts {
    pub worktree: PathBuf,
    pub git_common: PathBuf,
    /// `(volume, container path)`.
    pub caches: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerJob {
    pub name: String,
    pub image: String,
    pub command: String,
    pub env: Vec<(String, String)>,
    pub net: ContainerNet,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u32>,
    pub labels: Labels,
}

/// The whole `docker run` argument list. The mounts are the only way in: the worktree, its
/// `.clusia` and `.git` file read-only, the git dir read-only at its host path and the cache
/// volumes. `--pull never` keeps a missing `clusia/…` tag from being pulled from a namespace
/// Clúsia does not own; `--user 0:0` keeps an image's `USER` from changing who runs; `--init`
/// ends the container with its client, since a PID 1 `/bin/sh` ignores SIGTERM.
pub fn run_args(job: &ContainerJob, mounts: &RunMounts) -> Vec<String> {
    let net = match &job.net {
        ContainerNet::None => "none".to_string(),
        ContainerNet::Bridge => "bridge".to_string(),
        ContainerNet::Internal(name) => name.clone(),
    };
    let wt = mounts.worktree.display();
    let git = mounts.git_common.display();
    let mut args: Vec<String> = ["run", "--rm", "--name", &job.name, "--network", &net]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(["--pull", "never", "--user", "0:0", "--init"].map(String::from));
    args.extend(
        [
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            PIDS_LIMIT,
        ]
        .map(String::from),
    );
    args.extend(job.labels.args());
    for volume in [
        format!("{wt}:{WORK_DIR}"),
        format!("{wt}/.clusia:{WORK_DIR}/.clusia:ro"),
        format!("{wt}/.git:{WORK_DIR}/.git:ro"),
        format!("{git}:{git}:ro"),
    ]
    .into_iter()
    .chain(
        mounts
            .caches
            .iter()
            .map(|(name, path)| format!("{name}:{path}")),
    ) {
        args.extend(["-v".to_string(), volume]);
    }
    args.extend(["-e".to_string(), "GIT_OPTIONAL_LOCKS=0".to_string()]);
    for (key, value) in &job.env {
        args.extend(["-e".to_string(), format!("{key}={value}")]);
    }
    args.extend(["-w", WORK_DIR].map(String::from));
    if let Some(cpus) = job.cpus {
        args.extend(["--cpus".to_string(), cpus.to_string()]);
    }
    if let Some(mb) = job.memory_mb {
        args.extend(["--memory".to_string(), format!("{mb}m")]);
    }
    args.extend(["--entrypoint", "/bin/sh", &job.image, "-c", &job.command].map(String::from));
    args
}

pub fn kill_args(name: &str) -> Vec<String> {
    vec!["kill".to_string(), name.to_string()]
}

/// A `-v` value is split on `:`, so a path holding one (or a `,`) cannot be mounted as
/// written; a path that is not UTF-8 cannot be written into the argument at all.
pub fn mount_problem(mounts: &RunMounts) -> Option<String> {
    [&mounts.worktree, &mounts.git_common]
        .into_iter()
        .find_map(|path| match path.to_str() {
            None => Some(format!(
                "Clúsia can't mount {} into a container: the path is not UTF-8.",
                path.display()
            )),
            Some(text) if text.contains([':', ',']) => Some(format!(
                "Clúsia can't mount {text} into a container: the path holds a ':' or ','."
            )),
            Some(_) => None,
        })
}

/// `<worktree>/.git` must be a plain file: `docker run -v` would create a missing bind source
/// as a folder, and a symlink would mount wherever it points.
pub fn git_file_problem(worktree: &Path) -> Option<String> {
    let git = worktree.join(".git");
    match std::fs::symlink_metadata(&git) {
        Ok(meta) if meta.file_type().is_file() => None,
        _ => Some(format!(
            "Clúsia can't mount {} read-only: it is not a plain file.",
            git.display()
        )),
    }
}

pub fn cache_mounts(slug: &str, ecos: &[Ecosystem]) -> Vec<(String, String)> {
    ecos.iter()
        .map(|&eco| {
            (
                cache_volume(slug, eco),
                format!("{CACHE_DIR}/{}", name(eco)),
            )
        })
        .collect()
}

/// The ecosystems' cache variables, then the recipe's `env` without a key already set or
/// `GIT_OPTIONAL_LOCKS`, which `run_args` always sets itself.
pub fn job_env(ecos: &[Ecosystem], recipe_env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ecos
        .iter()
        .flat_map(|&eco| eco_env(eco, CACHE_DIR))
        .collect();
    for (key, value) in recipe_env {
        if key != "GIT_OPTIONAL_LOCKS" && !env.iter().any(|(k, _)| k == key) {
            env.push((key.clone(), value.clone()));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use crate::names::LabelKind;

    use super::*;

    fn labels(kind: LabelKind) -> Labels {
        Labels {
            review: "acme~widgets~7".into(),
            repo: "acme/widgets".into(),
            kind,
        }
    }

    fn mounts() -> RunMounts {
        RunMounts {
            worktree: "/d/worktrees/acme~widgets~7".into(),
            git_common: "/d/repos/acme~widgets/.git".into(),
            caches: vec![(
                "clusia-cache-acme-widgets-abc123-cargo".into(),
                "/cache/cargo".into(),
            )],
        }
    }

    fn job() -> ContainerJob {
        ContainerJob {
            name: "clusia-run-acme-widgets-abc123-7-1".into(),
            image: "clusia/acme-widgets-abc123:0123456789abcdef".into(),
            command: "cargo test --workspace -- --exact 'a b'".into(),
            env: job_env(
                &[Ecosystem::Cargo],
                &BTreeMap::from([("RUST_BACKTRACE".to_string(), "1".to_string())]),
            ),
            net: ContainerNet::None,
            cpus: Some(4),
            memory_mb: Some(4096),
            labels: labels(LabelKind::Run),
        }
    }

    #[test]
    fn run_args_mount_only_the_worktree_git_and_caches() {
        let args = run_args(&job(), &mounts());
        assert_eq!(
            args,
            [
                "run",
                "--rm",
                "--name",
                "clusia-run-acme-widgets-abc123-7-1",
                "--network",
                "none",
                "--pull",
                "never",
                "--user",
                "0:0",
                "--init",
                "--security-opt",
                "no-new-privileges",
                "--pids-limit",
                "4096",
                "--label",
                "io.clusia.review=acme~widgets~7",
                "--label",
                "io.clusia.repo=acme/widgets",
                "--label",
                "io.clusia.kind=run",
                "-v",
                "/d/worktrees/acme~widgets~7:/work",
                "-v",
                "/d/worktrees/acme~widgets~7/.clusia:/work/.clusia:ro",
                "-v",
                "/d/worktrees/acme~widgets~7/.git:/work/.git:ro",
                "-v",
                "/d/repos/acme~widgets/.git:/d/repos/acme~widgets/.git:ro",
                "-v",
                "clusia-cache-acme-widgets-abc123-cargo:/cache/cargo",
                "-e",
                "GIT_OPTIONAL_LOCKS=0",
                "-e",
                "CARGO_HOME=/cache/cargo/home",
                "-e",
                "CARGO_TARGET_DIR=/cache/cargo/target",
                "-e",
                "RUST_BACKTRACE=1",
                "-w",
                "/work",
                "--cpus",
                "4",
                "--memory",
                "4096m",
                "--entrypoint",
                "/bin/sh",
                "clusia/acme-widgets-abc123:0123456789abcdef",
                "-c",
                "cargo test --workspace -- --exact 'a b'",
            ]
        );
        // Whatever the job, nothing else from the host gets in.
        let volumes: Vec<&String> = args
            .windows(2)
            .filter(|w| w[0] == "-v")
            .map(|w| &w[1])
            .collect();
        assert_eq!(volumes.len(), 5);
        for flag in [
            "--privileged",
            "-p",
            "--publish",
            "--mount",
            "--env-file",
            "--volumes-from",
        ] {
            assert!(!args.iter().any(|a| a == flag), "{flag}");
        }
        assert!(!args.iter().any(|a| a.contains("docker.sock")));
        let envs: Vec<&String> = args
            .windows(2)
            .filter(|w| w[0] == "-e")
            .map(|w| &w[1])
            .collect();
        assert_eq!(
            envs.len(),
            4,
            "only the lock flag, the caches and the recipe's env"
        );
    }

    #[test]
    fn the_network_and_limits_follow_the_job() {
        let mut job = job();
        job.net = ContainerNet::Bridge;
        job.cpus = None;
        job.memory_mb = None;
        let args = run_args(
            &job,
            &RunMounts {
                caches: vec![],
                ..mounts()
            },
        );
        let at = args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(args[at + 1], "bridge");
        assert!(!args.iter().any(|a| a == "--cpus" || a == "--memory"));
        job.net = ContainerNet::Internal("clusia-acme-widgets-abc123-7_clusia_internal".into());
        let args = run_args(&job, &mounts());
        assert_eq!(args[at + 1], "clusia-acme-widgets-abc123-7_clusia_internal");
    }

    #[test]
    fn job_env_keeps_caches_and_drops_repeats() {
        let recipe = BTreeMap::from([
            ("CARGO_HOME".to_string(), "/evil".to_string()),
            ("GIT_OPTIONAL_LOCKS".to_string(), "1".to_string()),
            ("RUST_BACKTRACE".to_string(), "1".to_string()),
        ]);
        assert_eq!(
            job_env(&[Ecosystem::Cargo], &recipe),
            [
                ("CARGO_HOME".to_string(), "/cache/cargo/home".to_string()),
                (
                    "CARGO_TARGET_DIR".to_string(),
                    "/cache/cargo/target".to_string()
                ),
                ("RUST_BACKTRACE".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(
            cache_mounts("acme-widgets-abc123", &[Ecosystem::Cargo, Ecosystem::Node]),
            [
                (
                    "clusia-cache-acme-widgets-abc123-cargo".to_string(),
                    "/cache/cargo".to_string()
                ),
                (
                    "clusia-cache-acme-widgets-abc123-node".to_string(),
                    "/cache/node".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_path_docker_would_split_is_refused() {
        assert_eq!(mount_problem(&mounts()), None);
        let bad = RunMounts {
            worktree: "/d/a:b".into(),
            ..mounts()
        };
        assert_eq!(
            mount_problem(&bad).as_deref(),
            Some("Clúsia can't mount /d/a:b into a container: the path holds a ':' or ','.")
        );
        let bad = RunMounts {
            git_common: "/d/x,y/.git".into(),
            ..mounts()
        };
        assert!(mount_problem(&bad).unwrap().contains("/d/x,y/.git"));
    }

    #[test]
    fn every_call_gets_only_the_cli_env() {
        let docker = Docker {
            cli: "/opt/homebrew/bin/docker".into(),
            env: vec![(
                "PATH".into(),
                "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
            )],
        };
        let cmd = docker.command(kill_args("clusia-run-x-7-1"));
        assert_eq!(cmd.program, PathBuf::from("/opt/homebrew/bin/docker"));
        assert_eq!(
            cmd.args,
            [OsString::from("kill"), OsString::from("clusia-run-x-7-1")]
        );
        assert!(cmd.clear_env);
        assert_eq!(cmd.env, docker.env);
    }
}
