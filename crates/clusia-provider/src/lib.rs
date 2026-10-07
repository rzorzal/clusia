//! Talking to the git server: authentication and the GitHub API (spec §5.1).

pub mod auth;
pub mod github;
pub mod graphql;
pub mod notifications;

pub use auth::{AuthError, Token, TokenOrigin, TokenSources, gh_token, resolve_token};
pub use github::{GitHub, ProviderError, PublishedReview, Viewer, api_base_for_host, search_query};
pub use graphql::{PendingReview, PrNode, graphql_url};
pub use notifications::{CheckReport, CheckState, Notification};
