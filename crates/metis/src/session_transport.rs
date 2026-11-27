//! Asynchronous Host Session transport on a dedicated tokio thread.
//!
//! The transport forwards ordered follow frames and `$events` unchanged;
//! interpreting them belongs to the UI-thread projection. Bounded queues apply
//! backpressure instead of dropping ordered frames. The command worker owns
//! the `DshClient` exclusively, so the pump loop cannot execute RPCs even by
//! accident: streams and the independent close signal always keep progress.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    thread,
};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, watch};

use crate::{
    client::{
        ApprovalOutcome, ApprovalRequest, ClientError, ClientResult, DshClient, DshClientConfig,
        DshStream, HostEvent, HostEvents, Project,
    },
    runtime::{DshRuntime, DshRuntimeConfig, DshSupervisor},
};

/// Ordered events are never dropped; a full queue slows the Host stream.
const EVENT_CAPACITY: usize = 16;
/// User intents are low-frequency; a full queue is reported to the sender.
const COMMAND_CAPACITY: usize = 16;
/// Snapshot budget for the visible Session window.
const FOLLOW_MESSAGE_LIMIT: u64 = 200;

/// Everything needed to prepare the local Workbench's private Host and first
/// project-attached Session. Construction is blocking; graphical callers run it
/// on a background executor.
#[derive(Clone, Debug)]
pub struct LocalSessionConfig {
    pub runtime: DshRuntimeConfig,
    pub client: DshClientConfig,
    pub project_path: PathBuf,
}

/// A running local Host with its first Session ready for a dedicated transport.
/// The authentication URL remains private and is never included in diagnostics.
pub struct LocalSession {
    runtime: DshRuntime,
    auth_url: String,
    client: DshClientConfig,
    project: Project,
    session_id: String,
    // Declared after the runtime so an unobserved drop stops the Host before
    // removing its state. ActiveHost also shuts down explicitly first.
    owned_home: Option<OwnedHome>,
}

struct OwnedHome {
    path: PathBuf,
    lock_path: PathBuf,
    lock: Option<File>,
}

impl OwnedHome {
    fn create(parent: &Path) -> crate::Result<Self> {
        fs::create_dir_all(parent).map_err(|source| crate::Error::RuntimePrepare {
            path: parent.to_owned(),
            source,
        })?;
        // Allocation and stale recovery are serialized. An owner lock is
        // acquired before its run directory is published, so a free lock can
        // never belong to a half-initialized live allocation.
        let allocation_path = parent.join(".allocation.lock");
        let allocation = open_lock(&allocation_path, false)?;
        allocation
            .lock()
            .map_err(|source| crate::Error::RuntimePrepare {
                path: allocation_path,
                source,
            })?;
        recover_stale_homes(parent)?;

        for attempt in 0..100 {
            let stem = format!("run-{}-{attempt}", std::process::id());
            let path = parent.join(&stem);
            let lock_path = parent.join(format!(".{stem}.lock"));
            let lock = match open_lock(&lock_path, true) {
                Ok(lock) => lock,
                Err(crate::Error::RuntimePrepare { source, .. })
                    if source.kind() == io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let Err(source) = lock.try_lock() {
                drop(lock);
                let _ = fs::remove_file(&lock_path);
                let source = match source {
                    TryLockError::WouldBlock => io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "new Workbench owner lock is unexpectedly held",
                    ),
                    TryLockError::Error(source) => source,
                };
                return Err(crate::Error::RuntimePrepare {
                    path: lock_path,
                    source,
                });
            }
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        lock_path,
                        lock: Some(lock),
                    });
                }
                Err(source) => {
                    drop(lock);
                    let _ = fs::remove_file(&lock_path);
                    if source.kind() == io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    return Err(crate::Error::RuntimePrepare { path, source });
                }
            }
        }
        Err(crate::Error::RuntimePrepare {
            path: parent.to_owned(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate an ephemeral Workbench Host directory",
            ),
        })
    }
}

impl Drop for OwnedHome {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {
                let _ = fs::remove_file(&self.lock_path);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&self.lock_path);
            }
            // Keep the lock path for a later recovery attempt if directory
            // removal failed. Releasing the owner lock makes it eligible.
            Err(_) => {}
        }
        self.lock.take();
    }
}

fn open_lock(path: &Path, create_new: bool) -> crate::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(!create_new)
        .create_new(create_new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|source| crate::Error::RuntimePrepare {
            path: path.to_owned(),
            source,
        })
}

fn recover_stale_homes(parent: &Path) -> crate::Result<()> {
    let entries = fs::read_dir(parent).map_err(|source| crate::Error::RuntimePrepare {
        path: parent.to_owned(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| crate::Error::RuntimePrepare {
            path: parent.to_owned(),
            source,
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(".lock"))
            .filter(|stem| stem.starts_with("run-"))
        else {
            continue;
        };
        let lock_path = entry.path();
        let lock = match open_lock(&lock_path, false) {
            Ok(lock) => lock,
            Err(crate::Error::RuntimePrepare { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        match lock.try_lock() {
            Ok(()) => {
                drop(lock);
                let path = parent.join(stem);
                match fs::remove_dir_all(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(crate::Error::RuntimePrepare { path, source });
                    }
                }
                match fs::remove_file(&lock_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(crate::Error::RuntimePrepare {
                            path: lock_path,
                            source,
                        });
                    }
                }
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(source)) => {
                return Err(crate::Error::RuntimePrepare {
                    path: lock_path,
                    source,
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum LocalSessionError {
    #[error("failed to start the local Host: {0}")]
    Runtime(#[source] Box<crate::Error>),
    #[error("failed to create the local Host executor: {0}")]
    Executor(#[source] io::Error),
    #[error("the local project path is not valid UTF-8")]
    ProjectPathNotUtf8,
    #[error("local Host {operation} failed: {source}")]
    Client {
        operation: &'static str,
        #[source]
        source: Box<ClientError>,
    },
}

impl LocalSession {
    /// Start with an isolated child of `runtime.home` and remove that child
    /// after the Host stops. The stable parent and caller-owned configuration
    /// are never deleted.
    pub fn start_ephemeral(
        supervisor: &DshSupervisor,
        config: &LocalSessionConfig,
    ) -> Result<Self, LocalSessionError> {
        let home = OwnedHome::create(&config.runtime.home)
            .map_err(|error| LocalSessionError::Runtime(Box::new(error)))?;
        let mut config = config.clone();
        config.runtime.home.clone_from(&home.path);
        let mut session = Self::start(supervisor, &config)?;
        session.owned_home = Some(home);
        Ok(session)
    }

    /// Start the supervised Host, open the configured project, create or reuse
    /// its Session, and close the short-lived setup client. Any error drops the
    /// runtime owner, which cleans up the supervised process tree.
    pub fn start(
        supervisor: &DshSupervisor,
        config: &LocalSessionConfig,
    ) -> Result<Self, LocalSessionError> {
        let runtime = supervisor
            .start(&config.runtime)
            .map_err(|error| LocalSessionError::Runtime(Box::new(error)))?;
        let auth_url = runtime.auth_url().to_owned();
        let project_path = config
            .project_path
            .to_str()
            .ok_or(LocalSessionError::ProjectPathNotUtf8)?
            .to_owned();
        let async_runtime = tokio::runtime::Runtime::new().map_err(LocalSessionError::Executor)?;
        let (project, session_id) = async_runtime.block_on(async {
            let client = DshClient::connect(&auth_url, config.client)
                .await
                .map_err(|source| LocalSessionError::Client {
                    operation: "connection",
                    source: Box::new(source),
                })?;
            let project = client.open_project(&project_path).await.map_err(|source| {
                LocalSessionError::Client {
                    operation: "project/open",
                    source: Box::new(source),
                }
            })?;
            let session_id =
                client
                    .create_session(&project.project_id)
                    .await
                    .map_err(|source| LocalSessionError::Client {
                        operation: "session/create",
                        source: Box::new(source),
                    })?;
            // The dedicated transport owns the long-lived connection. Setup
            // close failure does not invalidate the already-created Session.
            let _ = client.close().await;
            Ok::<_, LocalSessionError>((project, session_id))
        })?;
        Ok(Self {
            runtime,
            auth_url,
            client: config.client,
            project,
            session_id,
            owned_home: None,
        })
    }

    #[must_use]
    pub fn project(&self) -> &Project {
        &self.project
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn runtime(&self) -> &DshRuntime {
        &self.runtime
    }

    /// Start a new client generation bound to the prepared Session.
    #[must_use]
    pub fn spawn_transport(&self) -> SessionTransport {
        SessionTransport::spawn(&self.auth_url, &self.session_id, self.client)
    }

    /// Stop the supervised Host after its transport has been closed.
    pub fn shutdown(&mut self) -> crate::Result<()> {
        self.runtime.shutdown()
    }
}

/// Intents accepted by the transport. Session identity is bound at spawn time;
/// model/effort selection is not yet submitted to the Host. The caller owns
/// prompt request ids so a retry can reuse the same identity unchanged.
#[derive(Debug)]
pub enum TransportCommand {
    Prompt {
        request_id: String,
        text: String,
    },
    Answer {
        request: ApprovalRequest,
        outcome: ApprovalOutcome,
    },
    /// Read the patch for a project already opened on this Host.
    GitDiff {
        request_id: u64,
        project_id: String,
    },
}

/// Ordered transport notifications. Frames retain the Host JSON unchanged.
#[derive(Debug)]
pub enum TransportEvent {
    /// The follow stream opened and carries the current client generation.
    Connected {
        client_id: String,
    },
    FollowFrame(Value),
    Host(HostEvent),
    /// A recorded intent failed; no success state is inferred. `request_id`
    /// correlates a failed prompt so the caller can retry it unchanged.
    CommandFailed {
        operation: &'static str,
        error: ClientError,
        request_id: Option<String>,
    },
    /// Completion of a project-scoped Git diff request, including rejection
    /// by the bounded RPC queue.
    GitDiff {
        request_id: u64,
        result: ClientResult<String>,
    },
    /// Terminal state: no further events follow. `reason` is `None` for an
    /// orderly local or Host-side close. After a local close the notification
    /// is best-effort; joining the thread is the reliable confirmation.
    Closed {
        reason: Option<ClientError>,
    },
}

/// Owns the transport thread. The UI consumes events through
/// [`SessionTransport::take_events`] and wakes on [`SessionTransport::wake`].
pub struct SessionTransport {
    commands: mpsc::Sender<TransportCommand>,
    events: Option<mpsc::Receiver<TransportEvent>>,
    wake: Option<mpsc::Receiver<()>>,
    close: watch::Sender<bool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SessionTransport {
    /// Connect and follow `session_id` on a new thread. Failures surface as
    /// [`TransportEvent::Closed`] instead of panicking across the thread boundary.
    #[must_use]
    pub fn spawn(auth_url: &str, session_id: &str, config: DshClientConfig) -> Self {
        let (command_sender, command_receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (event_sender, event_receiver) = mpsc::channel(EVENT_CAPACITY);
        let (wake_sender, wake_receiver) = mpsc::channel(1);
        let (close_sender, close_receiver) = watch::channel(false);
        let auth_url = auth_url.to_owned();
        let session_id = session_id.to_owned();
        let thread = thread::spawn(move || {
            let outbox = Outbox {
                events: event_sender,
                wake: wake_sender,
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(run(
                    &auth_url,
                    &session_id,
                    config,
                    command_receiver,
                    &outbox,
                    close_receiver,
                )),
                Err(_) => {
                    let _ = outbox.publish_sync(TransportEvent::Closed {
                        reason: Some(ClientError::Transport {
                            operation: "async runtime construction",
                        }),
                    });
                }
            }
        });
        Self {
            commands: command_sender,
            events: Some(event_receiver),
            wake: Some(wake_receiver),
            close: close_sender,
            thread: Some(thread),
        }
    }

    /// Cloneable intent endpoint for the UI-thread effect handler.
    #[must_use]
    pub fn commands(&self) -> mpsc::Sender<TransportCommand> {
        self.commands.clone()
    }

    /// Take the ordered event stream exactly once, for the UI-thread bridge.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<TransportEvent>> {
        self.events.take()
    }

    /// Coalesced wake signal: after receiving, drain all pending events.
    /// A full signal slot is fine because one wake covers every queued event.
    #[must_use]
    pub fn wake(&mut self) -> &mut mpsc::Receiver<()> {
        self.wake.as_mut().expect("transport wake is available")
    }

    /// Move the coalesced wake signal to a UI event pump exactly once. The
    /// receiver is `Send`, so waiting can stay off the UI thread.
    pub fn take_wake(&mut self) -> Option<mpsc::Receiver<()>> {
        self.wake.take()
    }

    /// Take the thread handle for diagnostics or a custom join; the close
    /// signal still works after the handle is taken.
    pub fn thread(&mut self) -> Option<thread::JoinHandle<()>> {
        self.thread.take()
    }

    /// Close orderly and join the thread. The close signal interrupts
    /// backpressured publishes, so this cannot deadlock on an undrained event
    /// queue; never call from the transport thread itself.
    pub fn close_and_join(&mut self) {
        self.events.take();
        let _ = self.close.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SessionTransport {
    fn drop(&mut self) {
        // Detached on purpose: a UI drop must not block on Host shutdown. The
        // signal plus the sender's own drop both wake the transport thread.
        let _ = self.close.send(true);
    }
}

#[derive(Clone)]
struct Outbox {
    events: mpsc::Sender<TransportEvent>,
    wake: mpsc::Sender<()>,
}

impl Outbox {
    /// Publish in order, interruptibly. A closed receiver or a close signal
    /// stops the transport instead of buffering or blocking without bound.
    async fn publish(
        &self,
        event: TransportEvent,
        close: &mut watch::Receiver<bool>,
    ) -> Result<(), ()> {
        tokio::select! {
            result = self.events.send(event) => result.map_err(|_| ())?,
            () = closed(close) => return Err(()),
        }
        let _ = self.wake.try_send(());
        Ok(())
    }

    fn publish_sync(&self, event: TransportEvent) -> Result<(), ()> {
        self.events.blocking_send(event).map_err(|_| ())?;
        let _ = self.wake.try_send(());
        Ok(())
    }
}

/// Resolve once the close flag is set or every close sender has dropped.
async fn closed(close: &mut watch::Receiver<bool>) {
    if *close.borrow() {
        return;
    }
    while close.changed().await.is_ok() {
        if *close.borrow() {
            return;
        }
    }
}

/// The RPC surface the command worker needs; tests substitute a scripted
/// implementation instead of standing up a fake Host.
trait SessionCommands: Send + 'static {
    fn prompt(
        &self,
        session_id: &str,
        request_id: &str,
        text: &str,
    ) -> impl std::future::Future<Output = ClientResult<()>> + Send;
    fn answer(
        &self,
        request: &ApprovalRequest,
        outcome: ApprovalOutcome,
    ) -> impl std::future::Future<Output = ClientResult<()>> + Send;
    fn git_diff(
        &self,
        project_id: &str,
    ) -> impl std::future::Future<Output = ClientResult<String>> + Send;
    fn close(self) -> impl std::future::Future<Output = ()> + Send;
}

impl SessionCommands for DshClient {
    fn prompt(
        &self,
        session_id: &str,
        request_id: &str,
        text: &str,
    ) -> impl std::future::Future<Output = ClientResult<()>> + Send {
        DshClient::prompt(self, session_id, request_id, text)
    }

    fn answer(
        &self,
        request: &ApprovalRequest,
        outcome: ApprovalOutcome,
    ) -> impl std::future::Future<Output = ClientResult<()>> + Send {
        DshClient::answer(self, request, outcome)
    }

    fn git_diff(
        &self,
        project_id: &str,
    ) -> impl std::future::Future<Output = ClientResult<String>> + Send {
        DshClient::git_diff(self, project_id)
    }

    async fn close(self) {
        // A close failure adds no actionable information beyond `Closed`.
        let _ = DshClient::close(self).await;
    }
}

/// Minimal stream seams so the pump loop is testable without a real Host.
trait FrameStream: Send + 'static {
    fn next(&mut self) -> impl std::future::Future<Output = ClientResult<Option<Value>>> + Send;
    fn cancel(&mut self);
}

impl FrameStream for DshStream {
    async fn next(&mut self) -> ClientResult<Option<Value>> {
        DshStream::next(self).await
    }

    fn cancel(&mut self) {
        DshStream::cancel(self);
    }
}

trait EventStream: Send + 'static {
    fn next(&mut self) -> impl std::future::Future<Output = ClientResult<HostEvent>> + Send;
    fn cancel(&mut self);
}

impl EventStream for HostEvents {
    async fn next(&mut self) -> ClientResult<HostEvent> {
        HostEvents::next(self).await
    }

    fn cancel(&mut self) {
        HostEvents::cancel(self);
    }
}

struct OpenedSession {
    client: DshClient,
    follow: DshStream,
    host_events: Option<HostEvents>,
}

/// Connect, open the follow stream, and publish `Connected`; `None` means the
/// attempt failed (already reported) or close was signalled. Every wait,
/// including error-path shutdown, honors the close signal.
async fn open_session(
    auth_url: &str,
    session_id: &str,
    config: DshClientConfig,
    outbox: &Outbox,
    close: &mut watch::Receiver<bool>,
) -> Option<OpenedSession> {
    let mut client = match tokio::select! {
        result = DshClient::connect(auth_url, config) => result,
        () = closed(close) => return None,
    } {
        Ok(client) => client,
        Err(error) => {
            close_with_reason(outbox, close, error).await;
            return None;
        }
    };
    let host_events = client.take_events();
    let follow = match tokio::select! {
        result = client.follow_with_limit(session_id, FOLLOW_MESSAGE_LIMIT) => result,
        () = closed(close) => return None,
    } {
        Ok(follow) => follow,
        Err(error) => {
            tokio::select! {
                () = SessionCommands::close(client) => {}
                () = closed(close) => {}
            }
            close_with_reason(outbox, close, error).await;
            return None;
        }
    };
    let client_id = client.client_id().to_owned();
    outbox
        .publish(TransportEvent::Connected { client_id }, close)
        .await
        .ok()?;
    Some(OpenedSession {
        client,
        follow,
        host_events,
    })
}

async fn close_with_reason(outbox: &Outbox, close: &mut watch::Receiver<bool>, error: ClientError) {
    let _ = outbox
        .publish(
            TransportEvent::Closed {
                reason: Some(error),
            },
            close,
        )
        .await;
}

async fn run(
    auth_url: &str,
    session_id: &str,
    config: DshClientConfig,
    commands: mpsc::Receiver<TransportCommand>,
    outbox: &Outbox,
    mut close: watch::Receiver<bool>,
) {
    let Some(opened) = open_session(auth_url, session_id, config, outbox, &mut close).await else {
        return;
    };
    let (rpc_sender, rpc_receiver) = mpsc::channel(COMMAND_CAPACITY);
    let worker = tokio::spawn(run_worker(
        opened.client,
        session_id.to_owned(),
        rpc_receiver,
        outbox.clone(),
        close.clone(),
    ));
    let reason = pump(
        opened.follow,
        opened.host_events,
        commands,
        rpc_sender.clone(),
        outbox,
        &mut close,
    )
    .await;
    shutdown_worker(worker, rpc_sender).await;
    let _ = outbox
        .publish(TransportEvent::Closed { reason }, &mut close)
        .await;
}

/// Forward frames, events, and commands without ever awaiting an RPC: command
/// execution lives only in the worker, by ownership.
async fn pump<F: FrameStream, E: EventStream>(
    mut follow: F,
    mut host_events: Option<E>,
    mut commands: mpsc::Receiver<TransportCommand>,
    rpc_sender: mpsc::Sender<TransportCommand>,
    outbox: &Outbox,
    close: &mut watch::Receiver<bool>,
) -> Option<ClientError> {
    let reason = loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    None => break None,
                    Some(command) => {
                        if let Err(full) = rpc_sender.try_send(command) {
                            let error = ClientError::CommandQueueFull;
                            let event = match full.into_inner() {
                                TransportCommand::Prompt { request_id, .. } => {
                                    TransportEvent::CommandFailed {
                                        operation: "session/prompt",
                                        error,
                                        request_id: Some(request_id),
                                    }
                                }
                                TransportCommand::Answer { .. } => TransportEvent::CommandFailed {
                                    operation: "$events/result",
                                    error,
                                    request_id: None,
                                },
                                TransportCommand::GitDiff { request_id, .. } => {
                                    TransportEvent::GitDiff { request_id, result: Err(error) }
                                }
                            };
                            if outbox.publish(event, close).await.is_err() {
                                break None;
                            }
                        }
                    }
                }
            }
            () = closed(close) => break None,
            frame = follow.next() => {
                match frame {
                    Ok(Some(frame)) => {
                        if outbox.publish(TransportEvent::FollowFrame(frame), close).await.is_err() {
                            break None;
                        }
                    }
                    Ok(None) => break None,
                    Err(error) => break Some(error),
                }
            }
            event = async {
                match &mut host_events {
                    Some(events) => events.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Ok(event) => {
                        if outbox.publish(TransportEvent::Host(event), close).await.is_err() {
                            break None;
                        }
                    }
                    Err(error) => break Some(error),
                }
            }
        }
    };
    follow.cancel();
    if let Some(events) = &mut host_events {
        events.cancel();
    }
    reason
}

/// Abort in-flight work so a slow RPC cannot delay shutdown, then wait for
/// the worker to release the client.
async fn shutdown_worker(
    worker: tokio::task::JoinHandle<()>,
    rpc_sender: mpsc::Sender<TransportCommand>,
) {
    drop(rpc_sender);
    worker.abort();
    let _ = worker.await;
}

/// Execute recorded intents sequentially, report failures without inferring
/// success, and close the owned client on the way out.
async fn run_worker<C: SessionCommands>(
    client: C,
    session_id: String,
    mut commands: mpsc::Receiver<TransportCommand>,
    outbox: Outbox,
    mut close: watch::Receiver<bool>,
) {
    loop {
        let command = tokio::select! {
            command = commands.recv() => command,
            () = closed(&mut close) => return,
        };
        let Some(command) = command else {
            break;
        };
        let event = match command {
            TransportCommand::Prompt { request_id, text } => client
                .prompt(&session_id, &request_id, &text)
                .await
                .err()
                .map(|error| TransportEvent::CommandFailed {
                    operation: "session/prompt",
                    error,
                    request_id: Some(request_id),
                }),
            TransportCommand::Answer { request, outcome } => {
                client.answer(&request, outcome).await.err().map(|error| {
                    TransportEvent::CommandFailed {
                        operation: "$events/result",
                        error,
                        request_id: None,
                    }
                })
            }
            TransportCommand::GitDiff {
                request_id,
                project_id,
            } => Some(TransportEvent::GitDiff {
                request_id,
                result: client.git_diff(&project_id).await,
            }),
        };
        if let Some(event) = event
            && outbox.publish(event, &mut close).await.is_err()
        {
            return;
        }
    }
    tokio::select! {
        () = client.close() => {}
        () = closed(&mut close) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use serde_json::json;

    use super::*;

    fn outbox_channels(
        capacity: usize,
    ) -> (Outbox, mpsc::Receiver<TransportEvent>, mpsc::Receiver<()>) {
        let (events, receiver) = mpsc::channel(capacity);
        let (wake, wake_receiver) = mpsc::channel(1);
        (Outbox { events, wake }, receiver, wake_receiver)
    }

    #[cfg(unix)]
    #[test]
    fn owned_homes_preserve_active_runs_and_recover_released_stale_locks() {
        let parent =
            std::env::temp_dir().join(format!("metis-owned-home-recovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        let active = OwnedHome::create(&parent).expect("create active home");
        let active_path = active.path.clone();
        let second = OwnedHome::create(&parent).expect("create second active home");
        assert!(active_path.exists());

        let mut stale = OwnedHome::create(&parent).expect("create stale fixture");
        let stale_path = stale.path.clone();
        let stale_lock = stale.lock_path.clone();
        fs::write(stale_path.join("stale-marker"), "abandoned").expect("write stale marker");
        // Process death releases the OS lock but cannot run Drop. Reproduce
        // that exact ownership state without spawning an unbounded child.
        stale.lock.take();
        std::mem::forget(stale);
        let recovered = OwnedHome::create(&parent).expect("recover stale home");
        assert!(!stale_path.join("stale-marker").exists());
        assert!(active_path.exists());

        drop(recovered);
        assert!(!stale_path.exists());
        assert!(!stale_lock.exists());
        drop(active);
        drop(second);
        fs::remove_dir_all(&parent).expect("remove recovery test parent");
    }

    #[tokio::test]
    async fn publish_wakes_once_per_burst_and_applies_backpressure() {
        let (outbox, mut receiver, mut wake) = outbox_channels(2);
        let (_close_sender, mut close) = watch::channel(false);
        for _ in 0..2 {
            outbox
                .publish(TransportEvent::Closed { reason: None }, &mut close)
                .await
                .expect("publish within capacity");
        }
        // One wake covers the whole burst; the queue holds every event.
        assert!(wake.try_recv().is_ok());
        assert!(wake.try_recv().is_err());

        // A full queue pends the publish instead of dropping the event.
        let pending = outbox.publish(TransportEvent::Closed { reason: None }, &mut close);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pending)
                .await
                .is_err(),
            "the third publish must wait for capacity"
        );
        assert!(receiver.try_recv().is_ok());
        outbox
            .publish(TransportEvent::Closed { reason: None }, &mut close)
            .await
            .expect("publish after draining one event");
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_ok());
    }

    #[tokio::test]
    async fn publish_stops_on_receiver_drop_and_close_signal() {
        let (outbox, receiver, _wake) = outbox_channels(1);
        let (_close_sender, mut close) = watch::channel(false);
        drop(receiver);
        assert!(
            outbox
                .publish(TransportEvent::Closed { reason: None }, &mut close)
                .await
                .is_err()
        );

        let (outbox, _receiver, _wake) = outbox_channels(1);
        let (close_sender, mut close) = watch::channel(false);
        outbox
            .publish(TransportEvent::Closed { reason: None }, &mut close)
            .await
            .expect("fill the queue");
        close_sender.send(true).expect("signal close");
        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                outbox.publish(TransportEvent::Closed { reason: None }, &mut close),
            )
            .await
            .expect("close interrupts a backpressured publish")
            .is_err()
        );
    }

    struct FakeFrames {
        frames: VecDeque<ClientResult<Option<Value>>>,
    }

    impl FrameStream for FakeFrames {
        async fn next(&mut self) -> ClientResult<Option<Value>> {
            // Pop only on poll: like DshStream::next, this future must stay
            // cancellation-safe when select! drops an unpolled branch.
            match self.frames.pop_front() {
                Some(frame) => frame,
                // No more frames: stay open like a live Host stream.
                None => std::future::pending().await,
            }
        }

        fn cancel(&mut self) {}
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<String>,
        closed: bool,
    }

    struct FakeCommands {
        state: Arc<Mutex<FakeState>>,
        prompt_error: bool,
        answer_error: bool,
        pending: bool,
    }

    impl FakeCommands {
        fn recording() -> (Self, Arc<Mutex<FakeState>>) {
            let state = Arc::new(Mutex::new(FakeState::default()));
            (
                Self {
                    state: Arc::clone(&state),
                    prompt_error: false,
                    answer_error: false,
                    pending: false,
                },
                state,
            )
        }

        fn pending() -> Self {
            let (mut commands, _) = Self::recording();
            commands.pending = true;
            commands
        }
    }

    impl SessionCommands for FakeCommands {
        fn prompt(
            &self,
            _session_id: &str,
            request_id: &str,
            _text: &str,
        ) -> impl std::future::Future<Output = ClientResult<()>> + Send {
            self.state
                .lock()
                .expect("fake state")
                .calls
                .push(format!("prompt:{request_id}"));
            let error = self.prompt_error;
            let pending = self.pending;
            async move {
                if pending {
                    std::future::pending::<()>().await;
                }
                if error {
                    Err(ClientError::Protocol("scripted prompt failure"))
                } else {
                    Ok(())
                }
            }
        }

        fn answer(
            &self,
            request: &ApprovalRequest,
            _outcome: ApprovalOutcome,
        ) -> impl std::future::Future<Output = ClientResult<()>> + Send {
            self.state
                .lock()
                .expect("fake state")
                .calls
                .push(format!("answer:{}", request.event_id));
            let error = self.answer_error;
            async move {
                if error {
                    Err(ClientError::Protocol("scripted answer failure"))
                } else {
                    Ok(())
                }
            }
        }

        fn git_diff(
            &self,
            project_id: &str,
        ) -> impl std::future::Future<Output = ClientResult<String>> + Send {
            self.state
                .lock()
                .expect("fake state")
                .calls
                .push(format!("git-diff:{project_id}"));
            async { Ok("diff --git a/a.txt b/a.txt".to_owned()) }
        }

        async fn close(self) {
            self.state.lock().expect("fake state").closed = true;
        }
    }

    fn prompt(request_id: &str) -> TransportCommand {
        TransportCommand::Prompt {
            request_id: request_id.to_owned(),
            text: "text".to_owned(),
        }
    }

    fn answer() -> TransportCommand {
        TransportCommand::Answer {
            request: ApprovalRequest {
                client_id: "client-a".to_owned(),
                event_id: "event-1".to_owned(),
                agent_id: "session-1".to_owned(),
                request: json!({ "callId": "call-1" }),
            },
            outcome: ApprovalOutcome::AllowedOnce,
        }
    }

    #[tokio::test]
    async fn worker_preserves_order_and_correlates_failures() {
        let (mut commands, state) = FakeCommands::recording();
        commands.prompt_error = true;
        commands.answer_error = true;
        let (command_sender, command_receiver) = mpsc::channel(8);
        let (outbox, mut events, _wake) = outbox_channels(8);
        let (_close_sender, close) = watch::channel(false);
        let worker = tokio::spawn(run_worker(
            commands,
            "session-1".to_owned(),
            command_receiver,
            outbox,
            close,
        ));

        for request_id in ["id-1", "id-2"] {
            command_sender
                .send(prompt(request_id))
                .await
                .expect("send prompt");
        }
        command_sender.send(answer()).await.expect("send answer");
        drop(command_sender);
        worker.await.expect("worker finishes after sender drop");

        let failures = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| match event {
                TransportEvent::CommandFailed {
                    operation,
                    request_id,
                    ..
                } => (operation, request_id),
                other => panic!("expected command failure, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            failures,
            [
                ("session/prompt", Some("id-1".to_owned())),
                ("session/prompt", Some("id-2".to_owned())),
                ("$events/result", None),
            ]
        );
        let state = state.lock().expect("fake state");
        assert_eq!(
            state.calls,
            ["prompt:id-1", "prompt:id-2", "answer:event-1"]
        );
        assert!(state.closed, "the worker closes the owned client");
    }

    #[tokio::test]
    async fn worker_returns_git_diff_with_its_request_identity() {
        let (commands, state) = FakeCommands::recording();
        let (command_sender, command_receiver) = mpsc::channel(2);
        let (outbox, mut events, _wake) = outbox_channels(2);
        let (_close_sender, close) = watch::channel(false);
        let worker = tokio::spawn(run_worker(
            commands,
            "session-1".to_owned(),
            command_receiver,
            outbox,
            close,
        ));
        command_sender
            .send(TransportCommand::GitDiff {
                request_id: 7,
                project_id: "project-1".to_owned(),
            })
            .await
            .expect("send git diff request");
        drop(command_sender);
        worker.await.expect("worker finishes");

        assert!(matches!(
            events.try_recv(),
            Ok(TransportEvent::GitDiff { request_id: 7, result: Ok(patch) })
                if patch.contains("diff --git")
        ));
        assert_eq!(
            state.lock().expect("fake state").calls,
            ["git-diff:project-1"]
        );
    }

    #[tokio::test]
    async fn slow_worker_neither_blocks_frames_nor_overflows_silently() {
        let (command_sender, command_receiver) = mpsc::channel(32);
        let (rpc_sender, rpc_receiver) = mpsc::channel(COMMAND_CAPACITY);
        let shutdown_sender = rpc_sender.clone();
        let (outbox, mut events, _wake) = outbox_channels(64);
        let (_close_sender, close) = watch::channel(false);
        let worker = tokio::spawn(run_worker(
            FakeCommands::pending(),
            "session-1".to_owned(),
            rpc_receiver,
            outbox.clone(),
            close.clone(),
        ));
        let follow = FakeFrames {
            frames: VecDeque::from([Ok(Some(json!({ "type": "snapshot" })))]),
        };
        let mut pump_close = close.clone();
        let pump_outbox = outbox.clone();
        let pump = tokio::spawn(async move {
            pump(
                follow,
                None::<HostEvents>,
                command_receiver,
                rpc_sender,
                &pump_outbox,
                &mut pump_close,
            )
            .await
        });

        // The worker is stuck in its first RPC, yet the follow frame flows.
        command_sender
            .send(prompt("blocking"))
            .await
            .expect("send blocking prompt");
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("follow frame arrives while the worker is blocked");
        match event.expect("follow frame event") {
            TransportEvent::FollowFrame(frame) => {
                assert_eq!(frame["type"], json!("snapshot"));
            }
            other => panic!("expected follow frame, got {other:?}"),
        }

        // Fill the worker queue while its RPC stays pending; the next command
        // is reported with its own request id instead of vanishing.
        for index in 0..COMMAND_CAPACITY {
            command_sender
                .send(prompt(&format!("queued-{index}")))
                .await
                .expect("fill worker queue");
        }
        command_sender
            .send(prompt("overflow"))
            .await
            .expect("send overflow prompt");
        let failure = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match events.recv().await {
                    Some(TransportEvent::CommandFailed {
                        operation,
                        error: ClientError::CommandQueueFull,
                        request_id,
                    }) => break (operation, request_id),
                    Some(_) => {}
                    None => panic!("event stream ended"),
                }
            }
        })
        .await
        .expect("queue-full report is not blocked by the stuck RPC");
        assert_eq!(failure, ("session/prompt", Some("overflow".to_owned())));

        // Aborting a stuck worker must not delay shutdown; the pump stops
        // once the intent sender is gone.
        drop(command_sender);
        tokio::time::timeout(
            Duration::from_secs(1),
            shutdown_worker(worker, shutdown_sender),
        )
        .await
        .expect("shutdown aborts the stuck worker");
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("pump stops after the command sender drops")
            .expect("pump task joins");
    }
}
