//! Supervision for the private dsh runtime used by a workbench.
//!
//! The Node Host assembles dsh through its public profile-boot API. Line-based
//! stdout events carry readiness without copying launch credentials into logs;
//! stdin carries application shutdown and doubles as the Host lifetime pipe.
//! Child stdout/stderr are drained by dedicated threads on both sides: the
//! watchdog forwards them upward, and the supervisor parses stdout for ready
//! while retaining all output for diagnostics.
//!
//! The watchdog is a hidden mode of the Metis executable. Its stdin is a
//! lifetime pipe: if the workbench disappears, EOF tells the watchdog to stop
//! the dsh process tree. Windows uses nested job objects so either the parent or
//! the watchdog can explicitly terminate the complete tree. Unix shares one
//! process group; dsh gets a grace period to clean up its deliberately detached
//! systemd scopes before the shared group receives its final signal. The Host
//! PID also identifies those scopes, so either layer can reclaim them when Host
//! disposal fails or the watchdog itself must be killed.

use std::{
    collections::BTreeSet,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use process_wrap::std::{ChildWrapper, CommandWrap};

use crate::{Error, Result};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const DSH_GRACE_PERIOD: Duration = Duration::from_secs(10);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const DSH_EXIT_STATUS_FILE_NAME: &str = "dsh-exit-status";
#[cfg(target_os = "linux")]
use std::process::Output;

#[cfg(target_os = "linux")]
const COMMAND_GROUP_OWNER_ENV: &str = "METIS_COMMAND_GROUP_OWNER";
#[cfg(target_os = "linux")]
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const SCOPE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const SCOPE_CLEANUP_POLL: Duration = Duration::from_millis(50);
#[doc(hidden)]
pub const WATCHDOG_COMMAND: &str = "__dsh-watchdog";

/// Paths and timeouts needed to start an isolated, extended dsh runtime.
#[derive(Clone, Debug)]
pub struct DshRuntimeConfig {
    /// Metis executable used for the hidden watchdog mode.
    pub metis_executable: PathBuf,
    /// Pinned Node.js executable.
    pub node_executable: PathBuf,
    /// Built JavaScript entry point of the private Metis Host; the caller owns
    /// its packaged location.
    pub host_entrypoint: PathBuf,
    /// Per-runtime dsh state directory, passed as `DSH_HOME`. The Host creates
    /// its profile here, and the watchdog records an early Host exit beside it.
    pub home: PathBuf,
    /// Maximum time to wait for the Host's authenticated ready event.
    pub startup_timeout: Duration,
    /// Maximum time to wait for the watchdog to clean up dsh on shutdown.
    /// Normal configurations should exceed the watchdog's internal grace period;
    /// shorter values are useful to exercise forced escalation.
    pub shutdown_timeout: Duration,
}

/// Starts private dsh runtimes using injected platform services.
pub struct DshSupervisor {
    platform: Arc<dyn RuntimePlatform>,
}

impl DshSupervisor {
    pub fn new(platform: Arc<dyn RuntimePlatform>) -> Self {
        Self { platform }
    }

    #[must_use]
    pub fn system() -> Self {
        Self::new(Arc::new(SystemPlatform))
    }

    pub fn start(&self, config: &DshRuntimeConfig) -> Result<DshRuntime> {
        self.platform
            .create_dir_all(&config.home)
            .map_err(|source| Error::RuntimePrepare {
                path: config.home.clone(),
                source,
            })?;

        let dsh_exit_status = config.home.join(DSH_EXIT_STATUS_FILE_NAME);
        let _ = self.platform.remove_file(&dsh_exit_status);

        let mut command = Command::new(&config.metis_executable);
        command
            .arg(WATCHDOG_COMMAND)
            .arg("--node")
            .arg(&config.node_executable)
            .arg("--host")
            .arg(&config.host_entrypoint)
            .arg("--home")
            .arg(&config.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = self
            .platform
            .spawn(command, ProcessContainment::OwnedTree)
            .map_err(|source| Error::RuntimeSpawn {
                program: config.metis_executable.clone(),
                source,
            })?;
        let id = child.id();
        let watchdog_identity = match host_identity(self.platform.as_ref(), id) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = child.start_kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout().take(), child.stderr().take()) else {
            if let Err(error) = child.start_kill() {
                tracing::warn!(%error, "failed to stop a watchdog after output capture failed");
            } else if let Err(error) = child.wait() {
                tracing::warn!(%error, "failed to reap a watchdog after output capture failed");
            }
            return Err(Error::RuntimeOutputCapture);
        };
        let output = Arc::new(Mutex::new(String::new()));
        let (host_events, events) = mpsc::sync_channel(4);
        let mut runtime = DshRuntime {
            id,
            watchdog_identity,
            host_identity: None,
            pending_scope_cleanup: None,
            watchdog_cleanup_done: false,
            web_url: None,
            auth_url: None,
            child: Some(child),
            output: Arc::clone(&output),
            readers: Vec::new(),
            events,
            platform: Arc::clone(&self.platform),
            shutdown_timeout: config.shutdown_timeout,
        };
        // Construct the cleanup guard before spawning reader threads: a thread
        // creation panic must not strand the already-spawned runtime.
        runtime.readers.push(
            spawn_host_reader(stdout, Arc::clone(&output), "dsh-host-stdout", host_events)
                .map_err(|_| Error::RuntimeOutputCapture)?,
        );
        runtime.readers.push(
            spawn_output_reader(stderr, output, "dsh-stderr")
                .map_err(|_| Error::RuntimeOutputCapture)?,
        );

        if let Err(error) = runtime.wait_until_ready(config) {
            if let Err(cleanup_error) = runtime.shutdown() {
                tracing::warn!(
                    %error,
                    %cleanup_error,
                    "dsh startup failed and its runtime cleanup also failed"
                );
            }
            // Cleanup closes the output pipes and gives reader threads a chance
            // to append the actual startup failure before it is reported.
            let output = runtime.output_snapshot();
            return Err(match error {
                Error::RuntimeStartupExit { status, .. } => {
                    Error::RuntimeStartupExit { status, output }
                }
                Error::RuntimeStartupTimeout { timeout, .. } => {
                    Error::RuntimeStartupTimeout { timeout, output }
                }
                Error::RuntimeStartupProtocol { message, .. } => {
                    Error::RuntimeStartupProtocol { message, output }
                }
                error => error,
            });
        }
        Ok(runtime)
    }
}

/// A running private dsh instance. Dropping it performs the same cleanup as
/// [`DshRuntime::shutdown`].
pub struct DshRuntime {
    id: u32,
    watchdog_identity: HostIdentity,
    host_identity: Option<HostIdentity>,
    pending_scope_cleanup: Option<PendingScopeCleanup>,
    watchdog_cleanup_done: bool,
    web_url: Option<String>,
    auth_url: Option<String>,
    child: Option<Box<dyn ChildWrapper>>,
    output: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
    events: mpsc::Receiver<std::result::Result<HostControl, String>>,
    platform: Arc<dyn RuntimePlatform>,
    shutdown_timeout: Duration,
}

impl DshRuntime {
    /// PID of the watchdog, not the Node.js process.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The local dsh Host root.
    #[must_use]
    pub fn web_url(&self) -> &str {
        self.web_url.as_deref().expect("runtime is ready")
    }

    /// Public dsh authentication URL for this Host instance.
    #[must_use]
    pub fn auth_url(&self) -> &str {
        self.auth_url.as_deref().expect("runtime is ready")
    }

    /// Recent combined stdout/stderr for diagnostics. The buffer is bounded;
    /// this is not a durable log or an event stream.
    #[must_use]
    pub fn output_snapshot(&self) -> String {
        lock_output(&self.output).clone()
    }

    /// Stop the supervised process group/job and wait for its watchdog. Host
    /// disposal runs first; failed scope cleanup remains retryable after the
    /// final process-group/job kill.
    pub fn shutdown(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return self.cleanup_pending_scopes();
        };

        // Closing the lifetime pipe asks the watchdog to signal dsh and clean
        // its tree. The outer group/job is also killed below as a backstop and
        // to cover descendants which outlived an already-exited watchdog.
        drop(child.stdin().take());
        let deadline = self.platform.now() + self.shutdown_timeout;
        let mut timed_out = false;
        let mut monitor_error = None;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    monitor_error = Some(error);
                    break;
                }
            }
            let now = self.platform.now();
            if now >= deadline {
                timed_out = true;
                break;
            }
            self.platform.sleep(POLL_INTERVAL.min(deadline - now));
        }

        // The watchdog normally leaves an empty Unix group, for which killpg
        // reports POSIX ESRCH (3). Any other failure is actionable: do not
        // pretend that a still-running runtime was cleaned up.
        if let Err(error) = child.start_kill() {
            let already_stopped = cfg!(unix) && error.raw_os_error() == Some(3);
            if !already_stopped {
                self.child = Some(child);
                return Err(Error::RuntimeKill(error));
            }
        }
        if let Err(error) = child.wait() {
            self.child = Some(child);
            return Err(Error::RuntimeWait(error));
        }
        // Once the watchdog is reaped it cannot spawn another cleanup command;
        // only now is a single detached-command sweep sufficient.
        let command_group_cleanup = kill_detached_command_groups(self.watchdog_identity).err();
        // dsh and its descendants only inherit watchdog-owned pipes. Once the
        // watchdog is reaped, the outer readers have finite input and can drain
        // all remaining diagnostics without waiting for escaped descendants.
        self.finish_readers();

        // Only an explicit handoff proves scope cleanup. In particular, the
        // watchdog's own Unix group kill makes its exit status non-successful.
        while let Ok(event) = self.events.try_recv() {
            match event {
                Ok(HostControl::CleanupDone) => self.watchdog_cleanup_done = true,
                Ok(HostControl::HostIdentity(identity)) => {
                    self.host_identity.get_or_insert(identity);
                }
                _ => {}
            }
        }
        let scope_cleanup_required =
            self.pending_scope_cleanup.is_some() || !self.watchdog_cleanup_done;
        if scope_cleanup_required
            && self.pending_scope_cleanup.is_none()
            && let Some(identity) = self.host_identity
        {
            self.pending_scope_cleanup = Some(PendingScopeCleanup {
                identity,
                units: None,
            });
        }
        if scope_cleanup_required {
            self.cleanup_pending_scopes()?;
        }
        if let Some(error) = command_group_cleanup {
            return Err(error);
        }
        if let Some(error) = monitor_error {
            return Err(Error::RuntimeWait(error));
        }
        if timed_out {
            return Err(Error::RuntimeShutdownTimeout(self.shutdown_timeout));
        }
        Ok(())
    }

    fn cleanup_pending_scopes(&mut self) -> Result<()> {
        let Some(pending) = self.pending_scope_cleanup.as_mut() else {
            return Ok(());
        };
        let platform = Arc::clone(&self.platform);
        if pending.units.is_none() {
            pending.units = Some(discover_systemd_scopes(
                platform.as_ref(),
                pending.identity,
            )?);
        }
        let result = cleanup_scopes(
            platform.as_ref(),
            pending
                .units
                .as_ref()
                .expect("scope snapshot was initialized above"),
        );
        if result.is_ok() {
            self.pending_scope_cleanup = None;
        }
        result
    }

    fn wait_until_ready(&mut self, config: &DshRuntimeConfig) -> Result<()> {
        let deadline = self.platform.now() + config.startup_timeout;
        let mut control_closed = false;
        loop {
            if !control_closed {
                match self.events.try_recv() {
                    Ok(Ok(HostControl::HostIdentity(identity))) => {
                        if self.host_identity.is_some() {
                            return Err(Error::RuntimeStartupProtocol {
                                message: "the Host sent duplicate identity events".to_owned(),
                                output: self.output_snapshot(),
                            });
                        }
                        self.host_identity = Some(identity);
                    }
                    Ok(Ok(HostControl::CleanupDone)) => {
                        self.watchdog_cleanup_done = true;
                    }
                    Ok(Ok(HostControl::Ready(HostReady { web_url, auth_url }))) => {
                        self.web_url = Some(web_url);
                        self.auth_url = Some(auth_url);
                        return Ok(());
                    }

                    Ok(Err(message)) => {
                        return Err(Error::RuntimeStartupProtocol {
                            message,
                            output: self.output_snapshot(),
                        });
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => control_closed = true,
                }
            }
            // stdout EOF can become visible before waitpid reports the Host's
            // exit. Always give process status priority over a protocol error.
            if let Some(status) = self
                .child
                .as_mut()
                .expect("runtime child present during startup")
                .try_wait()
                .map_err(Error::RuntimeWait)?
            {
                let status = self
                    .platform
                    .read(&config.home.join(DSH_EXIT_STATUS_FILE_NAME))
                    .map_or_else(
                        |_| status.to_string(),
                        |status| String::from_utf8_lossy(&status).into_owned(),
                    );
                return Err(Error::RuntimeStartupExit {
                    status,
                    output: self.output_snapshot(),
                });
            }

            let now = self.platform.now();
            if now >= deadline {
                if control_closed {
                    return Err(Error::RuntimeStartupProtocol {
                        message: "the Host closed its control output before becoming ready"
                            .to_owned(),
                        output: self.output_snapshot(),
                    });
                }
                return Err(Error::RuntimeStartupTimeout {
                    timeout: config.startup_timeout,
                    output: self.output_snapshot(),
                });
            }
            self.platform.sleep(POLL_INTERVAL.min(deadline - now));
        }
    }

    fn finish_readers(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl Drop for DshRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::warn!(%error, "failed to stop the private dsh runtime during drop");
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HostIdentity {
    pid: u32,
    start_time: u64,
}

struct PendingScopeCleanup {
    identity: HostIdentity,
    units: Option<ScopeSnapshot>,
}

enum HostControl {
    HostIdentity(HostIdentity),
    Ready(HostReady),
    CleanupDone,
}

struct HostReady {
    web_url: String,
    auth_url: String,
}

/// Clock, filesystem, and process creation used by runtime supervision.
/// Production uses [`SystemPlatform`]; tests can inject deterministic clocks or
/// filesystem failures without global state.
pub trait RuntimePlatform: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    fn spawn(
        &self,
        command: Command,
        containment: ProcessContainment,
    ) -> io::Result<Box<dyn ChildWrapper>>;
}

/// Whether the new process is the outer watchdog or its dsh child. Unix places
/// both in the watchdog's group using std's safe `process_group`; Windows nests
/// a dsh job inside the watchdog job so both sides can terminate the whole tree.
#[derive(Clone, Copy, Debug)]
pub enum ProcessContainment {
    OwnedTree,
    ChildTree,
}

/// Real operating-system services for runtime supervision.
pub struct SystemPlatform;

impl RuntimePlatform for SystemPlatform {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        std::fs::write(path, contents)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn spawn(
        &self,
        command: Command,
        containment: ProcessContainment,
    ) -> io::Result<Box<dyn ChildWrapper>> {
        #[cfg(unix)]
        {
            match containment {
                ProcessContainment::OwnedTree => CommandWrap::from(command)
                    .wrap(process_wrap::std::ProcessGroup::leader())
                    .spawn(),
                ProcessContainment::ChildTree => {
                    use std::os::unix::process::CommandExt as _;

                    let mut command = command;
                    let group = i32::try_from(std::process::id()).map_err(io::Error::other)?;
                    command.process_group(group);
                    let child = command.spawn()?;
                    Ok(Box::new(child))
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = containment;
            CommandWrap::from(command)
                .wrap(process_wrap::std::JobObject)
                .spawn()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = containment;
            let child = command.spawn()?;
            Ok(Box::new(child))
        }
    }
}

/// Arguments for the binary's hidden watchdog mode. Not a stable user-facing
/// interface; the supervisor and watchdog are always shipped together.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct WatchdogConfig {
    pub node_executable: PathBuf,
    pub host_entrypoint: PathBuf,
    pub home: PathBuf,
}

/// Run the Host side of the lifetime pipe. Called only by the hidden binary
/// subcommand, never by the workbench in-process.
#[doc(hidden)]
pub fn run_watchdog(config: &WatchdogConfig) -> Result<()> {
    let platform = SystemPlatform;
    let mut command = Command::new(&config.node_executable);
    command
        .arg(&config.host_entrypoint)
        .env("DSH_HOME", &config.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = platform
        .spawn(command, ProcessContainment::ChildTree)
        .map_err(|source| Error::RuntimeSpawn {
            program: config.node_executable.clone(),
            source,
        })?;

    let identity = match host_identity(&platform, child.id()) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = force_stop_dsh(child.as_mut());
            return Err(error);
        }
    };
    if write_host_identity(identity).is_err() {
        let _ = force_stop_dsh(child.as_mut());
        return Err(Error::RuntimeOutputCapture);
    }

    let (Some(stdout), Some(stderr)) = (child.stdout().take(), child.stderr().take()) else {
        if let Err(error) = force_stop_dsh(child.as_mut()) {
            tracing::warn!(%error, "failed to stop dsh after output capture failed");
        }
        return Err(Error::RuntimeOutputCapture);
    };
    let mut forwarders = match spawn_output_forwarders(stdout, stderr) {
        Ok(forwarders) => forwarders,
        Err(error) => {
            tracing::warn!(%error, "failed to spawn dsh output forwarding");
            if let Err(cleanup_error) = force_stop_dsh(child.as_mut()) {
                tracing::warn!(%cleanup_error, "dsh cleanup also failed after output forwarding failed");
            }
            return Err(Error::RuntimeOutputCapture);
        }
    };

    let (parent_events, parent_closed) = mpsc::channel();
    thread::spawn(move || {
        parent_events.send(read_parent_pipe()).ok();
    });

    match monitor_dsh(child.as_mut(), &platform, &parent_closed) {
        Ok(None) => {
            let graceful_result = terminate_dsh(child.as_mut(), &platform);
            if let Err(error) = &graceful_result {
                tracing::warn!(%error, "graceful dsh shutdown failed; escalating to a group kill");
            }
            let cleanup_result = cleanup_dsh(child.as_mut(), &platform, &mut forwarders, identity);
            graceful_result?;
            cleanup_result
        }
        Ok(Some(status)) => {
            // The record is closed before cleanup can terminate the watchdog,
            // so the supervisor can recover dsh's status after reaping it.
            publish_dsh_exit_status(&platform, config, status);
            cleanup_dsh(child.as_mut(), &platform, &mut forwarders, identity)?;

            if status.success() {
                Ok(())
            } else {
                Err(Error::RuntimeDshExit(status))
            }
        }
        Err(error) => {
            // Unix's successful group kill prevents the normal error return,
            // so leave the cause in the captured output before terminating.
            #[cfg(unix)]
            tracing::warn!(%error, "watchdog failed; terminating the dsh process group");
            // Once dsh has spawned, no watchdog error may bypass containment.
            if let Err(cleanup_error) =
                cleanup_dsh(child.as_mut(), &platform, &mut forwarders, identity)
            {
                tracing::warn!(%error, %cleanup_error, "watchdog failed and dsh cleanup also failed");
            }
            Err(error)
        }
    }
}

fn publish_dsh_exit_status(platform: &SystemPlatform, config: &WatchdogConfig, status: ExitStatus) {
    let path = config.home.join(DSH_EXIT_STATUS_FILE_NAME);
    if let Err(error) = platform.write(&path, status.to_string().as_bytes()) {
        // A diagnostics failure must never skip process-tree cleanup.
        tracing::warn!(%error, path = %path.display(), "failed to record the dsh exit status");
    }
}

fn spawn_output_forwarders(
    stdout: impl Read + Send + 'static,
    stderr: impl Read + Send + 'static,
) -> io::Result<Vec<JoinHandle<()>>> {
    Ok(vec![
        spawn_output_forwarder(stdout, io::stdout(), "dsh-stdout-forwarder")?,
        spawn_output_forwarder(stderr, io::stderr(), "dsh-stderr-forwarder")?,
    ])
}

fn spawn_output_forwarder(
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    name: &'static str,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || forward_output(reader, writer))
}

fn forward_output(mut reader: impl Read, mut writer: impl Write) {
    let mut buffer = [0_u8; 8192];
    let mut sink_open = true;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                // If the supervisor disappeared, keep draining dsh's pipe so
                // output backpressure cannot stall its signal handler or exit.
                if sink_open
                    && writer
                        .write_all(&buffer[..read])
                        .and_then(|()| writer.flush())
                        .is_err()
                {
                    sink_open = false;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn drain_output_forwarders(readers: &mut Vec<JoinHandle<()>>, platform: &SystemPlatform) {
    let deadline = platform.now() + OUTPUT_DRAIN_GRACE;
    loop {
        if readers.iter().all(JoinHandle::is_finished) {
            break;
        }
        let now = platform.now();
        if now >= deadline {
            break;
        }
        platform.sleep(POLL_INTERVAL.min(deadline - now));
    }
    for reader in readers.drain(..) {
        // Joining a live forwarder could block forever while a descendant holds
        // an inner pipe; the short-lived watchdog exits after this bounded drain.
        if reader.is_finished() {
            let _ = reader.join();
        }
    }
}

fn cleanup_dsh(
    child: &mut dyn ChildWrapper,
    platform: &SystemPlatform,
    forwarders: &mut Vec<JoinHandle<()>>,
    identity: HostIdentity,
) -> Result<()> {
    // Freeze the producer before taking the unit snapshot. This closes both the
    // PID-reuse window and the race where Host creates another scope mid-cleanup.
    let units = stop_host_and_collect_scopes(child, platform, identity);
    // Unix's final group kill also terminates this watchdog, so drain first.
    // Windows closes every inner pipe in force_stop_dsh and can drain afterwards.
    #[cfg(unix)]
    drain_output_forwarders(forwarders, platform);
    let scope_cleanup = units.and_then(|units| cleanup_scopes(platform, &units));
    let control_result = if scope_cleanup.is_ok() {
        write_cleanup_done().map_err(|_| Error::RuntimeOutputCapture)
    } else {
        Ok(())
    };
    let result = force_stop_dsh(child);
    #[cfg(windows)]
    drain_output_forwarders(forwarders, platform);
    scope_cleanup.and(control_result).and(result)
}

fn monitor_dsh(
    child: &mut dyn ChildWrapper,
    platform: &SystemPlatform,
    parent_closed: &mpsc::Receiver<io::Result<()>>,
) -> Result<Option<ExitStatus>> {
    loop {
        match parent_closed.try_recv() {
            Ok(Ok(())) | Err(mpsc::TryRecvError::Disconnected) => return Ok(None),
            Ok(Err(error)) => return Err(Error::RuntimeParentPipe(error)),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(status) = child.try_wait().map_err(Error::RuntimeWait)? {
            return Ok(Some(status));
        }
        platform.sleep(POLL_INTERVAL);
    }
}

fn terminate_dsh(child: &mut dyn ChildWrapper, platform: &SystemPlatform) -> Result<()> {
    // The Host disposes dsh's root synchronously before exiting; this includes
    // its deliberately detached systemd scopes. Closing stdin
    // after the command also covers a Host which has not finished booting.
    if let Some(stdin) = child.stdin().as_mut()
        && let Err(error) = stdin.write_all(b"METIS_SHUTDOWN\n")
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(Error::RuntimeShutdownRequest(error));
    }
    drop(child.stdin().take());

    // dsh's own disposal deadline is five seconds. Leave room for that deadline
    // and process exit before escalating at the process-tree layer.
    let deadline = platform.now() + DSH_GRACE_PERIOD;
    let mut wait_result = Ok(());
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                wait_result = Err(error);
                break;
            }
        }
        let now = platform.now();
        if now >= deadline {
            break;
        }
        platform.sleep(POLL_INTERVAL.min(deadline - now));
    }

    wait_result.map_err(Error::RuntimeWait)
}

fn force_stop_dsh(child: &mut dyn ChildWrapper) -> Result<()> {
    #[cfg(unix)]
    {
        let _ = child;
        let detached = host_identity(&SystemPlatform, std::process::id())
            .and_then(kill_detached_command_groups);
        let group = i32::try_from(std::process::id())
            .map_err(io::Error::other)
            .map_err(Error::RuntimeKill)?;
        let group = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        )
        .map_err(io::Error::from)
        .map_err(Error::RuntimeKill);
        detached.and(group)
    }
    #[cfg(windows)]
    {
        child.start_kill().map_err(Error::RuntimeKill)?;
        child.wait().map_err(Error::RuntimeWait)?;
        Ok(())
    }
}

fn kill_detached_command_groups(owner: HostIdentity) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let root = i32::try_from(owner.pid)
            .map_err(io::Error::other)
            .map_err(Error::RuntimeKill)?;
        let marker = format!(
            "{COMMAND_GROUP_OWNER_ENV}={}",
            command_group_owner_value(owner)
        );
        let mut groups = BTreeSet::new();
        for path in std::fs::read_dir("/proc").map_err(Error::RuntimeKill)? {
            let path = path.map_err(Error::RuntimeKill)?.path();
            let Ok(stat) = std::fs::read(path.join("stat")) else {
                continue;
            };
            let Some((_, group)) = process_relation_from_stat(&stat) else {
                continue;
            };
            if group <= 0 || group == root || groups.contains(&group) {
                continue;
            }
            // PPID is not ownership: orphaned commands are re-parented as soon
            // as their watchdog dies. The inherited marker survives that handoff.
            if std::fs::read(path.join("environ")).is_ok_and(|environment| {
                environment
                    .split(|byte| *byte == 0)
                    .any(|entry| entry == marker.as_bytes())
            }) {
                groups.insert(group);
            }
        }
        let mut failure = None;
        for group in groups {
            if let Err(error) = kill_command_group(group)
                && failure.is_none()
            {
                failure = Some(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = owner;
    Ok(())
}

#[cfg(target_os = "linux")]
fn command_group_owner_value(owner: HostIdentity) -> String {
    format!("{}:{}", owner.pid, owner.start_time)
}

#[cfg(target_os = "linux")]
fn kill_command_group(group: i32) -> Result<()> {
    match nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(group),
        nix::sys::signal::Signal::SIGKILL,
    ) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(Error::RuntimeKill(io::Error::from(error))),
    }
}

#[cfg(target_os = "linux")]
fn process_relation_from_stat(stat: &[u8]) -> Option<(i32, i32)> {
    let stat = String::from_utf8_lossy(stat);
    let end = stat.rfind(')')?;
    let mut fields = stat.get(end + 2..)?.split_whitespace();
    let parent = fields.nth(1)?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    Some((parent, group))
}

fn host_identity(platform: &dyn RuntimePlatform, pid: u32) -> Result<HostIdentity> {
    #[cfg(target_os = "linux")]
    {
        Ok(HostIdentity {
            pid,
            start_time: process_start_time(platform, pid)?,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = platform;
        Ok(HostIdentity { pid, start_time: 0 })
    }
}

fn write_host_identity(identity: HostIdentity) -> io::Result<()> {
    let mut output = io::stdout().lock();
    writeln!(
        output,
        "METIS_HOST_PID\t{}\t{}",
        identity.pid, identity.start_time
    )?;
    output.flush()
}

fn write_cleanup_done() -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(b"METIS_CLEANUP_DONE\n")?;
    output.flush()
}

fn stop_host_and_collect_scopes(
    child: &mut dyn ChildWrapper,
    platform: &dyn RuntimePlatform,
    identity: HostIdentity,
) -> Result<ScopeSnapshot> {
    #[cfg(target_os = "linux")]
    {
        verify_host_identity(platform, identity)?;
        let running = child.try_wait().map_err(Error::RuntimeWait)?.is_none();
        let stopped = running && signal_host(identity.pid, nix::sys::signal::Signal::SIGSTOP)?;
        let units = discover_systemd_scopes(platform, identity);
        let killed = if stopped {
            match verify_host_identity(platform, identity) {
                Ok(()) => signal_host(identity.pid, nix::sys::signal::Signal::SIGKILL).map(|_| ()),
                Err(error) => {
                    // If the PID changed after the first check, do not kill the
                    // replacement process; resume it and leave its scopes alone.
                    let _ = signal_host(identity.pid, nix::sys::signal::Signal::SIGCONT);
                    Err(error)
                }
            }
        } else {
            Ok(())
        };
        match (units, killed) {
            (Ok(units), Ok(())) => Ok(units),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (child, platform, identity);
        Ok(BTreeSet::new())
    }
}

type ScopeSnapshot = BTreeSet<String>;

fn discover_systemd_scopes(
    platform: &dyn RuntimePlatform,
    identity: HostIdentity,
) -> Result<ScopeSnapshot> {
    #[cfg(target_os = "linux")]
    {
        verify_host_identity(platform, identity)?;
        let prefixes = [
            format!("dsh-subprocess-{}-", identity.pid),
            format!("dsh-subprocess-probe-{}-", identity.pid),
            format!("dsh-terminal-{}-", identity.pid),
        ];
        let snapshot = scan_scopes(platform, |unit| {
            prefixes.iter().any(|prefix| {
                unit.strip_prefix(prefix).is_some_and(|suffix| {
                    suffix.strip_suffix(".scope").is_some_and(|name| {
                        name.len() == 12
                            && name.chars().all(|character| character.is_ascii_hexdigit())
                    })
                })
            })
        })?;
        // The Host may have exited before the first check. Verify again after
        // reading /proc so a reused PID cannot add a replacement Host's scopes.
        verify_host_identity(platform, identity)?;
        Ok(snapshot)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (platform, identity);
        Ok(BTreeSet::new())
    }
}

fn cleanup_scopes(platform: &dyn RuntimePlatform, snapshot: &ScopeSnapshot) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if snapshot.is_empty() {
            return Ok(());
        }
        let deadline = platform.now() + SCOPE_CLEANUP_TIMEOUT;
        loop {
            // systemd signals the current unit membership, so a fork or PID
            // reuse after discovery cannot turn this into a signal for an
            // unrelated process. Repeat until no member remains in the units.
            let mut kill_args = vec!["--user", "kill", "--kill-whom=all", "--signal=SIGKILL"];
            kill_args.extend(snapshot.iter().map(String::as_str));
            run_systemctl_scope_command(platform, &kill_args)?;
            let remaining = scan_scopes(platform, |unit| snapshot.contains(unit))?;
            if remaining.is_empty() {
                break;
            }
            if platform.now() >= deadline {
                return Err(scope_cleanup_error(format!(
                    "scoped processes are still running after {SCOPE_CLEANUP_TIMEOUT:?}: {remaining:?}"
                )));
            }
            platform.sleep(SCOPE_CLEANUP_POLL);
        }
        let mut stop_args = vec!["--user", "stop"];
        stop_args.extend(snapshot.iter().map(String::as_str));
        run_systemctl_scope_command(platform, &stop_args)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (platform, snapshot);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn scan_scopes(
    platform: &dyn RuntimePlatform,
    predicate: impl Fn(&str) -> bool,
) -> Result<ScopeSnapshot> {
    let mut snapshot = ScopeSnapshot::new();
    for path in platform
        .read_dir(Path::new("/proc"))
        .map_err(|error| scope_cleanup_error(format!("read /proc: {error}")))?
    {
        let Some(_pid) = path
            .file_name()
            .and_then(|name| name.to_string_lossy().parse::<u32>().ok())
        else {
            continue;
        };
        let cgroup = match platform.read(&path.join("cgroup")) {
            Ok(cgroup) => cgroup,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(scope_cleanup_error(format!(
                    "read {}: {error}",
                    path.display()
                )));
            }
        };
        let Some(unit) = scope_unit_from_cgroup(&cgroup) else {
            continue;
        };
        if !predicate(unit) {
            continue;
        }
        snapshot.insert(unit.to_owned());
    }
    Ok(snapshot)
}

#[cfg(target_os = "linux")]
fn scope_unit_from_cgroup(cgroup: &[u8]) -> Option<&str> {
    std::str::from_utf8(cgroup)
        .ok()?
        .lines()
        .find_map(|line| line.split_once("::/").map(|(_, path)| path))
        .and_then(|path| path.rsplit('/').next())
        .filter(|unit| unit.strip_suffix(".scope").is_some())
}

#[cfg(target_os = "linux")]
fn scope_missing(output: &Output) -> bool {
    let details = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    details.contains("could not be found")
        || details.contains("not found")
        || details.contains("not loaded")
        || details.contains("NoSuchUnit")
}

#[cfg(target_os = "linux")]
fn process_start_time(platform: &dyn RuntimePlatform, pid: u32) -> Result<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = platform
        .read(&path)
        .map_err(|error| scope_cleanup_error(format!("read {}: {error}", path.display())))?;
    process_start_time_from_stat(&stat)
        .ok_or_else(|| scope_cleanup_error(format!("parse {}", path.display())))
}

#[cfg(target_os = "linux")]
fn process_start_time_from_stat(stat: &[u8]) -> Option<u64> {
    let stat = String::from_utf8_lossy(stat);
    let end = stat.rfind(')')?;
    stat.get(end + 2..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
fn verify_host_identity(platform: &dyn RuntimePlatform, identity: HostIdentity) -> Result<()> {
    let path = PathBuf::from(format!("/proc/{}/stat", identity.pid));
    match platform.read(&path) {
        Ok(stat) => {
            let actual = process_start_time_from_stat(&stat)
                .ok_or_else(|| scope_cleanup_error(format!("parse {}", path.display())))?;
            if actual == identity.start_time {
                Ok(())
            } else {
                Err(scope_cleanup_error(format!(
                    "Host PID {} was reused before scope cleanup",
                    identity.pid
                )))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(scope_cleanup_error(format!(
            "read {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(target_os = "linux")]
fn signal_host(pid: u32, signal: nix::sys::signal::Signal) -> Result<bool> {
    let pid = i32::try_from(pid)
        .map_err(io::Error::other)
        .map_err(Error::RuntimeKill)?;
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
        Ok(()) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(Error::RuntimeKill(io::Error::from(error))),
    }
}

#[cfg(target_os = "linux")]
fn run_systemctl(platform: &dyn RuntimePlatform, args: &[&str]) -> Result<Option<Output>> {
    run_systemctl_with_timeout(platform, "systemctl", args, SYSTEMCTL_TIMEOUT)
}

#[cfg(target_os = "linux")]
fn run_systemctl_scope_command(platform: &dyn RuntimePlatform, args: &[&str]) -> Result<()> {
    let output = run_systemctl(platform, args)?.ok_or_else(|| {
        scope_cleanup_error("systemctl is required to release scoped units".to_owned())
    })?;
    if output.status.success() || scope_missing(&output) {
        Ok(())
    } else {
        Err(scope_cleanup_error(format!(
            "systemctl {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

#[cfg(target_os = "linux")]
fn spawn_command_output_reader(
    mut reader: impl Read + Send + 'static,
) -> io::Result<JoinHandle<io::Result<Vec<u8>>>> {
    thread::Builder::new()
        .name("systemctl-output".to_owned())
        .spawn(move || {
            let mut output = Vec::new();
            reader.read_to_end(&mut output).map(|_| output)
        })
}

#[cfg(target_os = "linux")]
fn run_systemctl_with_timeout(
    platform: &dyn RuntimePlatform,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Option<Output>> {
    let owner = host_identity(platform, std::process::id())?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .env(COMMAND_GROUP_OWNER_ENV, command_group_owner_value(owner))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Each command gets its own process group so timeout escalation covers its
    // descendants; kill_detached_command_groups covers watchdog death as well.
    let mut child = match platform.spawn(command, ProcessContainment::OwnedTree) {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(scope_cleanup_error(format!("spawn systemctl: {error}")));
        }
    };
    let (Some(stdout), Some(stderr)) = (child.stdout().take(), child.stderr().take()) else {
        let _ = child.start_kill();
        let _ = child.wait();
        return Err(scope_cleanup_error(
            "could not capture systemctl output".to_owned(),
        ));
    };
    let readers: io::Result<Vec<JoinHandle<io::Result<Vec<u8>>>>> = (|| {
        Ok(vec![
            spawn_command_output_reader(stdout)?,
            spawn_command_output_reader(stderr)?,
        ])
    })();
    let mut readers = match readers {
        Ok(readers) => readers,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait();
            return Err(scope_cleanup_error(format!(
                "spawn systemctl output reader: {error}"
            )));
        }
    };

    let deadline = platform.now() + timeout;
    let mut failure = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                failure = Some(format!("wait for systemctl: {error}"));
                if let Err(kill_error) = child.start_kill() {
                    return Err(scope_cleanup_error(format!(
                        "{failure:?}; kill systemctl failed: {kill_error}"
                    )));
                }
                break;
            }
        }
        let now = platform.now();
        if now >= deadline {
            failure = Some(format!("systemctl {} exceeded {timeout:?}", args.join(" ")));
            if let Err(kill_error) = child.start_kill() {
                return Err(scope_cleanup_error(format!(
                    "{failure:?}; kill systemctl failed: {kill_error}"
                )));
            }
            break;
        }
        platform.sleep(POLL_INTERVAL.min(deadline - now));
    }
    let wait = child.wait();
    let stdout = readers
        .remove(0)
        .join()
        .map_err(|_| scope_cleanup_error("systemctl stdout reader panicked".to_owned()))?
        .map_err(|error| scope_cleanup_error(format!("read systemctl stdout: {error}")))?;
    let stderr = readers
        .remove(0)
        .join()
        .map_err(|_| scope_cleanup_error("systemctl stderr reader panicked".to_owned()))?
        .map_err(|error| scope_cleanup_error(format!("read systemctl stderr: {error}")))?;
    if let Some(error) = failure {
        return Err(scope_cleanup_error(error));
    }
    let status = wait.map_err(|error| scope_cleanup_error(format!("reap systemctl: {error}")))?;
    Ok(Some(Output {
        status,
        stdout,
        stderr,
    }))
}

#[cfg(target_os = "linux")]
fn scope_cleanup_error(message: String) -> Error {
    Error::RuntimeScopeCleanup(message)
}

/// Drain the watchdog's stdin until the workbench closes it. The pipe carries no
/// commands; EOF is the parent-death signal.
fn read_parent_pipe() -> io::Result<()> {
    io::copy(&mut io::stdin().lock(), &mut io::sink()).map(|_| ())
}

fn push_output(output: &mut String, chunk: &[u8]) {
    output.push_str(&String::from_utf8_lossy(chunk));
    if output.len() > MAX_OUTPUT_BYTES {
        let mut remove = output.len() - MAX_OUTPUT_BYTES;
        while !output.is_char_boundary(remove) {
            remove += 1;
        }
        output.drain(..remove);
    }
}

fn spawn_output_reader<T>(
    mut reader: T,
    output: Arc<Mutex<String>>,
    name: &'static str,
) -> io::Result<JoinHandle<()>>
where
    T: Read + Send + 'static,
{
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => {
                    push_output(&mut lock_output(&output), &buffer[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    })
}

fn lock_output(output: &Mutex<String>) -> MutexGuard<'_, String> {
    output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn valid_host_web_url(url: &str) -> bool {
    url.strip_prefix("http://127.0.0.1:")
        .and_then(|rest| rest.strip_suffix('/'))
        .is_some_and(|port| {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port > 0)
        })
}

/// The callers scan one bounded output line for a short fixed marker, so a
/// direct scan is simpler than a substring-search state machine.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn parse_host_control_line(line: &[u8]) -> Option<std::result::Result<HostControl, String>> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(fields) = line.strip_prefix("METIS_HOST_PID\t") {
        let mut fields = fields.split('\t');
        let pid = fields.next().and_then(|value| value.parse::<u32>().ok());
        let start_time = fields.next().and_then(|value| value.parse::<u64>().ok());
        let valid =
            fields.next().is_none() && pid.is_some_and(|pid| pid > 0) && start_time.is_some();
        if !valid {
            return Some(Err("the Host sent an invalid identity event".to_owned()));
        }
        return Some(Ok(HostControl::HostIdentity(HostIdentity {
            pid: pid.expect("PID was validated above"),
            start_time: start_time.expect("start time was validated above"),
        })));
    }
    if line == "METIS_CLEANUP_DONE" {
        return Some(Ok(HostControl::CleanupDone));
    }
    if !line.starts_with("METIS_READY\t") {
        return line
            .contains("METIS_READY")
            .then(|| Err("the Host interleaved control and diagnostic output".to_owned()));
    }
    let mut fields = line["METIS_READY\t".len()..].split('\t');
    let web_url = fields.next().expect("ready URL field");
    let auth_url = fields.next().unwrap_or_default();
    let auth_prefix = format!("{web_url}connect?token=");
    let token = auth_url.strip_prefix(&auth_prefix).unwrap_or_default();
    let valid = fields.next().is_none()
        && valid_host_web_url(web_url)
        && !token.is_empty()
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !valid {
        return Some(Err("the Host sent an invalid ready event".to_owned()));
    }
    Some(Ok(HostControl::Ready(HostReady {
        web_url: web_url.to_owned(),
        auth_url: auth_url.to_owned(),
    })))
}

fn drain_host_line(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let (consumed, finished) = {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(());
            }
            match buffer.iter().position(|byte| *byte == b'\n') {
                Some(position) => (position + 1, true),
                None => (buffer.len(), false),
            }
        };
        reader.consume(consumed);
        if finished {
            return Ok(());
        }
    }
}

fn spawn_host_reader<T>(
    reader: T,
    output: Arc<Mutex<String>>,
    name: &'static str,
    events: mpsc::SyncSender<std::result::Result<HostControl, String>>,
) -> io::Result<JoinHandle<()>>
where
    T: Read + Send + 'static,
{
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = Vec::new();
            // The Take limit bounds one line for this call only; bytes after a
            // newline stay in the BufReader for the next iteration.
            let read = reader
                .by_ref()
                .take((MAX_OUTPUT_BYTES + 1) as u64)
                .read_until(b'\n', &mut line);
            match read {
                Ok(0) => return,
                Ok(_) if line.len() > MAX_OUTPUT_BYTES && !line.ends_with(b"\n") => {
                    if contains_bytes(&line, b"METIS_READY") {
                        let _ = events
                            .try_send(Err("the Host exceeded the control-line limit".to_owned()));
                    } else {
                        push_output(&mut lock_output(&output), &line);
                    }
                    if drain_host_line(&mut reader).is_err() {
                        return;
                    }
                }
                Ok(_) => match parse_host_control_line(&line) {
                    Some(event) => {
                        let _ = events.try_send(event);
                    }
                    None => push_output(&mut lock_output(&output), &line),
                },
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    const COMMAND_GROUP_FIXTURE_ENV: &str = "METIS_TEST_COMMAND_GROUP_FIXTURE";

    #[cfg(target_os = "linux")]
    struct OwnerGuard(std::process::Child);

    #[cfg(target_os = "linux")]
    impl Drop for OwnerGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "linux")]
    struct GroupGuard(i32);

    #[cfg(target_os = "linux")]
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            let _ = kill_command_group(self.0);
        }
    }

    #[cfg(target_os = "linux")]
    fn find_command_group(marker: &str) -> Option<(u32, i32, i32)> {
        std::fs::read_dir("/proc")
            .ok()?
            .filter_map(std::result::Result::ok)
            .find_map(|entry| {
                let path = entry.path();
                let environment = std::fs::read(path.join("environ")).ok()?;
                if !environment
                    .split(|byte| *byte == 0)
                    .any(|entry| entry == marker.as_bytes())
                {
                    return None;
                }
                let stat = std::fs::read(path.join("stat")).ok()?;
                let (parent, group) = process_relation_from_stat(&stat)?;
                let pid = path.file_name()?.to_string_lossy().parse::<u32>().ok()?;
                Some((pid, parent, group))
            })
    }

    #[test]
    fn output_buffer_discards_old_bytes_without_splitting_utf8() {
        let mut output = String::new();
        push_output(&mut output, "汉".repeat(MAX_OUTPUT_BYTES).as_bytes());
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        assert!(output.chars().all(|character| character == '汉'));
    }

    #[test]
    fn forwarder_drains_input_after_the_output_sink_closes() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut input = io::Cursor::new(vec![b'x'; 4 * 8192]);
        forward_output(&mut input, BrokenWriter);
        assert_eq!(input.position(), input.get_ref().len() as u64);
    }

    fn read_host_output(input: Vec<u8>) -> (String, Vec<std::result::Result<HostControl, String>>) {
        let output = Arc::new(Mutex::new(String::new()));
        let (events, receiver) = mpsc::sync_channel(4);
        let reader = spawn_host_reader(
            io::Cursor::new(input),
            Arc::clone(&output),
            "test-host-stdout",
            events,
        )
        .expect("spawn Host reader");
        reader.join().expect("join Host reader");
        let events = receiver.iter().collect();
        (lock_output(&output).clone(), events)
    }

    #[test]
    fn host_ready_requires_paired_loopback_urls() {
        let token = "test-token";
        let web_url = "http://127.0.0.1:1234/";
        let auth_url = format!("{web_url}connect?token={token}");
        let event =
            parse_host_control_line(format!("METIS_READY\t{web_url}\t{auth_url}\n").as_bytes())
                .expect("control line")
                .expect("valid ready event");
        let HostControl::Ready(event) = event else {
            panic!("expected Host ready event")
        };
        assert_eq!(event.web_url, web_url);
        assert_eq!(event.auth_url, auth_url);

        for (web_url, auth_url) in [
            (
                "http://127.0.0.1:1234@evil.example/",
                format!("http://127.0.0.1:1234@evil.example/connect?token={token}"),
            ),
            (
                "http://127.0.0.1:0/",
                format!("{web_url}connect?token={token}"),
            ),
            (
                "http://127.0.0.1:not-a-port/",
                format!("{web_url}connect?token={token}"),
            ),
            (web_url, format!("{auth_url}&unexpected=true")),
        ] {
            assert!(
                parse_host_control_line(format!("METIS_READY\t{web_url}\t{auth_url}\n").as_bytes())
                    .expect("control line")
                    .is_err(),
                "accepted invalid pair: {web_url} {auth_url}"
            );
        }
    }

    #[test]
    fn host_reader_preserves_identity_ready_and_cleanup_events() {
        let web_url = "http://127.0.0.1:1234/";
        let input = format!(
            "METIS_HOST_PID\t123\t456\nMETIS_READY\t{web_url}\t{web_url}connect?token=test-token\nMETIS_CLEANUP_DONE\n"
        );
        let (_, events) = read_host_output(input.into_bytes());
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            Ok(HostControl::HostIdentity(HostIdentity {
                pid: 123,
                start_time: 456,
            }))
        ));
        assert!(matches!(events[1], Ok(HostControl::Ready(_))));
        assert!(matches!(events[2], Ok(HostControl::CleanupDone)));
    }

    #[test]
    fn host_reader_bounds_unterminated_output() {
        let (output, events) = read_host_output(vec![b'x'; MAX_OUTPUT_BYTES * 3]);
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        assert!(events.is_empty());
    }

    #[test]
    fn host_reader_redacts_interleaved_ready_credentials() {
        let token = "A".repeat(43);
        let web_url = "http://127.0.0.1:1234/";
        let input =
            format!("unfinished logMETIS_READY\t{web_url}\t{web_url}connect?token={token}\n");
        let (output, events) = read_host_output(input.into_bytes());
        assert!(!output.contains(&token));
        assert!(events.first().is_some_and(std::result::Result::is_err));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detached_command_groups_are_killed_when_owner_dies() {
        use std::os::unix::fs::PermissionsExt as _;

        if let Some(path) = std::env::var_os(COMMAND_GROUP_FIXTURE_ENV) {
            let path = PathBuf::from(path);
            let _ = run_systemctl_with_timeout(
                &SystemPlatform,
                path.to_str().expect("UTF-8 command fixture path"),
                &[],
                Duration::from_secs(30),
            );
            return;
        }

        let path = std::env::temp_dir().join(format!(
            "metis-systemctl-owner-death-{}",
            std::process::id()
        ));
        std::fs::write(&path, "#!/bin/sh\nsleep 30\n").expect("write hanging command fixture");
        let mut permissions = std::fs::metadata(&path)
            .expect("read command fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("make command fixture executable");

        let mut owner = OwnerGuard(
            Command::new(std::env::current_exe().expect("current test executable"))
                .args([
                    "runtime::tests::detached_command_groups_are_killed_when_owner_dies",
                    "--exact",
                ])
                .env(COMMAND_GROUP_FIXTURE_ENV, &path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn command owner"),
        );
        let identity = host_identity(&SystemPlatform, owner.0.id()).expect("read owner identity");
        let marker = format!(
            "{COMMAND_GROUP_OWNER_ENV}={}",
            command_group_owner_value(identity)
        );

        let mut observed = None;
        for attempt in 0..200 {
            observed = find_command_group(&marker);
            if observed.is_some() {
                break;
            }
            assert!(attempt < 199, "command fixture did not start in time");
            thread::sleep(Duration::from_millis(10));
        }
        let (_, _, group) = observed.expect("command fixture was observed");
        let _group_cleanup = GroupGuard(group);

        let started = Instant::now();
        owner.0.kill().expect("kill the command owner");
        owner.0.wait().expect("reap the command owner");
        let (_, parent, _) =
            find_command_group(&marker).expect("orphaned command must remain running");
        assert_ne!(
            parent,
            i32::try_from(identity.pid).expect("owner PID fits in i32"),
            "owner death must re-parent the command before cleanup"
        );
        kill_detached_command_groups(identity).expect("kill the orphaned command group");
        for attempt in 0..200 {
            if find_command_group(&marker).is_none() {
                break;
            }
            assert!(attempt < 199, "orphaned command group did not stop");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            find_command_group(&marker).is_none(),
            "orphaned command must be gone"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "owner cleanup must not wait for the command's 30-second sleep"
        );
        std::fs::remove_file(path).expect("remove hanging command fixture");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_command_group_is_already_stopped() {
        use std::os::unix::process::CommandExt as _;

        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn command group");
        let group = i32::try_from(child.id()).expect("child PID fits in i32");
        child.kill().expect("kill command group leader");
        child.wait().expect("reap command group leader");
        kill_command_group(group).expect("an exited command group needs no further cleanup");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scope_cleanup_failure_keeps_retry_state() {
        let identity = HostIdentity {
            pid: 1,
            start_time: 0,
        };
        let (_events, receiver) = mpsc::channel();
        let mut runtime = DshRuntime {
            id: 0,
            watchdog_identity: identity,
            host_identity: Some(identity),
            pending_scope_cleanup: Some(PendingScopeCleanup {
                identity,
                units: None,
            }),
            watchdog_cleanup_done: false,
            web_url: None,
            auth_url: None,
            child: None,
            output: Arc::new(Mutex::new(String::new())),
            readers: Vec::new(),
            events: receiver,
            platform: Arc::new(SystemPlatform),
            shutdown_timeout: Duration::ZERO,
        };

        runtime
            .shutdown()
            .expect_err("the mismatched Host start time must fail discovery");
        assert!(runtime.pending_scope_cleanup.is_some());
        runtime
            .pending_scope_cleanup
            .as_mut()
            .expect("retry state remains present")
            .units = Some(BTreeSet::new());
        runtime.shutdown().expect("retry with no remaining scopes");
        assert!(runtime.pending_scope_cleanup.is_none());
        runtime
            .shutdown()
            .expect("an already-clean runtime is idempotent");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemctl_timeout_kills_the_command_group() {
        use std::os::unix::fs::PermissionsExt as _;

        let path =
            std::env::temp_dir().join(format!("metis-systemctl-timeout-{}", std::process::id()));
        std::fs::write(&path, "#!/bin/sh\nsleep 30\n").expect("write hanging systemctl fixture");
        let mut permissions = std::fs::metadata(&path)
            .expect("read systemctl fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("make systemctl fixture executable");

        let started = Instant::now();
        let error = run_systemctl_with_timeout(
            &SystemPlatform,
            path.to_str().expect("UTF-8 systemctl fixture path"),
            &[],
            Duration::from_millis(50),
        )
        .expect_err("hanging systemctl must time out");
        assert!(matches!(error, Error::RuntimeScopeCleanup(_)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout must not wait for the fixture's own 30-second sleep"
        );
        std::fs::remove_file(path).expect("remove hanging systemctl fixture");
    }
}
