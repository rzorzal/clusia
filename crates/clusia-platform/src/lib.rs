//! macOS-specific pieces behind traits, so other platforms can be added later (spec §3.2).

pub mod secrets;

pub use secrets::{Keychain, MemoryStore, SecretError, SecretStore};
