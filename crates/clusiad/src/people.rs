//! `GetProfile`: the public fields of a GitHub user, for the hover card.

use clusia_protocol::{Outcome, Reply};

use crate::handlers::{no_token, provider_error};
use crate::state::Shared;
use crate::sync::github_client;

pub(crate) async fn profile(shared: &Shared, login: &str) -> Outcome {
    let gh = match github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return no_token(),
        Err(e) => return provider_error(e),
    };
    match gh.get_profile(login).await {
        Ok(p) => Outcome::Ok(Reply::Profile(p)),
        Err(e) => provider_error(e),
    }
}
