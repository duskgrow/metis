//! Command-line shell for `metis`.
//!
//! `--help` and `--version` are generated from the clap definitions.

use clap::{Args, Parser, Subcommand};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
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

#[cfg(feature = "workbench")]
mod workbench;

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
    let packaged = HostLayout::locate()?;
    let node_executable = args.node.unwrap_or(packaged.node_executable);
    let host_entrypoint = match args.host_entrypoint {
        Some(path) => path,
        None => packaged.host_entrypoint,
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
                metis_executable: packaged.executable,
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

struct HostLayout {
    executable: PathBuf,
    node_executable: PathBuf,
    host_entrypoint: PathBuf,
}

impl HostLayout {
    fn locate() -> metis::Result<Self> {
        let executable = std::env::current_exe().map_err(metis::Error::RuntimeLocate)?;
        Self::locate_from(executable)
    }

    fn locate_from(executable: PathBuf) -> metis::Result<Self> {
        Self::locate_from_with(executable, Path::is_file)
    }

    fn locate_from_with(
        executable: PathBuf,
        is_file: impl Fn(&Path) -> bool,
    ) -> metis::Result<Self> {
        let binary_directory = executable.parent().ok_or_else(installation_layout_error)?;
        let node_executable =
            binary_directory.join(if cfg!(windows) { "node.exe" } else { "node" });
        let host_entrypoint = binary_directory
            .parent()
            .ok_or_else(installation_layout_error)?
            .join("lib")
            .join("metis-host")
            .join("lib")
            .join("host.js");
        if is_file(&node_executable) && is_file(&host_entrypoint) {
            return Ok(Self {
                executable,
                node_executable,
                host_entrypoint,
            });
        }

        // Source builds do not have the archive's bin/lib layout. Bound the
        // ancestor search and require the actual built Host entry point.
        for ancestor in binary_directory.ancestors().take(6) {
            let source_entrypoint = ancestor
                .join("packages")
                .join("metis-host")
                .join("lib")
                .join("host.js");
            if is_file(&source_entrypoint) {
                return Ok(Self {
                    executable,
                    node_executable: PathBuf::from("node"),
                    host_entrypoint: source_entrypoint,
                });
            }
        }
        // Preserve the standard paths in the eventual spawn diagnostic when
        // neither a package nor a source checkout supplies the Host.
        Ok(Self {
            executable,
            node_executable,
            host_entrypoint,
        })
    }
}

#[cfg(feature = "workbench")]
fn workbench_session_config() -> metis::Result<metis::session_transport::LocalSessionConfig> {
    let packaged = HostLayout::locate()?;
    let project_path = std::env::current_dir().map_err(metis::Error::RuntimeLocate)?;
    let user_home = std::env::home_dir().ok_or_else(|| {
        metis::Error::RuntimeLocate(io::Error::new(
            io::ErrorKind::NotFound,
            "the user home directory is unavailable",
        ))
    })?;
    // Each launch atomically creates and later removes a child run directory;
    // the stable parent is only a container, never shared Host state.
    let home = user_home.join(".metis").join("workbench-runs");
    Ok(metis::session_transport::LocalSessionConfig {
        runtime: metis::runtime::DshRuntimeConfig {
            metis_executable: packaged.executable,
            node_executable: packaged.node_executable,
            host_entrypoint: packaged.host_entrypoint,
            home,
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(15),
        },
        client: metis::client::DshClientConfig::default(),
        project_path,
    })
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

    let session_config = workbench_session_config()?;
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
                open_window(gpui_kit::WindowOptions::default(), cx, move |window, cx| {
                    let view = cx.new(|cx| {
                        workbench::session::Workbench::cold_start(session_config, window, cx)
                    });
                    view.update(cx, |this, cx| this.mount(window, cx));
                    view
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
    use std::path::{Path, PathBuf};

    use clap::CommandFactory as _;

    #[test]
    fn packaged_and_source_host_layouts_are_discovered() {
        let package = Path::new("package");
        let packaged_node =
            package
                .join("bin")
                .join(if cfg!(windows) { "node.exe" } else { "node" });
        let packaged_host = package
            .join("lib")
            .join("metis-host")
            .join("lib")
            .join("host.js");
        let packaged =
            super::HostLayout::locate_from_with(package.join("bin").join("metis"), |path| {
                path == packaged_node || path == packaged_host
            })
            .expect("packaged layout");
        assert_eq!(packaged.node_executable, packaged_node);
        assert_eq!(packaged.host_entrypoint, packaged_host);

        let source = Path::new("workspace/metis");
        let source_host = source
            .join("packages")
            .join("metis-host")
            .join("lib")
            .join("host.js");
        let layout = super::HostLayout::locate_from_with(
            source.join("target").join("debug").join("metis"),
            |path| path == source_host,
        )
        .expect("source layout");
        assert_eq!(layout.node_executable, PathBuf::from("node"));
        assert_eq!(layout.host_entrypoint, source_host);
    }

    #[test]
    fn watchdog_subcommand_name_is_wired() {
        assert!(
            super::Cli::command()
                .find_subcommand(metis::runtime::WATCHDOG_COMMAND)
                .is_some()
        );
    }
}
