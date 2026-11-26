//! Core library of `metis`.
//!
//! The binary (`src/main.rs`) is a thin CLI shell over this library: keep
//! logic here so it stays testable without spawning a process.

pub mod client;
mod error;
pub mod headless;
pub mod journal;
pub mod runtime;
pub mod session_transport;

pub use error::{Error, Result};
