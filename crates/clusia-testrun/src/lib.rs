//! Everything that decides or spawns for a pull request's test runs. It never talks to the
//! daemon's socket and starts programs only through [`exec::Exec`].

pub mod engine;
pub mod exec;
pub mod log;
pub mod names;
pub mod scan;
#[cfg(any(test, feature = "test-kit"))]
pub mod testkit;
