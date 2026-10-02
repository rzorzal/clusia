//! How the daemon reaches GitHub and stores secrets. Tests replace every outside dependency.

use std::path::PathBuf;
use std::sync::Arc;

use clusia_platform::{Keychain, MemoryStore, SecretStore};

pub struct DaemonOptions {
    /// GitHub API base URL override. Env: `CLUSIA_GITHUB_API`.
    pub github_api: Option<String>,
    /// Token that wins over gh and the Keychain. Env: `CLUSIA_GITHUB_TOKEN`.
    pub github_token: Option<String>,
    /// The `gh` executable. Env: `CLUSIA_GH_BIN`.
    pub gh_program: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    /// Run the periodic GitHub sync.
    pub background_sync: bool,
}

impl DaemonOptions {
    pub fn from_env() -> Self {
        let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let secrets: Arc<dyn SecretStore> =
            if var("CLUSIA_SECRET_STORE").as_deref() == Some("memory") {
                Arc::new(MemoryStore::default())
            } else {
                Arc::new(Keychain::new(Keychain::GITHUB_SERVICE))
            };
        Self {
            github_api: var("CLUSIA_GITHUB_API"),
            github_token: var("CLUSIA_GITHUB_TOKEN"),
            gh_program: var("CLUSIA_GH_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("gh")),
            secrets,
            background_sync: true,
        }
    }
}
