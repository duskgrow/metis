//! Core library of `metis`.
//!
//! The binary (`src/main.rs`) is a thin CLI shell over this library: keep
//! logic here so it stays testable without spawning a process.

mod error;
pub mod runtime;

pub use error::{Error, Result};
