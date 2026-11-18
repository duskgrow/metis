//! Supervision for the private dsh runtime used by a workbench.
//!
//! The watchdog is a hidden mode of the Metis executable. Its stdin is a
//! lifetime pipe: if the workbench disappears, EOF tells the watchdog to stop
//! the dsh process tree. Windows uses nested job objects so either the parent or
//! the watchdog can explicitly terminate the complete tree. Unix shares one
//! process group; dsh gets a grace period to clean up its deliberately detached
//! systemd scopes before the shared group receives its final signal. If dsh is
//! killed before that cleanup completes, orphaned user-systemd scopes need a
//! separate external owner; that boundary stays tracked in milestone 0.

use std::{
    fmt::Write as _,
    io::{self, Read, Write},
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
const DSH_GRACE_PERIOD: Duration = Duration::from_secs(2);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const PATCH_FILE_NAME: &str = "metis-host.patch.json";
const DSH_EXIT_STATUS_FILE_NAME: &str = "dsh-exit-status";
#[doc(hidden)]
pub const WATCHDOG_COMMAND: &str = "__dsh-watchdog";

/// Paths and timeouts needed to start an isolated, extended dsh runtime.
#[derive(Clone, Debug)]
pub struct DshRuntimeConfig {
    /// Metis executable used for the hidden watchdog mode.
    pub metis_executable: PathBuf,
    /// Pinned Node.js executable.
    pub node_executable: PathBuf,
    /// JavaScript entry point of the pinned dsh package.
    pub dsh_entrypoint: PathBuf,
    /// Compiled Metis Cordis plugin.
    pub plugin: PathBuf,
    /// Per-runtime dsh state directory, passed as `DSH_HOME`.
    pub home: PathBuf,
    /// Maximum time to wait for dsh to accept TCP connections.
    pub startup_timeout: Duration,
    /// Maximum time to wait for the watchdog to clean up dsh on shutdown.
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
        let patch_path = config.home.join(PATCH_FILE_NAME);
        let patch = plugin_patch(&config.plugin)?;
        self.platform
            .write(&patch_path, patch.as_bytes())
            .map_err(|source| Error::RuntimePrepare {
                path: patch_path.clone(),
                source,
            })?;
        let port = self.platform.available_port().map_err(Error::RuntimePort)?;

        let mut command = Command::new(&config.metis_executable);
        command
            .arg(WATCHDOG_COMMAND)
            .arg("--node")
            .arg(&config.node_executable)
            .arg("--dsh")
            .arg(&config.dsh_entrypoint)
            .arg("--patch")
            .arg(&patch_path)
            .arg("--home")
            .arg(&config.home)
            .arg("--port")
            .arg(port.to_string())
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
        let (Some(stdout), Some(stderr)) = (child.stdout().take(), child.stderr().take()) else {
            if let Err(error) = child.start_kill() {
                tracing::warn!(%error, "failed to stop a watchdog after output capture failed");
            } else if let Err(error) = child.wait() {
                tracing::warn!(%error, "failed to reap a watchdog after output capture failed");
            }
            return Err(Error::RuntimeOutputCapture);
        };
        let output = Arc::new(Mutex::new(String::new()));
        let mut runtime = DshRuntime {
            id,
            web_url: format!("http://127.0.0.1:{port}/"),
            child: Some(child),
            output: Arc::clone(&output),
            readers: Vec::new(),
            platform: Arc::clone(&self.platform),
            shutdown_timeout: config.shutdown_timeout,
        };
        // Construct the cleanup guard before spawning reader threads: a thread
        // creation panic must not strand the already-spawned runtime.
        runtime.readers.push(
            spawn_output_reader(stdout, Arc::clone(&output), "dsh-stdout")
                .map_err(|_| Error::RuntimeOutputCapture)?,
        );
        runtime.readers.push(
            spawn_output_reader(stderr, output, "dsh-stderr")
                .map_err(|_| Error::RuntimeOutputCapture)?,
        );

        if let Err(error) = runtime.wait_until_ready(config, port) {
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
    web_url: String,
    child: Option<Box<dyn ChildWrapper>>,
    output: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
    platform: Arc<dyn RuntimePlatform>,
    shutdown_timeout: Duration,
}

impl DshRuntime {
    /// PID of the watchdog, not the Node.js process.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The local dsh web root.
    #[must_use]
    pub fn web_url(&self) -> &str {
        &self.web_url
    }

    /// Recent combined stdout/stderr for diagnostics. The buffer is bounded;
    /// this is not a durable log or an event stream.
    #[must_use]
    pub fn output_snapshot(&self) -> String {
        lock_output(&self.output).clone()
    }

    /// Stop the supervised process group/job and wait for its watchdog. dsh's
    /// detached systemd scopes rely on its graceful signal handler; the final
    /// group kill is not an external systemd-scope cleanup mechanism.
    pub fn shutdown(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
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
        // dsh and its descendants only inherit watchdog-owned pipes. Once the
        // watchdog is reaped, the outer readers have finite input and can drain
        // all remaining diagnostics without waiting for escaped descendants.
        self.finish_readers();

        if let Some(error) = monitor_error {
            return Err(Error::RuntimeWait(error));
        }
        if timed_out {
            return Err(Error::RuntimeShutdownTimeout(self.shutdown_timeout));
        }
        Ok(())
    }

    fn wait_until_ready(&mut self, config: &DshRuntimeConfig, port: u16) -> Result<()> {
        let deadline = self.platform.now() + config.startup_timeout;
        loop {
            if self.platform.is_listening(port) {
                return Ok(());
            }
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

/// Clock, filesystem, and process creation used by runtime supervision.
/// Production uses [`SystemPlatform`]; tests can inject deterministic clocks or
/// filesystem failures without global state.
pub trait RuntimePlatform: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn available_port(&self) -> io::Result<u16>;
    fn is_listening(&self, port: u16) -> bool;
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

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        std::fs::write(path, contents)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn available_port(&self) -> io::Result<u16> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        Ok(listener.local_addr()?.port())
    }

    fn is_listening(&self, port: u16) -> bool {
        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        std::net::TcpStream::connect_timeout(&address, POLL_INTERVAL).is_ok()
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
    pub dsh_entrypoint: PathBuf,
    pub patch: PathBuf,
    pub home: PathBuf,
    pub port: u16,
}

/// Run the dsh side of the lifetime pipe. Called only by the hidden binary
/// subcommand, never by the workbench in-process.
#[doc(hidden)]
pub fn run_watchdog(config: &WatchdogConfig) -> Result<()> {
    let platform = SystemPlatform;
    let mut command = Command::new(&config.node_executable);
    command
        .arg(&config.dsh_entrypoint)
        .arg("web")
        .arg("--patch")
        .arg(&config.patch)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--no-open")
        .env("DSH_HOME", &config.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = platform
        .spawn(command, ProcessContainment::ChildTree)
        .map_err(|source| Error::RuntimeSpawn {
            program: config.node_executable.clone(),
            source,
        })?;

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
            #[cfg(unix)]
            if let Err(error) = &graceful_result {
                tracing::warn!(%error, "graceful dsh shutdown failed; escalating to a group kill");
            }
            let cleanup_result = cleanup_dsh(child.as_mut(), &platform, &mut forwarders);
            graceful_result?;
            cleanup_result
        }
        Ok(Some(status)) => {
            // The record is closed before cleanup can terminate the watchdog,
            // so the supervisor can recover dsh's status after reaping it.
            publish_dsh_exit_status(&platform, config, status);
            cleanup_dsh(child.as_mut(), &platform, &mut forwarders)?;

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
            if let Err(cleanup_error) = force_stop_dsh(child.as_mut()) {
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
        // A descendant can hold an inner pipe forever. Such a forwarder remains
        // owned by this short-lived watchdog and is destroyed with its process.
        if reader.is_finished() {
            let _ = reader.join();
        }
    }
}

fn cleanup_dsh(
    child: &mut dyn ChildWrapper,
    platform: &SystemPlatform,
    forwarders: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    // Unix's group kill also terminates this watchdog, so drain first. Windows
    // closes every inner pipe in force_stop_dsh and can drain afterwards.
    #[cfg(unix)]
    drain_output_forwarders(forwarders, platform);
    let result = force_stop_dsh(child);
    #[cfg(windows)]
    drain_output_forwarders(forwarders, platform);
    result
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
    #[cfg(unix)]
    {
        // Signal only Node first. The pinned dsh's SIGTERM handler disposes its
        // root; the resulting exit hook synchronously terminates systemd scopes
        // which deliberately escaped the shared process group.
        if let Err(error) = child.signal(nix::sys::signal::Signal::SIGTERM as i32)
            && error.raw_os_error() != Some(3)
        {
            return Err(Error::RuntimeKill(error));
        }

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
    #[cfg(windows)]
    {
        let _ = (child, platform);
        Ok(())
    }
}

fn force_stop_dsh(child: &mut dyn ChildWrapper) -> Result<()> {
    #[cfg(unix)]
    {
        let _ = child;
        let group = i32::try_from(std::process::id())
            .map_err(io::Error::other)
            .map_err(Error::RuntimeKill)?;
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        )
        .map_err(io::Error::from)
        .map_err(Error::RuntimeKill)
    }
    #[cfg(windows)]
    {
        child.start_kill().map_err(Error::RuntimeKill)?;
        child.wait().map_err(Error::RuntimeWait)?;
        Ok(())
    }
}

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

fn plugin_patch(plugin: &Path) -> Result<String> {
    let plugin = plugin
        .to_str()
        .ok_or_else(|| Error::RuntimePathNotUnicode(plugin.to_owned()))?;
    let mut json = String::from('"');
    for character in plugin.chars() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            '\u{08}' => json.push_str("\\b"),
            '\u{0c}' => json.push_str("\\f"),
            '\n' => json.push_str("\\n"),
            '\r' => json.push_str("\\r"),
            '\t' => json.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(json, "\\u{:04x}", u32::from(character))
                    .expect("writing to a String cannot fail");
            }
            character => json.push(character),
        }
    }
    json.push('"');
    Ok(format!(
        "[{{\"insert\":[{{\"id\":\"metis-host\",\"name\":{json}}}]}}]\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn patch_is_json_with_an_escaped_plugin_path() {
        let patch = plugin_patch(Path::new("C:\\work\\\"plugin\"\n.js")).expect("Unicode path");
        assert_eq!(
            patch,
            "[{\"insert\":[{\"id\":\"metis-host\",\"name\":\"C:\\\\work\\\\\\\"plugin\\\"\\n.js\"}]}]\n"
        );
    }
}
