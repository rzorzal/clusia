//! Everything that decides or spawns for a pull request's test runs. It never talks to the
//! daemon's socket and starts programs only through [`exec::Exec`].

pub mod cleanup;
pub mod compose;
pub mod container;
pub mod docker;
pub mod eco;
pub mod engine;
pub mod exec;
pub mod image;
pub mod local;
pub mod log;
pub mod names;
pub mod plan;
pub mod recipe;
pub mod sandbox;
pub mod scan;
#[cfg(any(test, feature = "test-kit"))]
pub mod testkit;
