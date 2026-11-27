//! Real-Host coverage for the asynchronous Session transport: ordered frames,
//! `$events` approvals, command failures, and connection closure.

mod support;

use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

use metis::{
    client::{ApprovalOutcome, ApprovalRequest, ClientError, DshClientConfig, HostEvent},
    runtime::DshSupervisor,
    session_transport::{
        LocalSession, LocalSessionConfig, SessionTransport, TransportCommand, TransportEvent,
    },
};
use support::{TestDirectory, approval_runtime_config, real_dsh_entrypoint, runtime_config};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);

struct TransportFixture {
    // Held for cleanup; the Session id is only needed during setup.
    _directory: TestDirectory,
    local: LocalSession,
    transport: SessionTransport,
    events: mpsc::Receiver<TransportEvent>,
}

fn approval_transport(name: &str) -> TransportFixture {
    let directory = TestDirectory::new(name);
    start_transport(directory, true)
}

fn start_transport(directory: TestDirectory, approval_fixture: bool) -> TransportFixture {
    // Product cold start owns project opening and Session creation; transport
    // tests start from that same prepared state rather than duplicating setup.
    let local = LocalSession::start(
        &DshSupervisor::system(),
        &LocalSessionConfig {
            runtime: if approval_fixture {
                approval_runtime_config(&directory)
            } else {
                runtime_config(&directory, real_dsh_entrypoint())
            },
            client: DshClientConfig::default(),
            project_path: directory.path.clone(),
        },
    )
    .expect("prepare local project Session");
    assert_ne!(local.session_id(), "");
    let mut transport = local.spawn_transport();
    let events = transport.take_events().expect("take transport events");
    TransportFixture {
        _directory: directory,
        local,
        transport,
        events,
    }
}

fn run_artifacts(home: &std::path::Path) -> Vec<fs::DirEntry> {
    fs::read_dir(home)
        .expect("read ephemeral parent")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect ephemeral entries")
        .into_iter()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("run-") || name.starts_with(".run-")
        })
        .collect()
}

fn next_event(
    events: &mut mpsc::Receiver<TransportEvent>,
    wake: &mut mpsc::Receiver<()>,
    what: &str,
) -> TransportEvent {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(event) = events.try_recv() {
            return event;
        }
        if wake.try_recv().is_err() {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
    }
}

fn expect_connected(fixture: &mut TransportFixture) {
    // The wake must arrive with the first event; polling events alone would
    // not catch a missing wake signal.
    let deadline = Instant::now() + WAIT;
    loop {
        match fixture.transport.wake().try_recv() {
            Ok(()) => break,
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                panic!("wake channel closed before Connected")
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for connect wake"
        );
    }
    match next_event(&mut fixture.events, fixture.transport.wake(), "Connected") {
        TransportEvent::Connected { client_id } => assert_ne!(client_id, ""),
        other => panic!("expected Connected, got {other:?}"),
    }
}

fn next_follow_frame(
    events: &mut mpsc::Receiver<TransportEvent>,
    wake: &mut mpsc::Receiver<()>,
    predicate: impl Fn(&serde_json::Value) -> bool,
    what: &str,
) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        match next_event(events, wake, what) {
            TransportEvent::FollowFrame(frame) if predicate(&frame) => return,
            TransportEvent::CommandFailed {
                operation, error, ..
            } => {
                panic!("{operation} failed while waiting for {what}: {error}");
            }
            TransportEvent::Closed { reason } => {
                panic!("transport closed while waiting for {what}: {reason:?}");
            }
            _ => {}
        }
    }
    panic!("timed out waiting for {what}");
}

fn wait_until(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + WAIT;
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn real_host_git_diff_round_trips_success_and_project_errors() {
    let directory = TestDirectory::new("git-diff-transport");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(["--no-pager", "--no-optional-locks"])
            .args(args)
            .current_dir(&directory.path)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run Git fixture command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&["config", "user.name", "Metis Git Diff Test"]);
    git(&["config", "user.email", "metis@example.invalid"]);
    fs::write(directory.path.join("tracked.txt"), "before\n").expect("write tracked file");
    git(&["add", "tracked.txt"]);
    git(&["commit", "--quiet", "-m", "initial"]);
    fs::write(directory.path.join("tracked.txt"), "after\n").expect("modify tracked file");

    let mut fixture = start_transport(directory, false);
    expect_connected(&mut fixture);
    next_follow_frame(
        &mut fixture.events,
        fixture.transport.wake(),
        |frame| frame.get("type").and_then(serde_json::Value::as_str) == Some("snapshot"),
        "the opening snapshot",
    );
    let project_id = fixture.local.project().project_id.clone();
    fixture
        .transport
        .commands()
        .try_send(TransportCommand::GitDiff {
            request_id: 11,
            project_id: project_id.clone(),
        })
        .expect("request the project diff");
    loop {
        match next_event(
            &mut fixture.events,
            fixture.transport.wake(),
            "Git diff result",
        ) {
            TransportEvent::GitDiff {
                request_id: 11,
                result: Ok(patch),
            } => {
                assert!(patch.contains("diff --git a/tracked.txt b/tracked.txt"));
                assert!(patch.contains("-before"));
                assert!(patch.contains("+after"));
                break;
            }
            TransportEvent::GitDiff { request_id, result } => {
                panic!("unexpected diff result for {request_id}: {result:?}");
            }
            TransportEvent::CommandFailed {
                operation, error, ..
            } => panic!("{operation} failed: {error}"),
            TransportEvent::Closed { reason } => panic!("transport closed: {reason:?}"),
            _ => {}
        }
    }

    fixture
        .transport
        .commands()
        .try_send(TransportCommand::GitDiff {
            request_id: 12,
            project_id: "missing-project".to_owned(),
        })
        .expect("request an invalid project diff");
    loop {
        match next_event(
            &mut fixture.events,
            fixture.transport.wake(),
            "Git diff error",
        ) {
            TransportEvent::GitDiff {
                request_id: 12,
                result: Err(_),
            } => break,
            TransportEvent::GitDiff { request_id, result } => {
                panic!("unexpected diff result for {request_id}: {result:?}");
            }
            TransportEvent::Closed { reason } => panic!("transport closed: {reason:?}"),
            _ => {}
        }
    }
    fixture.transport.close_and_join();
    fixture.local.shutdown().expect("stop Git Diff Host");
}

#[test]
fn ephemeral_homes_are_removed_after_success_and_startup_failure() {
    let directory = TestDirectory::new("ephemeral-home-cleanup");
    let home = directory.path.join("workbench-runs");
    let mut runtime = runtime_config(&directory, real_dsh_entrypoint());
    runtime.home.clone_from(&home);
    let config = LocalSessionConfig {
        runtime,
        client: DshClientConfig::default(),
        project_path: directory.path.clone(),
    };
    let mut local = LocalSession::start_ephemeral(&DshSupervisor::system(), &config)
        .expect("start ephemeral Host");
    let runs = run_artifacts(&home);
    assert_eq!(runs.len(), 2, "run directory and its owner lock");
    let run_home = runs
        .iter()
        .find(|entry| entry.file_type().expect("run file type").is_dir())
        .expect("run directory")
        .path();
    local.shutdown().expect("stop ephemeral Host");
    drop(local);
    assert!(!run_home.exists());

    let mut missing_runtime = runtime_config(&directory, real_dsh_entrypoint());
    missing_runtime.home.clone_from(&home);
    missing_runtime.metis_executable = directory.path.join("missing-metis");
    assert!(
        LocalSession::start_ephemeral(
            &DshSupervisor::system(),
            &LocalSessionConfig {
                runtime: missing_runtime,
                project_path: directory.path.clone(),
                ..config
            },
        )
        .is_err()
    );
    assert!(run_artifacts(&home).is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One linear real-Host scenario stays readable as a whole.
fn prompt_approval_and_failure_round_trip_through_real_host() {
    let mut fixture = approval_transport("session-transport");
    expect_connected(&mut fixture);
    next_follow_frame(
        &mut fixture.events,
        fixture.transport.wake(),
        |frame| frame.get("type").and_then(serde_json::Value::as_str) == Some("snapshot"),
        "the opening snapshot",
    );

    let commands = fixture.transport.commands();
    commands
        .try_send(TransportCommand::Prompt {
            request_id: "metis-transport-test-prompt-1".to_owned(),
            text: "METIS_TEST_APPROVAL verify the working directory".to_owned(),
        })
        .expect("record prompt intent");
    let approval = loop {
        match next_event(
            &mut fixture.events,
            fixture.transport.wake(),
            "live approval request",
        ) {
            TransportEvent::Host(HostEvent::Approval(request)) => break request,
            TransportEvent::CommandFailed {
                operation, error, ..
            } => {
                panic!("{operation} failed before the approval arrived: {error}");
            }
            TransportEvent::Closed { reason } => {
                panic!("transport closed before the approval arrived: {reason:?}");
            }
            _ => {}
        }
    };

    // A foreign generation is rejected locally and reported honestly instead
    // of being acknowledged as a Host decision.
    let mut forged: ApprovalRequest = approval.clone();
    forged.client_id = "foreign-generation".to_owned();
    commands
        .try_send(TransportCommand::Answer {
            request: forged,
            outcome: ApprovalOutcome::Rejected,
        })
        .expect("record forged answer");
    loop {
        match next_event(
            &mut fixture.events,
            fixture.transport.wake(),
            "forged answer failure",
        ) {
            TransportEvent::CommandFailed {
                operation: "$events/result",
                error: ClientError::Protocol(message),
                ..
            } => {
                assert!(message.contains("generation"), "local rejection: {message}");
                break;
            }
            TransportEvent::CommandFailed {
                operation, error, ..
            } => {
                panic!("unexpected {operation} failure: {error}");
            }
            TransportEvent::Closed { reason } => {
                panic!("transport closed before the forged answer failure: {reason:?}");
            }
            _ => {}
        }
    }

    commands
        .try_send(TransportCommand::Answer {
            request: approval,
            outcome: ApprovalOutcome::AllowedOnce,
        })
        .expect("record real answer");
    // The fixture logs the resolution; durable decision frames are covered by
    // the projection tests, so no particular follow frame is assumed here.
    let deadline = Instant::now() + WAIT;
    loop {
        while let Ok(event) = fixture.events.try_recv() {
            match event {
                TransportEvent::CommandFailed {
                    operation, error, ..
                } => {
                    panic!("{operation} failed after the real answer: {error}");
                }
                TransportEvent::Closed { reason } => {
                    panic!("transport closed after the real answer: {reason:?}");
                }
                _ => {}
            }
        }
        if fixture
            .local
            .runtime()
            .output_snapshot()
            .contains("[metis-test] approval resolved")
        {
            break;
        }
        if Instant::now() >= deadline {
            for line in fixture
                .local
                .runtime()
                .output_snapshot()
                .lines()
                .filter(|line| line.contains("[metis-test]"))
                .collect::<Vec<_>>()
            {
                eprintln!("{line}");
            }
            panic!("approval fixture did not resolve after the real answer");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    fixture
        .local
        .shutdown()
        .expect("stop approval fixture Host");
    loop {
        if let TransportEvent::Closed { .. } = next_event(
            &mut fixture.events,
            fixture.transport.wake(),
            "Closed after Host stop",
        ) {
            break;
        }
    }
    drop(fixture.events);
    fixture.transport.close_and_join();
}

#[test]
fn close_and_join_finishes_without_draining_events() {
    let mut fixture = approval_transport("transport-close-backpressure");
    expect_connected(&mut fixture);
    fixture
        .transport
        .commands()
        .try_send(TransportCommand::Prompt {
            request_id: "metis-transport-test-prompt-close".to_owned(),
            text: "METIS_TEST_APPROVAL keep events flowing".to_owned(),
        })
        .expect("record prompt intent");
    // Let the approval flow produce events without draining the queue.
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    fixture.transport.close_and_join();
    assert!(
        started.elapsed() < WAIT,
        "close_and_join must interrupt backpressured publishes"
    );
}

#[test]
fn drop_signals_close_and_the_thread_exits() {
    let mut fixture = approval_transport("transport-drop-close");
    expect_connected(&mut fixture);
    let thread = fixture.transport.thread().expect("take thread handle");
    drop(fixture.transport);
    wait_until(|| thread.is_finished(), "transport thread exit after drop");
    thread.join().expect("join transport thread");
}

#[test]
fn unreachable_host_reports_closed_instead_of_panicking() {
    // Bind and release a loopback port so the target is very likely closed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind probe port")
        .local_addr()
        .expect("probe address")
        .port();
    let auth_url = format!("http://127.0.0.1:{port}/connect?token=unreachable");
    let mut transport = SessionTransport::spawn(
        &auth_url,
        "session-1",
        DshClientConfig {
            request_timeout: Duration::from_secs(2),
            stream_open_timeout: Duration::from_secs(2),
        },
    );
    let mut events = transport.take_events().expect("take transport events");
    match next_event(&mut events, transport.wake(), "Closed for unreachable Host") {
        TransportEvent::Closed {
            reason: Some(error),
        } => {
            assert!(
                !format!("{error}").contains("unreachable"),
                "error must not echo the auth token"
            );
        }
        other => panic!("expected Closed with a reason, got {other:?}"),
    }
    drop(events);
    transport.close_and_join();
}
