//! Real-Host coverage for the asynchronous Session transport: ordered frames,
//! `$events` approvals, command failures, and connection closure.

mod support;

use std::time::{Duration, Instant};

use metis::{
    client::{
        ApprovalOutcome, ApprovalRequest, ClientError, DshClient, DshClientConfig, HostEvent,
    },
    runtime::{DshRuntime, DshSupervisor},
    session_transport::{SessionTransport, TransportCommand, TransportEvent},
};
use support::{TestDirectory, approval_runtime_config, create_session_at};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);

struct TransportFixture {
    // Held for cleanup; the Session id is only needed during setup.
    _directory: TestDirectory,
    runtime: DshRuntime,
    transport: SessionTransport,
    events: mpsc::Receiver<TransportEvent>,
}

fn approval_transport(name: &str) -> TransportFixture {
    let directory = TestDirectory::new(name);
    let config = approval_runtime_config(&directory);
    let runtime = DshSupervisor::system()
        .start(&config)
        .expect("start Host with approval fixture");

    // Session creation belongs to the product entry, so the transport binds an
    // already-created Session through a separate, short-lived setup client.
    let auth_url = runtime.auth_url().to_owned();
    let setup_runtime = tokio::runtime::Runtime::new().expect("create setup runtime");
    let session_id = setup_runtime.block_on(async {
        let client = DshClient::connect(&auth_url, DshClientConfig::default())
            .await
            .expect("connect setup client");
        let session_id = create_session_at(&client, &directory.path).await;
        client.close().await.expect("close setup client");
        session_id
    });
    drop(setup_runtime);

    let mut transport = SessionTransport::spawn(&auth_url, &session_id, DshClientConfig::default());
    let events = transport.take_events().expect("take transport events");
    TransportFixture {
        _directory: directory,
        runtime,
        transport,
        events,
    }
}

fn next_event(
    events: &mut mpsc::Receiver<TransportEvent>,
    wake: &std::sync::mpsc::Receiver<()>,
    what: &str,
) -> TransportEvent {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(event) = events.try_recv() {
            return event;
        }
        let _ = wake.recv_timeout(Duration::from_millis(50));
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
    }
}

fn expect_connected(fixture: &mut TransportFixture) {
    // The wake must arrive with the first event; polling events alone would
    // not catch a missing wake signal.
    fixture
        .transport
        .wake()
        .recv_timeout(WAIT)
        .expect("wake after connect");
    match next_event(&mut fixture.events, fixture.transport.wake(), "Connected") {
        TransportEvent::Connected { client_id } => assert_ne!(client_id, ""),
        other => panic!("expected Connected, got {other:?}"),
    }
}

fn next_follow_frame(
    events: &mut mpsc::Receiver<TransportEvent>,
    wake: &std::sync::mpsc::Receiver<()>,
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
            .runtime
            .output_snapshot()
            .contains("[metis-test] approval resolved")
        {
            break;
        }
        if Instant::now() >= deadline {
            for line in fixture
                .runtime
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
        .runtime
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
