//! Where each ecosystem's tools keep their caches, so a run writes them to Clúsia's per-repo
//! cache instead of the home folder or the worktree.

use clusia_core::tests::Ecosystem;

/// Every key `eco_env` can set, in the order of its table.
pub const ECO_KEYS: [&str; 15] = [
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "npm_config_cache",
    "YARN_CACHE_FOLDER",
    "npm_config_store_dir",
    "PIP_CACHE_DIR",
    "PYTHONPYCACHEPREFIX",
    "XDG_CACHE_HOME",
    "GOCACHE",
    "GOMODCACHE",
    "GOPATH",
    "GRADLE_USER_HOME",
    "MAVEN_OPTS",
    "BUNDLE_PATH",
    "GEM_HOME",
];

pub fn name(eco: Ecosystem) -> &'static str {
    match eco {
        Ecosystem::Cargo => "cargo",
        Ecosystem::Node => "node",
        Ecosystem::Python => "python",
        Ecosystem::Go => "go",
        Ecosystem::Jvm => "jvm",
        Ecosystem::Ruby => "ruby",
    }
}

/// The cache variables of `eco`, each under `<cache_root>/<name>/`.
pub fn eco_env(eco: Ecosystem, cache_root: &str) -> Vec<(String, String)> {
    let dir = format!("{cache_root}/{}", name(eco));
    let rows: &[(&str, &str, &str)] = match eco {
        Ecosystem::Cargo => &[
            ("CARGO_HOME", "", "home"),
            ("CARGO_TARGET_DIR", "", "target"),
        ],
        Ecosystem::Node => &[
            ("npm_config_cache", "", "npm"),
            ("YARN_CACHE_FOLDER", "", "yarn"),
            ("npm_config_store_dir", "", "pnpm-store"),
        ],
        Ecosystem::Python => &[
            ("PIP_CACHE_DIR", "", "pip"),
            ("PYTHONPYCACHEPREFIX", "", "pycache"),
            ("XDG_CACHE_HOME", "", "xdg"),
        ],
        Ecosystem::Go => &[
            ("GOCACHE", "", "build"),
            ("GOMODCACHE", "", "mod"),
            ("GOPATH", "", "path"),
        ],
        Ecosystem::Jvm => &[
            ("GRADLE_USER_HOME", "", "gradle"),
            ("MAVEN_OPTS", "-Dmaven.repo.local=", "m2"),
        ],
        Ecosystem::Ruby => &[("BUNDLE_PATH", "", "bundle"), ("GEM_HOME", "", "gems")],
    };
    rows.iter()
        .map(|(key, prefix, sub)| (key.to_string(), format!("{prefix}{dir}/{sub}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(eco: Ecosystem) -> Vec<(&'static str, String)> {
        let env = eco_env(eco, "/cache");
        let keys: Vec<&'static str> = env
            .iter()
            .map(|(k, _)| {
                *ECO_KEYS
                    .iter()
                    .find(|key| **key == k.as_str())
                    .expect("a listed key")
            })
            .collect();
        keys.into_iter()
            .zip(env.into_iter().map(|(_, v)| v))
            .collect()
    }

    #[test]
    fn every_ecosystem_keeps_its_caches_under_its_own_folder() {
        assert_eq!(
            pairs(Ecosystem::Cargo),
            [
                ("CARGO_HOME", "/cache/cargo/home".to_string()),
                ("CARGO_TARGET_DIR", "/cache/cargo/target".to_string()),
            ]
        );
        assert_eq!(
            pairs(Ecosystem::Node),
            [
                ("npm_config_cache", "/cache/node/npm".to_string()),
                ("YARN_CACHE_FOLDER", "/cache/node/yarn".to_string()),
                ("npm_config_store_dir", "/cache/node/pnpm-store".to_string()),
            ]
        );
        assert_eq!(
            pairs(Ecosystem::Python),
            [
                ("PIP_CACHE_DIR", "/cache/python/pip".to_string()),
                ("PYTHONPYCACHEPREFIX", "/cache/python/pycache".to_string()),
                ("XDG_CACHE_HOME", "/cache/python/xdg".to_string()),
            ]
        );
        assert_eq!(
            pairs(Ecosystem::Go),
            [
                ("GOCACHE", "/cache/go/build".to_string()),
                ("GOMODCACHE", "/cache/go/mod".to_string()),
                ("GOPATH", "/cache/go/path".to_string()),
            ]
        );
        assert_eq!(
            pairs(Ecosystem::Jvm),
            [
                ("GRADLE_USER_HOME", "/cache/jvm/gradle".to_string()),
                ("MAVEN_OPTS", "-Dmaven.repo.local=/cache/jvm/m2".to_string()),
            ]
        );
        assert_eq!(
            pairs(Ecosystem::Ruby),
            [
                ("BUNDLE_PATH", "/cache/ruby/bundle".to_string()),
                ("GEM_HOME", "/cache/ruby/gems".to_string()),
            ]
        );
    }

    #[test]
    fn the_key_list_is_every_key_the_table_sets() {
        let all = [
            Ecosystem::Cargo,
            Ecosystem::Node,
            Ecosystem::Python,
            Ecosystem::Go,
            Ecosystem::Jvm,
            Ecosystem::Ruby,
        ];
        let keys: Vec<String> = all
            .iter()
            .flat_map(|e| eco_env(*e, "/r"))
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ECO_KEYS.map(String::from));
    }

    #[test]
    fn a_recipe_can_set_no_cache_key() {
        for key in ECO_KEYS {
            assert!(crate::recipe::env_is_forbidden(key), "{key}");
        }
        for key in ["LANG", "TERM", "CI", "RUST_BACKTRACE"] {
            assert!(
                !crate::recipe::env_is_forbidden(key),
                "{key} stays the recipe's to set"
            );
        }
    }

    #[test]
    fn names_match_the_cache_volume_words() {
        assert_eq!(
            [
                Ecosystem::Cargo,
                Ecosystem::Node,
                Ecosystem::Python,
                Ecosystem::Go,
                Ecosystem::Jvm,
                Ecosystem::Ruby,
            ]
            .map(name),
            ["cargo", "node", "python", "go", "jvm", "ruby"]
        );
        let slug = crate::names::repo_slug("acme", "widgets");
        assert_eq!(
            crate::names::cache_volume(&slug, Ecosystem::Python),
            format!("clusia-cache-{slug}-{}", name(Ecosystem::Python))
        );
    }
}
