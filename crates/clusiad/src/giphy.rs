//! Giphy for the GIF picker. The API key lives in the secret store under the account `giphy`;
//! the window never holds it.

use clusia_protocol::{ErrorCode, GiphyKeyStatus, Outcome, ProtocolError, Reply};

use crate::state::Shared;

const KEY_ACCOUNT: &str = "giphy";

/// Whether a key is stored. The key itself is never part of an answer.
pub(crate) fn key_status(shared: &Shared) -> Outcome {
    match shared.secrets.get(KEY_ACCOUNT) {
        Ok(found) => Outcome::Ok(Reply::GiphyKeyStatus(GiphyKeyStatus {
            configured: found.is_some_and(|k| !k.trim().is_empty()),
        })),
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not read the key: {e}"),
        )),
    }
}
