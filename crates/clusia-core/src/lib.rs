//! Clúsia domain types and pure logic. No I/O beyond reading environment variables.

pub mod config;
pub mod paths;
pub mod pr;

pub use config::Config;
pub use paths::{Paths, PathsError};
pub use pr::{PrDetail, PrFilter, PrRef, PrRefError, PrSummary};
