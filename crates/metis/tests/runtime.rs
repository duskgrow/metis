//! Process-level runtime tests. These deliberately use real operating-system
//! processes: mocking the supervisor would not verify pipe closure, process
//! groups, job objects, or descendant cleanup.

use std::{
    env, fs,
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use metis::{
    Error,
    runtime::{DshRuntimeConfig, DshSupervisor},
};

const CRASH_FIXTURE_ENV: &str = "METIS_TEST_CRASH_FIXTURE";
const CRASH_HOME_ENV: &str = "METIS_TEST_CRASH_HOME";
const PROCESS_PROBE: &str = r#"
const fs = require("node:fs");
let alive = false;
for (const argument of process.argv.slice(1)) {
  const pid = Number(argument);
  try {
    process.kill(pid, 0);
  } catch (error) {
    if (error.code === "ESRCH") continue;
    throw error;
  }
  if (process.platform === "linux") {
    try {
      const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
      const end = stat.lastIndexOf(")");
      if (stat.slice(end + 2).startsWith("Z")) continue;
    } catch (error) {
      if (error.code === "ENOENT") continue;
      throw error;
    }
  }
  alive = true;
}
process.exitCode = alive ? 1 : 0;
"#;

const FAKE_WEB_SERVER: &str = r#"
const webServer = require("node:net").createServer();
webServer.listen(Number(process.argv[process.argv.indexOf("--port") + 1]), "127.0.0.1");
"#;

fn with_web_server(script: &str) -> String {
    format!("{FAKE_WEB_SERVER}\n{script}")
}

#[test]
fn real_dsh_loads_plugin_and_stops() {
    let directory = TestDirectory::new("real-dsh");
    let config = runtime_config(&directory, real_dsh_entrypoint());
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start the pinned dsh with the Metis plugin");

    wait_until(
        || {
            runtime
                .output_snapshot()
                .contains("[metis] host plugin loaded")
        },
        "dsh plugin load marker",
    );
    let address = runtime
        .web_url()
        .strip_prefix("http://")
        .and_then(|url| url.split('/').next())
        .expect("runtime exposes a loopback URL")
        .to_owned();
    let watchdog = runtime.id();
    let _cleanup = TestTreeGuard::containing(watchdog, &[watchdog]);
    runtime.shutdown().expect("stop the dsh process tree");
    runtime.shutdown().expect("shutdown is idempotent");
    wait_until(
        || processes_have_exited(&[watchdog]),
        "watchdog shutdown after real dsh test",
    );
    assert!(
        TcpStream::connect(address).is_err(),
        "the dsh listener must close with its runtime"
    );
}

#[test]
fn startup_timeout_cleans_up_the_dsh_process() {
    let directory = TestDirectory::new("startup-timeout");
    let script = directory.write(
        "never-ready.cjs",
        r#"
const fs = require("node:fs");
fs.writeFileSync(require("node:path").join(process.env.DSH_HOME, "dsh.pid"), String(process.pid));
console.log("fake dsh started without a listener");
setInterval(() => {}, 1000);
"#,
    );
    let mut config = runtime_config(&directory, script);
    config.startup_timeout = Duration::from_secs(2);

    let error = DshSupervisor::system()
        .start(&config)
        .err()
        .expect("a runtime without a listener must time out");
    assert!(
        matches!(&error, Error::RuntimeStartupTimeout { .. }),
        "unexpected startup error: {error:?}"
    );
    let dsh = read_pid(&directory.path.join("dsh-home/dsh.pid"));
    let _cleanup = TestTreeGuard::new(&[dsh]);
    wait_until(
        || processes_have_exited(&[dsh]),
        "dsh cleanup after startup timeout",
    );
}

#[test]
fn startup_exit_preserves_dsh_status_output_and_cleans_tree() {
    let directory = TestDirectory::new("startup-exit");
    let script = directory.write(
        "exit-with-agent.cjs",
        r#"
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const agent = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000);"], {
  stdio: "ignore",
});
agent.unref();
fs.writeFileSync(path.join(process.env.DSH_HOME, "watchdog.pid"), String(process.ppid));
fs.writeFileSync(path.join(process.env.DSH_HOME, "dsh.pid"), String(process.pid));
fs.writeFileSync(path.join(process.env.DSH_HOME, "agent.pid"), String(agent.pid));
process.stdout.write("dsh-final-stdout");
process.stderr.write("dsh-final-stderr");
process.exitCode = 7;
"#,
    );
    let config = runtime_config(&directory, script);

    let error = DshSupervisor::system()
        .start(&config)
        .err()
        .expect("dsh exits before its listener is ready");
    let watchdog = read_pid(&directory.path.join("dsh-home/watchdog.pid"));
    let dsh = read_pid(&directory.path.join("dsh-home/dsh.pid"));
    let agent = read_pid(&directory.path.join("dsh-home/agent.pid"));
    let _cleanup = TestTreeGuard::containing(watchdog, &[watchdog, dsh, agent]);
    wait_until(
        || processes_have_exited(&[watchdog, dsh, agent]),
        "tree cleanup after dsh startup exit",
    );

    match error {
        Error::RuntimeStartupExit { status, output } => {
            assert!(
                status.ends_with('7'),
                "preserve dsh's actual exit status: {status}"
            );
            assert!(
                output.contains("dsh-final-stdout"),
                "missing stdout: {output}"
            );
            assert!(
                output.contains("dsh-final-stderr"),
                "missing stderr: {output}"
            );
        }
        error => panic!("unexpected startup error: {error:?}"),
    }
}

#[cfg(unix)]
#[test]
fn shutdown_escalation_covers_the_shared_process_group() {
    let directory = TestDirectory::new("shutdown-escalation");
    let script = directory.write(
        "unresponsive-runtime.cjs",
        &with_web_server(UNRESPONSIVE_TREE_SCRIPT),
    );
    let mut config = runtime_config(&directory, script);
    config.shutdown_timeout = Duration::from_millis(100);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start the unresponsive fixture");
    let watchdog = runtime.id();
    let dsh = read_pid(&directory.path.join("dsh-home/dsh.pid"));
    let agent = read_pid(&directory.path.join("dsh-home/agent.pid"));
    let _cleanup = TestTreeGuard::containing(watchdog, &[watchdog, dsh, agent]);

    let started = Instant::now();
    let error = runtime.shutdown().expect_err("shutdown must escalate");
    assert!(
        matches!(error, Error::RuntimeShutdownTimeout(_)),
        "unexpected shutdown error: {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the outer group kill must not wait for the watchdog's two-second grace period"
    );
    wait_until(
        || processes_have_exited(&[watchdog, dsh, agent]),
        "shared-group cleanup after shutdown escalation",
    );
}

#[cfg(unix)]
#[test]
fn shutdown_drains_output_written_by_the_signal_handler() {
    let directory = TestDirectory::new("shutdown-output");
    let script = directory.write(
        "shutdown-output.cjs",
        &with_web_server(
            r#"
const fs = require("node:fs");
const path = require("node:path");
process.once("SIGTERM", () => {
  process.stdout.write("shutdown-final-stdout", () => {
    process.stderr.write("shutdown-final-stderr", () => process.exit(0));
  });
});
fs.writeFileSync(path.join(process.env.DSH_HOME, "dsh.pid"), String(process.pid));
setInterval(() => {}, 1000);
"#,
        ),
    );
    let config = runtime_config(&directory, script);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start the shutdown-output fixture");
    let watchdog = runtime.id();
    let dsh = read_pid(&directory.path.join("dsh-home/dsh.pid"));
    let _cleanup = TestTreeGuard::containing(watchdog, &[watchdog, dsh]);

    runtime.shutdown().expect("stop the runtime");
    let output = runtime.output_snapshot();
    assert!(
        output.contains("shutdown-final-stdout"),
        "missing stdout: {output}"
    );
    assert!(
        output.contains("shutdown-final-stderr"),
        "missing stderr: {output}"
    );
}

#[cfg(unix)]
#[test]
fn detached_pipe_writers_cannot_outlive_the_outer_readers() {
    let directory = TestDirectory::new("detached-pipe-writer");
    let script = directory.write(
        "detached-pipe-writer.cjs",
        &with_web_server(
            r#"
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const agent = spawn(process.execPath, ["-e", `
  setTimeout(() => process.stdout.write("late-escaped-output"), 300);
  setInterval(() => {}, 1000);
`], {
  detached: true,
  stdio: "inherit",
});
process.once("SIGTERM", () => process.exit(0));
fs.writeFileSync(path.join(process.env.DSH_HOME, "dsh.pid"), String(process.pid));
fs.writeFileSync(path.join(process.env.DSH_HOME, "agent.pid"), String(agent.pid));
setInterval(() => {}, 1000);
"#,
        ),
    );
    let config = runtime_config(&directory, script);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start the detached-pipe fixture");
    let watchdog = runtime.id();
    let dsh = read_pid(&directory.path.join("dsh-home/dsh.pid"));
    let agent = read_pid(&directory.path.join("dsh-home/agent.pid"));
    let _cleanup = TestTreeGuard::containing(watchdog, &[watchdog, dsh, agent]);

    runtime
        .shutdown()
        .expect("stop the runtime without joining the escaped writer");
    let final_output = runtime.output_snapshot();
    thread::sleep(Duration::from_millis(450));
    assert_eq!(runtime.output_snapshot(), final_output);
    assert!(!final_output.contains("late-escaped-output"));
}

#[test]
fn forced_parent_death_cleans_cooperative_tree_and_allows_restart() {
    if env::var_os(CRASH_FIXTURE_ENV).is_some() {
        run_crash_fixture_parent();
        return;
    }

    let directory = TestDirectory::new("forced-parent-death");
    let script = directory.write("runtime-with-agent.cjs", &with_web_server(FAKE_TREE_SCRIPT));
    let mut fixture = ProcessGuard(
        Command::new(env::current_exe().expect("current integration-test executable"))
            .args([
                "forced_parent_death_cleans_cooperative_tree_and_allows_restart",
                "--exact",
            ])
            .env(CRASH_FIXTURE_ENV, "1")
            .env(CRASH_HOME_ENV, &directory.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the fixture workbench process"),
    );

    wait_until(
        || directory.path.join("ready").is_file(),
        "fixture workbench startup",
    );
    let watchdog = read_pid(&directory.path.join("ready"));
    let dsh = read_pid(&directory.path.join("dsh-home/dsh.pid"));
    let agent = read_pid(&directory.path.join("dsh-home/agent.pid"));
    let _cleanup = TestTreeGuard::containing(watchdog, &[watchdog, dsh, agent]);

    fixture.0.kill().expect("force-kill the fixture workbench");
    fixture.0.wait().expect("reap the fixture workbench");
    wait_until(
        || processes_have_exited(&[watchdog, dsh, agent]),
        "watchdog, dsh, and agent cleanup after forced parent death",
    );

    let mut restarted = DshSupervisor::system()
        .start(&runtime_config(&directory, script))
        .expect("a fresh runtime starts after the previous workbench was killed");
    assert_ne!(restarted.id(), watchdog, "a new watchdog must be created");
    restarted.shutdown().expect("stop the restarted runtime");
}

const FAKE_TREE_SCRIPT: &str = r#"
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const agent = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000);"], {
  detached: true,
  stdio: "ignore",
});
// Model dsh's documented host-exit cleanup for its managed detached scopes.
process.once("SIGTERM", () => {
  try { process.kill(agent.pid, "SIGKILL"); } catch {}
  process.exit(0);
});
fs.writeFileSync(path.join(process.env.DSH_HOME, "dsh.pid"), String(process.pid));
fs.writeFileSync(path.join(process.env.DSH_HOME, "agent.pid"), String(agent.pid));
console.log("[metis] host plugin loaded");
setInterval(() => {}, 1000);
"#;

#[cfg(unix)]
const UNRESPONSIVE_TREE_SCRIPT: &str = r#"
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
// Keep this agent in the shared group: this test isolates outer PGID escalation.
const agent = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000);"], {
  stdio: "ignore",
});
process.on("SIGTERM", () => {});
fs.writeFileSync(path.join(process.env.DSH_HOME, "dsh.pid"), String(process.pid));
fs.writeFileSync(path.join(process.env.DSH_HOME, "agent.pid"), String(agent.pid));
setInterval(() => {}, 1000);
"#;

fn run_crash_fixture_parent() {
    let directory = PathBuf::from(env::var_os(CRASH_HOME_ENV).expect("crash fixture home"));
    let script = directory.join("runtime-with-agent.cjs");
    let runtime = DshSupervisor::system()
        .start(&DshRuntimeConfig {
            metis_executable: PathBuf::from(env!("CARGO_BIN_EXE_metis")),
            node_executable: PathBuf::from("node"),
            dsh_entrypoint: script,
            plugin: workspace_root().join("packages/metis-host/lib/index.js"),
            home: directory.join("dsh-home"),
            startup_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(5),
        })
        .expect("fixture workbench starts its runtime");
    fs::write(directory.join("ready"), runtime.id().to_string()).expect("publish watchdog PID");

    // Keep Drop from running: the outer test terminates this process directly.
    // Keeping the runtime binding alive is intentional.
    loop {
        thread::park();
    }
}

fn runtime_config(directory: &TestDirectory, dsh_entrypoint: PathBuf) -> DshRuntimeConfig {
    DshRuntimeConfig {
        metis_executable: PathBuf::from(env!("CARGO_BIN_EXE_metis")),
        node_executable: PathBuf::from("node"),
        dsh_entrypoint,
        plugin: workspace_root().join("packages/metis-host/lib/index.js"),
        home: directory.path.join("dsh-home"),
        startup_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_secs(5),
    }
}

fn real_dsh_entrypoint() -> PathBuf {
    workspace_root().join("node_modules/@deepseek-ai/dsh/lib/bin.js")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .parse()
        .expect("PID file contains an integer")
}

fn processes_have_exited(pids: &[u32]) -> bool {
    let mut command = Command::new("node");
    command.arg("-e").arg(PROCESS_PROBE);
    for pid in pids {
        command.arg(pid.to_string());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run the cross-platform Node.js process probe")
        .success()
}

fn wait_until(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

const PROCESS_KILL: &str = r#"
if (process.platform !== "win32" && process.env.METIS_TEST_KILL_GROUP) {
  try { process.kill(-Number(process.env.METIS_TEST_KILL_GROUP), "SIGKILL"); } catch {}
}
for (const argument of process.argv.slice(1)) {
  try { process.kill(Number(argument), "SIGKILL"); } catch {}
}
"#;

/// Last-resort cleanup owned by the test, independent of the supervisor under
/// test. On Unix the watchdog PID is also the shared process-group ID.
struct TestTreeGuard {
    group_leader: Option<u32>,
    pids: Vec<u32>,
}

impl TestTreeGuard {
    fn new(pids: &[u32]) -> Self {
        Self {
            group_leader: None,
            pids: pids.to_vec(),
        }
    }

    fn containing(group_leader: u32, pids: &[u32]) -> Self {
        Self {
            group_leader: Some(group_leader),
            pids: pids.to_vec(),
        }
    }
}

impl Drop for TestTreeGuard {
    fn drop(&mut self) {
        let mut command = Command::new("node");
        command.arg("-e").arg(PROCESS_KILL);
        for pid in &self.pids {
            command.arg(pid.to_string());
        }
        if let Some(group_leader) = self.group_leader {
            command.env("METIS_TEST_KILL_GROUP", group_leader.to_string());
        }
        let _ = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct ProcessGuard(Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "metis-runtime-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create isolated test directory");
        Self { path }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, contents).expect("write test fixture");
        path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
