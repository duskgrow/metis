//! Command-line shell for `metis`.
//!
//! `--help` and `--version` are generated from the clap definitions.

use clap::{Args, Parser, Subcommand};
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};
#[cfg(feature = "workbench")]
use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    #[command(
        name = "host",
        about = "Run a persistent Host without opening the workbench"
    )]
    Host(HostArgs),
    #[command(name = "__dsh-watchdog", hide = true)]
    DshWatchdog(WatchdogArgs),
}

#[derive(Args)]
struct HostArgs {
    /// Directory for persistent Host state and the private connection file
    #[arg(long, value_name = "PATH")]
    home: PathBuf,
    /// Node.js executable; defaults to the packaged sibling executable
    #[arg(long, value_name = "PATH")]
    node: Option<PathBuf>,
    /// Metis Host entry point; defaults to the packaged lib/metis-host directory
    #[arg(long, value_name = "PATH")]
    host_entrypoint: Option<PathBuf>,
}

#[derive(Args)]
struct WatchdogArgs {
    #[arg(long)]
    node: PathBuf,
    #[arg(long)]
    host: PathBuf,
    #[arg(long)]
    home: PathBuf,
}

fn main() -> miette::Result<()> {
    init_tracing();
    match Cli::parse().command {
        Some(CliCommand::Host(args)) => run_host(args),
        Some(CliCommand::DshWatchdog(args)) => {
            metis::runtime::run_watchdog(&metis::runtime::WatchdogConfig {
                node_executable: args.node,
                host_entrypoint: args.host,
                home: args.home,
            })
            .map_err(Into::into)
        }
        None => run_entrypoint(),
    }
}

fn run_host(args: HostArgs) -> miette::Result<()> {
    let executable = std::env::current_exe().map_err(metis::Error::RuntimeLocate)?;
    let binary_directory = executable.parent().ok_or_else(installation_layout_error)?;
    let node_executable = args
        .node
        .unwrap_or_else(|| binary_directory.join(if cfg!(windows) { "node.exe" } else { "node" }));
    let host_entrypoint = match args.host_entrypoint {
        Some(path) => path,
        None => binary_directory
            .parent()
            .ok_or_else(installation_layout_error)?
            .join("lib")
            .join("metis-host")
            .join("lib")
            .join("host.js"),
    };
    let async_runtime = tokio::runtime::Runtime::new().map_err(metis::Error::RuntimeExecutor)?;
    let mut signals = {
        let _runtime = async_runtime.enter();
        shutdown_signals().map_err(metis::Error::RuntimeSignal)?
    };
    let running = metis::headless::Headless::start(
        &metis::runtime::DshSupervisor::system(),
        &metis::headless::HeadlessConfig {
            runtime: metis::runtime::DshRuntimeConfig {
                metis_executable: executable,
                node_executable,
                host_entrypoint,
                home: args.home,
                startup_timeout: Duration::from_secs(30),
                shutdown_timeout: Duration::from_secs(15),
            },
            poll_interval: Duration::from_millis(250),
        },
    )?;
    let mut output = io::stdout().lock();
    writeln!(output, "{}", running.connection_file().display())
        .and_then(|()| output.flush())
        .map_err(|_| metis::Error::RuntimeOutputCapture)?;
    drop(output);
    async_runtime
        .block_on(running.wait(shutdown_signal(&mut signals)))
        .map_err(Into::into)
}

fn installation_layout_error() -> metis::Error {
    metis::Error::RuntimeLocate(io::Error::new(
        io::ErrorKind::NotFound,
        "the executable has no installation root",
    ))
}

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(windows)]
struct ShutdownSignals {
    interrupt: tokio::signal::windows::CtrlC,
}

#[cfg(unix)]
fn shutdown_signals() -> io::Result<ShutdownSignals> {
    Ok(ShutdownSignals {
        interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
        terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
    })
}

#[cfg(windows)]
fn shutdown_signals() -> io::Result<ShutdownSignals> {
    Ok(ShutdownSignals {
        interrupt: tokio::signal::windows::ctrl_c()?,
    })
}

#[cfg(unix)]
async fn shutdown_signal(signals: &mut ShutdownSignals) -> io::Result<()> {
    tokio::select! {
        _ = signals.interrupt.recv() => Ok(()),
        _ = signals.terminate.recv() => Ok(()),
    }
}

#[cfg(windows)]
async fn shutdown_signal(signals: &mut ShutdownSignals) -> io::Result<()> {
    signals.interrupt.recv().await;
    Ok(())
}

#[cfg(feature = "workbench")]
fn run_entrypoint() -> miette::Result<()> {
    run_workbench()
}

#[cfg(not(feature = "workbench"))]
fn run_entrypoint() -> miette::Result<()> {
    Err(metis::Error::RuntimeUnavailable.into())
}

#[cfg(feature = "workbench")]
struct Workbench;

#[cfg(feature = "workbench")]
impl gpui_kit::Render for Workbench {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::{ParentElement as _, Styled as _};

        gpui_kit::div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child("Metis")
            .child("A place for your projects and agent sessions.")
    }
}

#[cfg(feature = "workbench")]
fn run_workbench() -> miette::Result<()> {
    use gpui_kit::{AppContext as _, assets, init, open_window};

    #[cfg(target_os = "linux")]
    let application = {
        use gpui_kit::{GraphicalEnvironment, WindowingModes};

        if GraphicalEnvironment::detect().guess_compositor() == "Headless" {
            return Err(metis::Error::DisplayUnavailable.into());
        }
        gpui_kit::platform::linux(WindowingModes::WAYLAND | WindowingModes::X11)
    };
    #[cfg(not(target_os = "linux"))]
    let application = gpui_kit::application();

    let launching = Arc::new(AtomicBool::new(true));
    let launch_state = Arc::clone(&launching);
    let launch_thread = thread::current().id();
    let previous_hook = Arc::new(std::panic::take_hook());
    let launch_hook_state = Arc::clone(&launching);
    let launch_hook_previous = Arc::clone(&previous_hook);
    std::panic::set_hook(Box::new(move |info| {
        if !launch_hook_state.load(Ordering::Acquire) || thread::current().id() != launch_thread {
            launch_hook_previous(info);
        }
    }));

    let result = catch_unwind(AssertUnwindSafe(move || {
        application
            .with_assets(assets::Assets)
            .with_quit_mode(gpui_kit::QuitMode::LastWindowClosed)
            .run(move |cx| {
                init(cx);
                open_window(gpui_kit::WindowOptions::default(), cx, |_, cx| {
                    cx.new(|_| Workbench)
                })
                .expect("failed to open Metis workbench window");
                launch_state.store(false, Ordering::Release);
            });
    }));
    std::panic::set_hook(Box::new(move |info| previous_hook(info)));

    match result {
        Ok(()) => Ok(()),
        Err(payload) if launching.load(Ordering::Acquire) => {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("GPUI failed with a non-string panic payload");
            Err(metis::Error::WorkbenchStartup(message.to_owned()).into())
        }
        Err(payload) => resume_unwind(payload),
    }
}

/// Logs always go to stderr; default level is `warn`, override with `RUST_LOG`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    #[test]
    fn watchdog_subcommand_name_is_wired() {
        assert!(
            super::Cli::command()
                .find_subcommand(metis::runtime::WATCHDOG_COMMAND)
                .is_some()
        );
    }
}
