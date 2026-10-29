//! The compose stack: the version gate, the refusals and the override.

use std::path::{Component, Path, PathBuf};

use clusia_core::printable::printable;
use serde_json::{Map, Value};

use crate::names::{LabelKind, Labels};

pub const COMPOSE_MIN: (u32, u32, u32) = (2, 24, 4);
pub const INTERNAL_NET: &str = "clusia_internal";

/// Service keys that reach the host or escape the `clusia-` names and labels, in the order
/// their refusals are given. `privileged` counts only when `true`.
const SERVICE_KEYS: [&str; 11] = [
    "network_mode",
    "privileged",
    "devices",
    "pid",
    "ipc",
    "volumes_from",
    "cap_add",
    "security_opt",
    "userns_mode",
    "cgroup_parent",
    "container_name",
];

/// Reads the leading `major.minor.patch` (an optional `v`, missing parts 0, any suffix
/// ignored) and compares it with [`COMPOSE_MIN`].
pub fn compose_version_ok(version: &str) -> bool {
    let text = version.strip_prefix('v').unwrap_or(version);
    if !text.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let mut parts = text.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().unwrap_or(0)
    });
    let found = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    found >= COMPOSE_MIN
}

pub fn internal_network(project: &str) -> String {
    format!("{project}_{INTERNAL_NET}")
}

/// What Clúsia refuses in the merged `docker compose config --format json`. `names` are the
/// recipe's services, each of which must be in the merged file.
pub fn compose_problems(merged: &Value, worktree: &Path, names: &[String]) -> Vec<String> {
    refusals(merged, Some(worktree), names)
}

/// The refusals; those that need the worktree (build contexts and `env_file`) run only with
/// `Some`.
fn refusals(merged: &Value, worktree: Option<&Path>, names: &[String]) -> Vec<String> {
    let wt = worktree.map(resolved);
    let mut problems = Vec::new();
    let services = section(merged, "services");
    for (name, service) in sorted(services) {
        let s = printable(name);
        for key in SERVICE_KEYS {
            if set(service.get(key), key == "privileged") {
                problems.push(format!("service {s} sets {key}, which Clúsia refuses"));
            }
        }
        if names_host_gateway(service.get("extra_hosts")) {
            problems.push(format!("service {s} maps a host to host-gateway"));
        }
        if let Some(build) = service.get("build").and_then(Value::as_object) {
            build_refusals(&s, build, wt.as_deref(), &mut problems);
        }
        if let Some(wt) = &wt {
            for path in env_files(service.get("env_file")) {
                if !inside(wt, &path) {
                    problems.push(format!(
                        "service {s} reads an env_file outside the worktree: {}",
                        printable(&path)
                    ));
                }
            }
        }
    }

    for (name, network) in sorted(section(merged, "networks")) {
        if set(network.get("external"), true) {
            problems.push(format!("network {} is external", printable(name)));
        }
    }
    let project = merged.get("name").and_then(Value::as_str).unwrap_or("");
    for (name, volume) in sorted(section(merged, "volumes")) {
        let n = printable(name);
        if set(volume.get("external"), true) {
            problems.push(format!("volume {n} is external"));
            continue;
        }
        if let Some(driver) = volume.get("driver").and_then(Value::as_str)
            && driver != "local"
        {
            problems.push(format!("volume {n} uses the {} driver", printable(driver)));
        }
        if set(volume.get("driver_opts"), false) {
            problems.push(format!("volume {n} sets driver_opts"));
        }
        // Compose names every volume `<project>_<key>`; any other name could alias a volume
        // of the reviewer's, which `down -v` would delete.
        if let Some(own) = volume.get("name").and_then(Value::as_str)
            && own != format!("{project}_{name}")
        {
            problems.push(format!("volume {n} sets its own name {}", printable(own)));
        }
    }
    for (kind, key) in [("secret", "secrets"), ("config", "configs")] {
        for (name, entry) in sorted(section(merged, key)) {
            if set(entry.get("file"), false) {
                problems.push(format!("{kind} {} reads a host file", printable(name)));
            }
        }
    }
    for name in names {
        if !services.contains_key(name) {
            problems.push(format!(
                "service {} is not among the compose file's services (a profiled service is not started)",
                printable(name)
            ));
        }
    }
    problems
}

fn build_refusals(s: &str, build: &Map<String, Value>, wt: Option<&Path>, out: &mut Vec<String>) {
    if let Some(context) = build.get("context").and_then(Value::as_str) {
        if context.contains("://") || context.starts_with("git@") {
            out.push(format!("service {s} builds from a remote context"));
        } else if let Some(wt) = wt {
            let path = Path::new(context);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                wt.join(path)
            };
            if !inside(wt, &path.to_string_lossy()) {
                out.push(format!(
                    "service {s} builds from {}, outside the worktree",
                    printable(context)
                ));
            }
        }
    }
    if set(build.get("ssh"), false) {
        out.push(format!("service {s} forwards SSH into its build"));
    }
    if set(build.get("additional_contexts"), false) {
        out.push(format!("service {s} uses additional build contexts"));
    }
    if set(build.get("secrets"), false) {
        out.push(format!("service {s} uses build secrets"));
    }
    for key in ["network", "privileged", "entitlements"] {
        if set(build.get(key), key == "privileged") {
            out.push(format!(
                "service {s} sets build.{key}, which Clúsia refuses"
            ));
        }
    }
}

/// Whether a key counts as set: present and not `null` (and `true`, for a flag).
fn set(value: Option<&Value>, flag: bool) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b || !flag,
        Some(_) => !flag,
    }
}

fn names_host_gateway(value: Option<&Value>) -> bool {
    let named = |v: &Value| match v {
        Value::String(s) => s.contains("host-gateway"),
        Value::Array(list) => list
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.contains("host-gateway"))),
        _ => false,
    };
    match value {
        Some(Value::Array(list)) => list.iter().any(named),
        Some(Value::Object(map)) => map.values().any(named),
        _ => false,
    }
}

/// `env_file` entries are paths or objects with a `path`.
fn env_files(value: Option<&Value>) -> Vec<String> {
    let entry = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("path").and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    match value {
        Some(Value::Array(list)) => list.iter().filter_map(entry).collect(),
        Some(other) => entry(other).into_iter().collect(),
        None => vec![],
    }
}

/// Whether `path` resolves inside `wt` (already resolved). A path that is not there is
/// resolved through its parent, else taken as written with its `..` applied.
fn inside(wt: &Path, path: &str) -> bool {
    resolved(Path::new(path)).starts_with(wt)
}

fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(real) = parent.canonicalize()
    {
        return real.join(name);
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn section<'a>(merged: &'a Value, key: &str) -> &'a Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    merged
        .get(key)
        .and_then(Value::as_object)
        .unwrap_or_else(|| EMPTY.get_or_init(Map::new))
}

/// A map's entries in name order, whatever order `serde_json` keeps.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

fn q(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

/// The override file: the project name pinned, no ports, no binds, every service on one
/// internal network, and the stack's labels on everything Compose creates. It is YAML written
/// as text, every scalar a JSON string.
pub fn compose_override(merged: &Value, project: &str, labels: &Labels) -> Result<String, String> {
    let problems = refusals(merged, None, &[]);
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    let stack = Labels {
        kind: LabelKind::Stack,
        ..labels.clone()
    };
    let l = format!(
        "{{{}}}",
        stack
            .pairs()
            .iter()
            .map(|(k, v)| format!("{}: {}", q(k), q(v)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut out = format!("name: {}\n", q(project));
    let services = sorted(section(merged, "services"));
    out.push_str(if services.is_empty() {
        "services: {}\n"
    } else {
        "services:\n"
    });
    for (name, service) in services {
        let volumes: Vec<String> = service
            .get("volumes")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(kept_volume).collect())
            .unwrap_or_default();
        out.push_str(&format!("  {}:\n", q(name)));
        out.push_str("    ports: !reset []\n");
        out.push_str(&format!(
            "    volumes: !override [{}]\n",
            volumes.join(", ")
        ));
        out.push_str(&format!("    networks: !override [{}]\n", q(INTERNAL_NET)));
        out.push_str(&format!("    labels: {l}\n"));
        if service.get("build").is_some_and(|b| !b.is_null()) {
            out.push_str(&format!("    build: {{\"labels\": {l}}}\n"));
        }
    }
    out.push_str("networks:\n");
    out.push_str(&format!(
        "  {}: {{\"internal\": true, \"labels\": {l}}}\n",
        q(INTERNAL_NET)
    ));
    out.push_str("  default: !reset null\n");
    let volumes = sorted(section(merged, "volumes"));
    if !volumes.is_empty() {
        out.push_str("volumes:\n");
        for (name, _) in volumes {
            out.push_str(&format!("  {}: {{\"labels\": {l}}}\n", q(name)));
        }
    }
    Ok(out)
}

/// A named, anonymous or tmpfs volume entry, rebuilt in a fixed key order; a bind mount or
/// any other type is dropped.
fn kept_volume(entry: &Value) -> Option<String> {
    let target = entry.get("target").and_then(Value::as_str)?;
    match entry.get("type").and_then(Value::as_str)? {
        "volume" => Some(match entry.get("source").and_then(Value::as_str) {
            Some(source) if !source.is_empty() => {
                let read_only = if entry.get("read_only") == Some(&Value::Bool(true)) {
                    ", \"read_only\": true"
                } else {
                    ""
                };
                format!(
                    "{{\"type\": \"volume\", \"source\": {}, \"target\": {}{read_only}}}",
                    q(source),
                    q(target)
                )
            }
            _ => format!("{{\"type\": \"volume\", \"target\": {}}}", q(target)),
        }),
        "tmpfs" => Some(format!(
            "{{\"type\": \"tmpfs\", \"target\": {}}}",
            q(target)
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::names::LabelKind;

    use super::*;

    fn labels() -> Labels {
        Labels {
            review: "acme~widgets~7".into(),
            repo: "acme/widgets".into(),
            kind: LabelKind::Run,
        }
    }

    #[test]
    fn compose_needs_2_24_4_or_later() {
        for ok in ["2.24.4", "v2.24.4", "2.29.1-desktop.1", "5.4.0", "2.25"] {
            assert!(compose_version_ok(ok), "{ok}");
        }
        for old in ["2.24.3", "v2.20.0", "1.29.2", "", "dev", "v"] {
            assert!(!compose_version_ok(old), "{old}");
        }
        assert_eq!(
            internal_network("clusia-acme-widgets-abc123-7"),
            "clusia-acme-widgets-abc123-7_clusia_internal"
        );
    }

    #[test]
    fn the_override_drops_binds_ports_and_networks() {
        let merged = json!({
            "name": "widgets",
            "services": {
                "web": {
                    "build": {"context": "/w", "dockerfile": "Dockerfile"},
                    "ports": [{"target": 8080, "published": "8080", "protocol": "tcp"}],
                    "volumes": [{"type": "bind", "source": "/w", "target": "/app"}],
                    "networks": {"default": null}
                },
                "cache": {
                    "image": "redis:alpine",
                    "ports": [{"target": 6379, "published": "16379", "protocol": "tcp"}],
                    "volumes": [
                        {"type": "bind", "source": "/Users/octo/data", "target": "/data"},
                        {"type": "volume", "source": "redisstate", "target": "/state"},
                        {"type": "volume", "source": "seed", "target": "/seed", "read_only": true},
                        {"type": "volume", "target": "/anon"},
                        {"type": "tmpfs", "target": "/tmp"}
                    ],
                    "networks": {"default": null}
                }
            },
            "networks": {"default": {"name": "widgets_default"}},
            "volumes": {"redisstate": {"name": "widgets_redisstate"}, "seed": {"name": "widgets_seed"}}
        });
        let l = r#"{"io.clusia.review": "acme~widgets~7", "io.clusia.repo": "acme/widgets", "io.clusia.kind": "stack"}"#;
        let expected = format!(
            r#"name: "clusia-acme-widgets-abc123-7"
services:
  "cache":
    ports: !reset []
    volumes: !override [{{"type": "volume", "source": "redisstate", "target": "/state"}}, {{"type": "volume", "source": "seed", "target": "/seed", "read_only": true}}, {{"type": "volume", "target": "/anon"}}, {{"type": "tmpfs", "target": "/tmp"}}]
    networks: !override ["clusia_internal"]
    labels: {l}
  "web":
    ports: !reset []
    volumes: !override []
    networks: !override ["clusia_internal"]
    labels: {l}
    build: {{"labels": {l}}}
networks:
  "clusia_internal": {{"internal": true, "labels": {l}}}
  default: !reset null
volumes:
  "redisstate": {{"labels": {l}}}
  "seed": {{"labels": {l}}}
"#
        );
        assert_eq!(
            compose_override(&merged, "clusia-acme-widgets-abc123-7", &labels()).unwrap(),
            expected
        );
    }

    #[test]
    fn compose_features_that_reach_the_host_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        let wt = top.join("wt");
        let outside = top.join("outside");
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let merged = json!({
            "services": {
                "a": {"image": "x", "network_mode": "host", "privileged": true, "cap_add": ["SYS_ADMIN"]},
                "b": {"image": "x", "privileged": false, "devices": ["/dev/kvm"], "pid": "host",
                      "ipc": "host", "volumes_from": ["a"], "security_opt": ["seccomp=unconfined"],
                      "userns_mode": "host", "cgroup_parent": "x"},
                "c": {"build": {"context": outside, "ssh": ["default"],
                                "additional_contexts": {"x": "/"}, "secrets": ["s"]}},
                "d": {"build": {"context": "https://github.com/acme/widgets.git"}},
                "e": {"build": {"context": wt.join("sub")},
                      "env_file": [{"path": outside.join(".env"), "required": true}, wt.join(".env")]},
                "f": {"image": "x", "container_name": "pg",
                      "extra_hosts": ["host.docker.internal:host-gateway", "db.local:10.0.0.5"]},
                "g": {"build": {"context": wt.clone(), "network": "host", "privileged": true,
                                "entitlements": ["network.host"]}}
            },
            "name": "p",
            "networks": {"ext": {"external": true}, "ok": {}},
            "volumes": {
                "alias": {"name": "octo_pgdata"},
                "bound": {"name": "p_bound", "driver_opts": {"type": "none", "o": "bind", "device": "/Users/octo"}},
                "data": {"name": "data", "external": true},
                "fine": {"name": "p_fine"},
                "nfs": {"name": "p_nfs", "driver": "nfs-driver"}
            },
            "secrets": {"s": {"file": "/Users/octo/.ssh/id_test"}, "e": {"environment": "X"}},
            "configs": {"c": {"file": "/w/c.conf"}}
        });
        let o = outside.display();
        assert_eq!(
            compose_problems(&merged, &wt, &["a".to_string(), "missing".to_string()]),
            [
                "service a sets network_mode, which Clúsia refuses".to_string(),
                "service a sets privileged, which Clúsia refuses".into(),
                "service a sets cap_add, which Clúsia refuses".into(),
                "service b sets devices, which Clúsia refuses".into(),
                "service b sets pid, which Clúsia refuses".into(),
                "service b sets ipc, which Clúsia refuses".into(),
                "service b sets volumes_from, which Clúsia refuses".into(),
                "service b sets security_opt, which Clúsia refuses".into(),
                "service b sets userns_mode, which Clúsia refuses".into(),
                "service b sets cgroup_parent, which Clúsia refuses".into(),
                format!("service c builds from {o}, outside the worktree"),
                "service c forwards SSH into its build".into(),
                "service c uses additional build contexts".into(),
                "service c uses build secrets".into(),
                "service d builds from a remote context".into(),
                format!("service e reads an env_file outside the worktree: {o}/.env"),
                "service f sets container_name, which Clúsia refuses".into(),
                "service f maps a host to host-gateway".into(),
                "service g sets build.network, which Clúsia refuses".into(),
                "service g sets build.privileged, which Clúsia refuses".into(),
                "service g sets build.entitlements, which Clúsia refuses".into(),
                "network ext is external".into(),
                "volume alias sets its own name octo_pgdata".into(),
                "volume bound sets driver_opts".into(),
                "volume data is external".into(),
                "volume nfs uses the nfs-driver driver".into(),
                "secret s reads a host file".into(),
                "config c reads a host file".into(),
                "service missing is not among the compose file's services (a profiled service is not started)".into(),
            ]
        );
        let err = compose_override(&merged, "p", &labels()).unwrap_err();
        assert!(
            err.starts_with("service a sets network_mode, which Clúsia refuses; "),
            "{err}"
        );
        let fine = json!({
            "name": "p",
            "services": {"db": {"image": "postgres:16", "extra_hosts": {"db.local": "10.0.0.5"}}},
            "volumes": {"pgdata": {"name": "p_pgdata", "driver": "local"}}
        });
        assert!(compose_problems(&fine, &wt, &["db".to_string()]).is_empty());
    }
}
