//! Command-line shell for `metis`.
//!
//! `--help` and `--version` are generated from the clap definitions.

use clap::Parser;
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
struct Cli {}

#[cfg(feature = "workbench")]
fn main() -> miette::Result<()> {
    init_tracing();
    Cli::parse();
    run_workbench()
}

#[cfg(not(feature = "workbench"))]
fn main() -> miette::Result<()> {
    init_tracing();
    Cli::parse();
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
