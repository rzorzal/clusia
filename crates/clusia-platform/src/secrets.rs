//! Secret storage. The GitHub token lives here, never in Clúsia's files.

use std::collections::HashMap;
use std::sync::Mutex;

use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

/// `errSecItemNotFound`.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("keychain error: {0}")]
    Keychain(String),
    #[error("the stored secret is not valid UTF-8")]
    NotUtf8,
}

pub trait SecretStore: Send + Sync + 'static {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError>;
    /// `true` if something was deleted.
    fn delete(&self, account: &str) -> Result<bool, SecretError>;
}

/// Generic passwords in the user's login keychain, under one service name.
pub struct Keychain {
    service: String,
}

impl Keychain {
    pub const GITHUB_SERVICE: &'static str = "dev.clusia.github";

    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

impl SecretStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        match get_generic_password(&self.service, account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SecretError::NotUtf8),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        set_generic_password(&self.service, account, secret.as_bytes())
            .map_err(|e| SecretError::Keychain(e.to_string()))
    }

    fn delete(&self, account: &str) -> Result<bool, SecretError> {
        match delete_generic_password(&self.service, account) {
            Ok(()) => Ok(true),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }
}

/// In-process store for tests and `CLUSIA_SECRET_STORE=memory`.
#[derive(Default)]
pub struct MemoryStore {
    items: Mutex<HashMap<String, String>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        Ok(self
            .items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(account)
            .cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        self.items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<bool, SecretError> {
        Ok(self
            .items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(account)
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exercise(store: &dyn SecretStore, account: &str) {
        assert_eq!(store.get(account).unwrap(), None);
        assert!(!store.delete(account).unwrap());
        store.set(account, "s3cret").unwrap();
        assert_eq!(store.get(account).unwrap().as_deref(), Some("s3cret"));
        store.set(account, "rotated").unwrap();
        assert_eq!(store.get(account).unwrap().as_deref(), Some("rotated"));
        assert!(store.delete(account).unwrap());
        assert_eq!(store.get(account).unwrap(), None);
    }

    #[test]
    fn memory_store_behaves_like_a_secret_store() {
        exercise(&MemoryStore::default(), "github.com");
    }

    #[test]
    fn memory_store_keeps_accounts_separate() {
        let s = MemoryStore::default();
        s.set("github.com", "a").unwrap();
        s.set("ghe.example.com", "b").unwrap();
        assert_eq!(s.get("github.com").unwrap().as_deref(), Some("a"));
        assert_eq!(s.get("ghe.example.com").unwrap().as_deref(), Some("b"));
    }

    #[test]
    #[ignore = "touches the login keychain; run manually with --ignored"]
    fn keychain_round_trip() {
        let k = Keychain::new("dev.clusia.test");
        let account = format!("test-{}", std::process::id());
        let _ = k.delete(&account);
        exercise(&k, &account);
    }
}
