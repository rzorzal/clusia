//! Talking to the git server: authentication and the GitHub API (spec §5.1).

pub mod auth;
pub mod github;

pub use auth::{AuthError, Token, TokenOrigin, TokenSources, gh_token, resolve_token};
pub use github::{GitHub, ProviderError, PublishedReview, Viewer, api_base_for_host, search_query};
