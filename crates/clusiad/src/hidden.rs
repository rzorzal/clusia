//! The comments a reviewer hid, per pull request (`hidden/<pr>.json`). A file of its own, not
//! the review's: closing a review with an empty draft deletes the review. It stays on this
//! machine.

use clusia_core::PrRef;
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, Reply};
use clusia_store::{load_hidden, save_hidden};

use crate::state::Shared;

fn internal(what: &str, e: &std::io::Error) -> Outcome {
    Outcome::Err(ProtocolError::new(
        ErrorCode::Internal,
        format!("cannot {what} the hidden comments: {e}"),
    ))
}

pub(crate) async fn get(shared: &Shared, pr: &PrRef) -> Outcome {
    let _serialized = shared.hidden_lock.lock().await;
    match load_hidden(&shared.paths, pr) {
        Ok(set) => Outcome::Ok(Reply::HiddenComments(set)),
        Err(e) => internal("read", &e),
    }
}

/// Hides `key` (it holds `posts` posts now) or, with no `posts`, shows it again.
pub(crate) async fn set(shared: &Shared, pr: &PrRef, key: &str, posts: Option<u32>) -> Outcome {
    let _serialized = shared.hidden_lock.lock().await;
    let mut set = match load_hidden(&shared.paths, pr) {
        Ok(set) => set,
        Err(e) => return internal("read", &e),
    };
    match posts {
        Some(posts) => set.hide(key, posts),
        None => {
            set.show(key);
        }
    }
    match save_hidden(&shared.paths, pr, &set) {
        Ok(()) => Outcome::Ok(Reply::HiddenComments(set)),
        Err(e) => internal("save", &e),
    }
}

/// Shows every comment in `keys` again, in one write.
pub(crate) async fn show_all(shared: &Shared, pr: &PrRef, keys: &[String]) -> Outcome {
    let _serialized = shared.hidden_lock.lock().await;
    let mut set = match load_hidden(&shared.paths, pr) {
        Ok(set) => set,
        Err(e) => return internal("read", &e),
    };
    for key in keys {
        set.show(key);
    }
    match save_hidden(&shared.paths, pr, &set) {
        Ok(()) => Outcome::Ok(Reply::HiddenComments(set)),
        Err(e) => internal("save", &e),
    }
}
