//! The library's public error boundary.
//!
//! Decision rule: callers that need to *react* to a failure mode get a
//! `thiserror` enum variant; callers that only give up and report get an
//! opaque `miette::Report` (see the top level in `main.rs`).

use miette::Diagnostic;
use thiserror::Error;

/// All failure modes of the `metis` library.
///
/// Errors are user-facing documentation: `code` is searchable and `help`
/// gives an actionable fix. Once published, an error code is part of the
/// public interface — removing or renaming one is a `SemVer`-breaking change.
#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    /// The headless runtime entry point is not implemented yet.
    #[error("Metis headless runtime is not implemented yet")]
    #[diagnostic(
        code(metis::runtime_unavailable),
        help(
            "use a build with the default `workbench` feature; the headless runtime is not implemented yet"
        )
    )]
    RuntimeUnavailable,
    /// The workbench was started without a graphical session.
    #[error("no graphical session detected")]
    #[diagnostic(
        code(metis::display_unavailable),
        help("start Metis from a Wayland or X11 desktop session")
    )]
    DisplayUnavailable,
    /// GPUI failed while connecting to the graphical session.
    #[error("failed to start the Metis workbench: {0}")]
    #[diagnostic(
        code(metis::workbench_startup),
        help(
            "verify that the Wayland or X11 session is reachable and its runtime libraries are installed"
        )
    )]
    WorkbenchStartup(String),
}

/// Convenience alias for library results.
pub type Result<T> = std::result::Result<T, Error>;
