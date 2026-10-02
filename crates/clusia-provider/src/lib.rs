//! Talking to the git server: authentication and the GitHub API (spec §5.1).

pub mod auth;

pub use auth::{AuthError, Token, TokenOrigin, TokenSources, gh_token, resolve_token};
