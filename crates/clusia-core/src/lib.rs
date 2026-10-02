//! Clúsia domain types and pure logic. No I/O beyond reading environment variables.

pub mod config;
pub mod paths;

pub use config::Config;
pub use paths::{Paths, PathsError};
