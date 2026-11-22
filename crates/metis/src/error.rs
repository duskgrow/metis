//! The library's public error boundary.
//!
//! Decision rule: callers that need to *react* to a failure mode get a
//! `thiserror` enum variant; callers that only give up and report get an
//! opaque `miette::Report` (see the top level in `main.rs`).

use std::{io, path::PathBuf, process::ExitStatus, time::Duration};

use miette::Diagnostic;
use thiserror::Error;

/// All failure modes of the `metis` library.
///
/// Errors are user-facing documentation: `code` is searchable and `help`
/// gives an actionable fix. Once published, an error code is part of the
/// public interface — removing or renaming one is a `SemVer`-breaking change.
#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    /// A headless build was started without selecting its Host entry point.
    #[error("no entry point selected for the Metis headless build")]
    #[diagnostic(
        code(metis::runtime_unavailable),
        help("run `metis host --home <PATH>` to start a persistent Host")
    )]
    RuntimeUnavailable,
    /// The installed executable location could not be determined.
    #[error("failed to locate the Metis installation: {0}")]
    #[diagnostic(
        code(metis::runtime_locate),
        help("reinstall the complete Metis package in its standard bin/lib layout")
    )]
    RuntimeLocate(#[source] io::Error),
    /// The Host connection file could not be replaced or retired.
    #[error("failed to update Host connection file `{}`: {source}", .path.display())]
    #[diagnostic(
        code(metis::runtime_connection),
        help("check that the Host state directory is writable by the current user")
    )]
    RuntimeConnection {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Another persistent Host already owns the selected state directory.
    #[error("another Metis Host already uses `{}`", .home.display())]
    #[diagnostic(
        code(metis::runtime_already_running),
        help("stop the existing Host or choose a different --home directory")
    )]
    RuntimeAlreadyRunning { home: PathBuf },
    /// The asynchronous executor could not be created.
    #[error("failed to create the Host executor: {0}")]
    #[diagnostic(code(metis::runtime_executor))]
    RuntimeExecutor(#[source] io::Error),
    /// Listening for the headless process shutdown signal failed.
    #[error("failed to listen for Host shutdown: {0}")]
    #[diagnostic(code(metis::runtime_signal))]
    RuntimeSignal(#[source] io::Error),
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
    /// The per-runtime directory could not be created.
    #[error("failed to prepare dsh runtime at `{}`: {source}", .path.display())]
    #[diagnostic(
        code(metis::runtime_prepare),
        help("check that the runtime directory is writable and has free space")
    )]
    RuntimePrepare {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The watchdog or Node.js process could not be spawned.
    #[error("failed to spawn `{}`: {source}", .program.display())]
    #[diagnostic(
        code(metis::runtime_spawn),
        help("verify that the packaged Metis, Node.js, and dsh files are installed together")
    )]
    RuntimeSpawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A requested output pipe or forwarding thread could not be created.
    #[error("failed to capture or forward dsh output")]
    #[diagnostic(code(metis::runtime_output_capture))]
    RuntimeOutputCapture,
    /// Waiting on a supervised process failed.
    #[error("failed while waiting for a dsh runtime process: {0}")]
    #[diagnostic(code(metis::runtime_wait))]
    RuntimeWait(#[source] io::Error),
    /// Sending the application-level shutdown request failed.
    #[error("failed to request dsh Host shutdown: {0}")]
    #[diagnostic(code(metis::runtime_shutdown_request))]
    RuntimeShutdownRequest(#[source] io::Error),
    /// Sending the final termination signal failed.
    #[error("failed to terminate the dsh runtime process tree: {0}")]
    #[diagnostic(code(metis::runtime_kill))]
    RuntimeKill(#[source] io::Error),
    /// Detached dsh processes could not be collected after Host cleanup.
    #[error("failed to clean up detached dsh processes: {0}")]
    #[diagnostic(
        code(metis::runtime_scope_cleanup),
        help("inspect `systemctl --user list-units` for leftover dsh scopes and stop them")
    )]
    RuntimeScopeCleanup(String),
    /// dsh exited before its listener became ready.
    #[error("dsh exited before its listener became ready ({status}); output:\n{output}")]
    #[diagnostic(
        code(metis::runtime_startup_exit),
        help(
            "inspect the captured output and verify that the pinned Node.js version matches .node-version"
        )
    )]
    RuntimeStartupExit { status: String, output: String },
    /// The Host violated its private startup protocol.
    #[error("dsh Host startup protocol failed: {message}; output:\n{output}")]
    #[diagnostic(
        code(metis::runtime_startup_protocol),
        help("verify that the packaged Metis Host and dsh dependencies were installed together")
    )]
    RuntimeStartupProtocol { message: String, output: String },
    /// dsh stayed alive but did not become ready.
    #[error("dsh did not become ready within {timeout:?}; output:\n{output}")]
    #[diagnostic(
        code(metis::runtime_startup_timeout),
        help("inspect the captured output; the incomplete runtime was terminated")
    )]
    RuntimeStartupTimeout { timeout: Duration, output: String },
    /// The watchdog did not finish cleanup before the shutdown deadline.
    #[error("dsh watchdog cleanup exceeded {0:?}; termination was escalated")]
    #[diagnostic(code(metis::runtime_shutdown_timeout))]
    RuntimeShutdownTimeout(Duration),
    /// Reading the workbench-to-watchdog lifetime pipe failed.
    #[error("failed to read the workbench lifetime pipe: {0}")]
    #[diagnostic(code(metis::runtime_parent_pipe))]
    RuntimeParentPipe(#[source] io::Error),
    /// dsh exited unsuccessfully while its owner was still alive.
    #[error("dsh exited unexpectedly: {0}")]
    #[diagnostic(code(metis::runtime_exit))]
    RuntimeDshExit(ExitStatus),
}

/// Convenience alias for library results.
pub type Result<T> = std::result::Result<T, Error>;
