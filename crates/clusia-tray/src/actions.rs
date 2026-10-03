//! What a click does.

use clusia_core::PrRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open the review in the window, or the pull request in the browser without the window.
    OpenReview {
        pr: PrRef,
        url: String,
    },
    OpenUrl(String),
    OpenHome,
    OpenConfig,
}
