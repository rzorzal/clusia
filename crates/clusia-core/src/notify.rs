//! What a notification is about and where a click on it goes.

use serde::{Deserialize, Serialize};

use crate::PrRef;

/// Where the window goes when a notification or an inbox row is opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenTarget {
    /// The review, optionally at one thread.
    Review {
        pr: PrRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    /// Home, optionally with one pull request in view.
    Home {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr: Option<PrRef>,
    },
    /// A Config page, by its name (`git`, `notifications`).
    Config { page: String },
}
