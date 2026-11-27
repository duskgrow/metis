#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui_kit::{
    AppContext as _, Bounds, Entity, Point, TestAppContext, VisualTestContext, WindowBounds,
    WindowHandle, WindowOptions,
    base::{Root, test_support},
    px, size,
    test::TestWindowExt as _,
};
use metis::{
    client::{ApprovalOutcome, ApprovalRequest, DshClientConfig},
    runtime::DshRuntimeConfig,
    session_transport::LocalSessionConfig,
};
use serde_json::json;

use super::{ConnectionState, Workbench, cold_start_result_count};

const WAIT: Duration = Duration::from_secs(30);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "metis-workbench-cold-start-{name}-{}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).expect("remove stale Workbench test directory");
        }
        fs::create_dir_all(&path).expect("create Workbench test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn metis_binary() -> PathBuf {
    let test_binary = std::env::current_exe().expect("locate cold-start test binary");
    let name = if cfg!(windows) { "metis.exe" } else { "metis" };
    for ancestor in test_binary.ancestors().take(10) {
        let candidate = ancestor.join(name);
        if candidate != test_binary && candidate.is_file() {
            return candidate;
        }
    }
    panic!("no normal Metis binary beside {test_binary:?}");
}

fn cold_start_config(directory: &TestDirectory) -> LocalSessionConfig {
    let layout = crate::HostLayout::locate_from(metis_binary()).expect("locate source Host layout");
    LocalSessionConfig {
        runtime: DshRuntimeConfig {
            metis_executable: layout.executable,
            node_executable: layout.node_executable,
            host_entrypoint: layout.host_entrypoint,
            home: directory.path.join("workbench-runs"),
            startup_timeout: WAIT,
            shutdown_timeout: Duration::from_secs(15),
        },
        client: DshClientConfig::default(),
        project_path: directory.path.clone(),
    }
}

fn run_artifacts(home: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(home) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(entries
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("run-") || name.starts_with(".run-")
        })
        .map(|entry| entry.path())
        .collect())
}

fn open_cold_start(
    cx: &mut TestAppContext,
    config: LocalSessionConfig,
) -> (WindowHandle<Root>, Entity<Workbench>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1100.), px(720.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let view = cx.new(|cx| Workbench::cold_start(config, window, cx));
                view.update(cx, |this, cx| this.mount(window, cx));
                view
            },
        )
        .expect("open cold-start Workbench");
        (window.downcast::<Root>().expect("Base Root"), view)
    })
}

fn visual(handle: WindowHandle<Root>, cx: &TestAppContext) -> VisualTestContext {
    VisualTestContext::from_window(handle.into(), cx)
}

fn wait_for(
    cx: &mut VisualTestContext,
    what: &str,
    mut predicate: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + WAIT;
    loop {
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        if predicate(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn render(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
    });
}

fn process_running(pid: u32) -> bool {
    let pid = i32::try_from(pid).expect("Host PID fits in i32");
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

#[gpui_kit::test]
#[allow(clippy::too_many_lines)] // Keep the real-Host lifecycle assertions in order.
fn real_host_cold_start_projects_notices_and_cleans_up_on_window_close(cx: &mut TestAppContext) {
    let directory = TestDirectory::new("success");
    let config = cold_start_config(&directory);
    let home = config.runtime.home.clone();
    let (handle, view) = open_cold_start(cx, config);
    let mut cx = visual(handle, cx);
    wait_for(&mut cx, "the real Host snapshot", |cx| {
        view.read_with(cx, |view, _| {
            view.projection.connection == ConnectionState::Online
                && view
                    .projection
                    .sessions
                    .first()
                    .is_some_and(|session| session.generation == 1)
                && view
                    .host
                    .as_ref()
                    .is_some_and(|host| host.bridge.borrow().cursor().is_some())
        })
    });

    let (session_id, watchdog, host_process) = view.read_with(&cx, |view, _| {
        let host = view.host.as_ref().expect("active local Host");
        (
            view.current_projection()
                .expect("current Session")
                .key
                .session_id
                .clone(),
            host.local.runtime().id(),
            host.local
                .runtime()
                .host_process_id()
                .expect("running Node Host PID"),
        )
    });
    let transport_thread = cx.update(|_, cx| {
        view.update(cx, |view, _| {
            view.host
                .as_mut()
                .expect("active local Host")
                .transport
                .as_mut()
                .expect("active transport")
                .thread()
                .expect("transport thread")
        })
    });
    cx.update(|_, cx| {
        view.update(cx, |view, _| {
            view.host
                .as_ref()
                .expect("active local Host")
                .transport
                .as_ref()
                .expect("active transport")
                .commands()
                .try_send(metis::session_transport::TransportCommand::Answer {
                    request: ApprovalRequest {
                        client_id: "foreign-generation".to_owned(),
                        event_id: "foreign-event".to_owned(),
                        agent_id: session_id,
                        request: json!({"callId": "call-1", "toolName": "bash"}),
                    },
                    outcome: ApprovalOutcome::Rejected,
                })
                .expect("record forged approval");
        });
    });
    wait_for(&mut cx, "the rendered command failure", |cx| {
        view.read_with(cx, |view, cx| {
            view.composer.as_ref().is_some_and(|composer| {
                composer
                    .read(cx)
                    .feedback()
                    .is_some_and(|feedback| feedback.contains("$events/result"))
            })
        })
    });
    render(&mut cx);
    cx.update(|window, _| {
        assert!(test_support::texts(window).iter().any(|text| {
            text.text()
                .contains("$events/result was not completed by the Host")
        }));
    });

    let runs = run_artifacts(&home).expect("read ephemeral artifacts");
    assert_eq!(runs.len(), 2, "run directory and its owner lock");
    let run_home = runs
        .into_iter()
        .find(|path| path.is_dir())
        .expect("run directory");
    cx.update(|window, cx| {
        window.remove_window();
        cx.shutdown();
    });
    drop(view);
    let deadline = Instant::now() + WAIT;
    while !transport_thread.is_finished()
        || run_home.exists()
        || process_running(watchdog)
        || process_running(host_process)
    {
        assert!(
            Instant::now() < deadline,
            "timed out closing the real Workbench Host: transport_finished={} run_home_exists={} watchdog_running={} host_running={}",
            transport_thread.is_finished(),
            run_home.exists(),
            process_running(watchdog),
            process_running(host_process),
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    transport_thread.join().expect("join transport thread");
}

#[gpui_kit::test]
fn cold_start_failure_stays_offline_and_removes_the_ephemeral_home(cx: &mut TestAppContext) {
    let directory = TestDirectory::new("failure");
    let mut config = cold_start_config(&directory);
    config.runtime.metis_executable = directory.path.join("missing-metis");
    let home = config.runtime.home.clone();
    let (handle, view) = open_cold_start(cx, config);
    let mut cx = visual(handle, cx);
    wait_for(&mut cx, "the startup failure", |cx| {
        view.read_with(cx, |view, _| {
            view.status_notice
                .as_ref()
                .is_some_and(|notice| notice.as_str().contains("Local Host startup failed"))
        })
    });
    assert!(view.read_with(&cx, |view, _| view.projection.sessions.is_empty()));
    assert_eq!(
        run_artifacts(&home).expect("read cleaned parent"),
        Vec::<PathBuf>::new()
    );
}

#[gpui_kit::test]
fn closing_before_startup_finishes_still_cleans_up_the_late_result(cx: &mut TestAppContext) {
    let directory = TestDirectory::new("close-during-startup");
    let config = cold_start_config(&directory);
    let home = config.runtime.home.clone();
    let completed = cold_start_result_count();
    let (handle, view) = open_cold_start(cx, config);
    let mut cx = visual(handle, cx);
    cx.update(|window, _| window.remove_window());
    drop(view);
    let deadline = Instant::now() + WAIT;
    loop {
        cx.run_until_parked();
        if cold_start_result_count() > completed
            && run_artifacts(&home)
                .expect("read ephemeral parent")
                .is_empty()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out cleaning a late cold-start result"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
