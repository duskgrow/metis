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
    client::{
        ApprovalOutcome, ApprovalRequest, ClientError, DshClient, DshClientConfig, DshStream,
        HostEvent,
    },
    journal::SessionJournal,
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
webServer.listen(0, "127.0.0.1", () => {
  const url = `http://127.0.0.1:${webServer.address().port}/`;
  process.stdout.write(`METIS_READY\t${url}\t${url}connect?token=test-token\n`);
});
"#;

const FAKE_HOST_CONTROL: &str = r#"
let controlInput = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  controlInput += chunk;
  if (controlInput.includes("METIS_SHUTDOWN\n")) process.kill(process.pid, "SIGTERM");
});
process.stdin.on("end", () => process.kill(process.pid, "SIGTERM"));
process.stdin.resume();
"#;

fn with_host_control(script: &str) -> String {
    format!("{FAKE_HOST_CONTROL}\n{script}")
}

fn with_web_server(script: &str) -> String {
    with_host_control(&format!("{FAKE_WEB_SERVER}\n{script}"))
}

#[test]
fn real_dsh_loads_plugin_and_stops() {
    let directory = TestDirectory::new("real-dsh");
    let config = runtime_config(&directory, real_dsh_entrypoint());
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start the pinned dsh with the Metis plugin");
    let auth_url = runtime.auth_url().to_owned();
    assert!(auth_url.starts_with(&format!("{}connect?token=", runtime.web_url())));
    assert!(!runtime.output_snapshot().contains(&auth_url));
    assert!(!runtime.output_snapshot().contains("METIS_READY"));

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
    TcpStream::connect(&address).expect("the reported Host listener accepts connections");
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
        &with_host_control(
            r#"
const fs = require("node:fs");
fs.writeFileSync(require("node:path").join(process.env.DSH_HOME, "dsh.pid"), String(process.pid));
console.log("fake dsh started without a listener");
setInterval(() => {}, 1000);
"#,
        ),
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
        "the outer group kill must not wait for the watchdog's internal grace period"
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
    let mut cleanup = TestTreeGuard::containing(watchdog, &[watchdog]);
    let dsh = read_pid(&directory.path.join("dsh-home/dsh.pid"));
    cleanup.pids.push(dsh);
    let agent = read_pid(&directory.path.join("dsh-home/agent.pid"));
    cleanup.pids.push(agent);
    for pid in [watchdog, dsh, agent] {
        assert!(
            !processes_have_exited(&[pid]),
            "process {pid} must be alive before the forced workbench shutdown"
        );
    }

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

#[cfg(unix)]
#[test]
fn forced_parent_death_cleans_real_host_agent_and_job() {
    if env::var_os(CRASH_FIXTURE_ENV).is_some() {
        run_real_crash_fixture_parent();
        return;
    }

    let directory = TestDirectory::new("forced-parent-death-real-host");
    let config = restart_runtime_config(&directory);
    let mut fixture = ProcessGuard(
        Command::new(env::current_exe().expect("current integration-test executable"))
            .args([
                "forced_parent_death_cleans_real_host_agent_and_job",
                "--exact",
            ])
            .env(CRASH_FIXTURE_ENV, "1")
            .env(CRASH_HOME_ENV, &directory.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the real-Host fixture workbench process"),
    );

    wait_until(
        || directory.path.join("ready").is_file(),
        "real-Host fixture workbench startup and live Agent work",
    );
    let watchdog = read_pid(&directory.path.join("ready"));
    let mut cleanup = TestTreeGuard::containing(watchdog, &[watchdog]);
    let host = read_pid(&config.home.join("metis-test-host.pid"));
    cleanup.pids.push(host);
    let job = read_pid(&directory.path.join("metis-test-job.pid"));
    cleanup.pids.push(job);
    for pid in [watchdog, host, job] {
        assert!(
            !processes_have_exited(&[pid]),
            "process {pid} must be alive before the forced workbench shutdown"
        );
    }

    fixture
        .0
        .kill()
        .expect("force-kill the real-Host workbench");
    fixture.0.wait().expect("reap the real-Host workbench");
    wait_until(
        || processes_have_exited(&[watchdog, host, job]),
        "watchdog, real Host, and live Agent job cleanup after forced parent death",
    );

    let mut restarted = DshSupervisor::system()
        .start(&config)
        .expect("a fresh real Host starts after the previous workbench was killed");
    assert_ne!(restarted.id(), watchdog, "a new watchdog must be created");
    restarted.shutdown().expect("stop the restarted real Host");
}

#[cfg(target_os = "linux")]
#[test]
fn forced_parent_death_cleans_unresponsive_host_and_detached_job() {
    if env::var_os(CRASH_FIXTURE_ENV).is_some() {
        run_real_crash_fixture_parent();
        return;
    }

    let directory = TestDirectory::new("forced-parent-death-unresponsive-host");
    let config = unresponsive_runtime_config(&directory);
    let mut fixture = ProcessGuard(
        Command::new(env::current_exe().expect("current integration-test executable"))
            .args([
                "forced_parent_death_cleans_unresponsive_host_and_detached_job",
                "--exact",
            ])
            .env(CRASH_FIXTURE_ENV, "1")
            .env(CRASH_HOME_ENV, &directory.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the unresponsive-Host fixture workbench process"),
    );

    wait_until(
        || directory.path.join("ready").is_file(),
        "unresponsive-Host fixture workbench startup and live Agent work",
    );
    let watchdog = read_pid(&directory.path.join("ready"));
    let mut cleanup = TestTreeGuard::containing(watchdog, &[watchdog]);
    let host = read_pid(&config.home.join("metis-test-host.pid"));
    cleanup.pids.push(host);
    let job = read_pid(&directory.path.join("metis-test-job.pid"));
    cleanup.pids.push(job);
    for pid in [watchdog, host, job] {
        assert!(
            !processes_have_exited(&[pid]),
            "process {pid} must be alive before the forced workbench shutdown"
        );
    }
    let scope = detached_scope_unit(watchdog, host, job);

    fixture
        .0
        .kill()
        .expect("force-kill the unresponsive-Host workbench");
    fixture
        .0
        .wait()
        .expect("reap the unresponsive-Host workbench");
    wait_until(
        || processes_have_exited(&[watchdog, host, job]),
        "watchdog and detached Agent job cleanup after Host shutdown stops responding",
    );
    assert_scope_absent(&scope);

    let mut restarted = DshSupervisor::system()
        .start(&restart_runtime_config(&directory))
        .expect("a fresh real Host starts after unresponsive Host cleanup");
    assert_ne!(restarted.id(), watchdog, "a new watchdog must be created");
    restarted.shutdown().expect("stop the restarted real Host");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unresponsive_host_shutdown_cleans_detached_job() {
    let directory = TestDirectory::new("unresponsive-host-shutdown");
    let mut config = unresponsive_runtime_config(&directory);
    config.shutdown_timeout = Duration::from_millis(100);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start unresponsive Host");
    let watchdog = runtime.id();
    let mut cleanup = TestTreeGuard::containing(watchdog, &[watchdog]);
    let client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect unresponsive-Host client");
    let session_id = create_session_at(&client, &directory.path).await;
    client
        .prompt(
            &session_id,
            "unresponsive-host-shutdown-request",
            "METIS_RESTART_LIVE",
        )
        .await
        .expect("start live work before unresponsive Host shutdown");
    let job = wait_for_job_pid(&directory.path);
    wait_until(
        || {
            runtime
                .output_snapshot()
                .contains("[metis-test] assistant stream held open")
        },
        "unresponsive-Host assistant stream",
    );
    let host = read_pid(&config.home.join("metis-test-host.pid"));
    cleanup.pids.push(host);
    cleanup.pids.push(job);
    for pid in [watchdog, host, job] {
        assert!(
            !processes_have_exited(&[pid]),
            "process {pid} must be alive before unresponsive Host shutdown"
        );
    }
    let scope = detached_scope_unit(watchdog, host, job);

    let error = runtime
        .shutdown()
        .expect_err("outer shutdown deadline must expire while the Host is unresponsive");
    assert!(
        matches!(error, Error::RuntimeShutdownTimeout(_)),
        "unexpected shutdown error: {error:?}"
    );
    wait_until(
        || processes_have_exited(&[watchdog, host, job]),
        "detached Agent job cleanup after outer shutdown escalation",
    );
    assert_scope_absent(&scope);

    let mut restarted = DshSupervisor::system()
        .start(&restart_runtime_config(&directory))
        .expect("a fresh real Host starts after outer shutdown escalation");
    assert_ne!(restarted.id(), watchdog, "a new watchdog must be created");
    restarted.shutdown().expect("stop the restarted real Host");
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
            host_entrypoint: script,
            home: directory.join("dsh-home"),
            startup_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(15),
        })
        .expect("fixture workbench starts its runtime");
    publish_watchdog_pid(&directory, runtime.id());

    // Keep Drop from running: the outer test terminates this process directly.
    // Keeping the runtime binding alive is intentional.
    loop {
        thread::park();
    }
}

#[cfg(unix)]
fn run_real_crash_fixture_parent() {
    let directory = PathBuf::from(env::var_os(CRASH_HOME_ENV).expect("crash fixture home"));
    let config = DshRuntimeConfig {
        metis_executable: PathBuf::from(env!("CARGO_BIN_EXE_metis")),
        node_executable: PathBuf::from("node"),
        host_entrypoint: real_dsh_entrypoint(),
        home: directory.join("dsh-home"),
        startup_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_secs(15),
    };
    let tokio = tokio::runtime::Runtime::new().expect("create crash fixture async runtime");
    let (runtime, client) = tokio.block_on(async {
        let runtime = DshSupervisor::system()
            .start(&config)
            .expect("start real Host in the fixture workbench");
        let client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
            .await
            .expect("connect real-Host crash fixture client");
        let session_id = create_session_at(&client, &directory).await;
        client
            .prompt(
                &session_id,
                "controlled-parent-death-request",
                "METIS_RESTART_LIVE",
            )
            .await
            .expect("start real Agent work before the workbench crash");
        let _ = wait_for_job_pid(&directory);
        wait_until(
            || {
                runtime
                    .output_snapshot()
                    .contains("[metis-test] assistant stream held open")
            },
            "real Agent assistant stream",
        );
        let running = client
            .state(&session_id)
            .await
            .expect("read real Agent state before the workbench crash");
        assert_eq!(running["running"], true);
        (runtime, client)
    });
    publish_watchdog_pid(&directory, runtime.id());

    // The outer test kills this process directly, so none of these owners run Drop.
    let _keep_alive = (tokio, runtime, client);
    loop {
        thread::park();
    }
}

fn runtime_config(directory: &TestDirectory, host_entrypoint: PathBuf) -> DshRuntimeConfig {
    DshRuntimeConfig {
        metis_executable: PathBuf::from(env!("CARGO_BIN_EXE_metis")),
        node_executable: PathBuf::from("node"),
        host_entrypoint,
        home: directory.path.join("dsh-home"),
        startup_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_secs(15),
    }
}

fn real_dsh_entrypoint() -> PathBuf {
    workspace_root().join("packages/metis-host/lib/host.js")
}

fn approval_runtime_config(directory: &TestDirectory) -> DshRuntimeConfig {
    fixture_runtime_config(directory, "approval-fixture.js")
}

fn restart_runtime_config(directory: &TestDirectory) -> DshRuntimeConfig {
    fixture_runtime_config(directory, "restart-fixture.js")
}

#[cfg(target_os = "linux")]
fn unresponsive_runtime_config(directory: &TestDirectory) -> DshRuntimeConfig {
    fixture_runtime_config(directory, "unresponsive-host-fixture.js")
}

fn fixture_runtime_config(directory: &TestDirectory, fixture: &str) -> DshRuntimeConfig {
    let config = runtime_config(directory, real_dsh_entrypoint());
    fs::create_dir_all(&config.home).expect("create fixture DSH_HOME");
    let fixture = workspace_root()
        .join("packages/metis-host/test")
        .join(fixture);
    let fixture = fixture.to_string_lossy().replace('\'', "''");
    fs::write(
        config.home.join("cordis.patch.yml"),
        format!("- insert:\n    - id: metis-test-fixture\n      name: '{fixture}'\n"),
    )
    .expect("write fixture home patch");
    config
}

fn workspace_root() -> PathBuf {
    // Deliberately not canonicalized: on Windows `canonicalize` returns a
    // verbatim `\\?\` path, which Node's module loader reduces to the bare
    // drive root and rejects with EISDIR.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .parse()
        .expect("PID file contains an integer")
}

#[cfg(unix)]
fn wait_for_job_pid(directory: &Path) -> u32 {
    let pid_path = directory.join("metis-test-job.pid");
    let error_path = directory.join("metis-test-job.error");
    wait_until(
        || {
            if let Ok(error) = fs::read_to_string(&error_path) {
                panic!("controlled bash job failed: {error}");
            }
            fs::read_to_string(&pid_path)
                .ok()
                .and_then(|pid| pid.trim().parse::<u32>().ok())
                .is_some()
        },
        "controlled bash PID file",
    );
    read_pid(&pid_path)
}

fn publish_watchdog_pid(directory: &Path, pid: u32) {
    let pending = directory.join("ready.pending");
    fs::write(&pending, pid.to_string()).expect("write pending watchdog PID");
    fs::rename(&pending, directory.join("ready")).expect("atomically publish watchdog PID");
}

#[cfg(target_os = "linux")]
fn detached_scope_unit(watchdog: u32, host: u32, job: u32) -> String {
    let stat =
        fs::read_to_string(format!("/proc/{job}/stat")).expect("read Agent job process stat");
    let end = stat.rfind(')').expect("process stat contains its command");
    let process_group = stat[end + 2..]
        .split_whitespace()
        .nth(2)
        .and_then(|value| value.parse::<u32>().ok())
        .expect("process stat contains a process group");
    assert_ne!(
        process_group, watchdog,
        "Agent job must be detached from the watchdog process group"
    );

    let cgroup = fs::read_to_string(format!("/proc/{job}/cgroup"))
        .expect("read Agent job cgroup membership");
    let unit = cgroup
        .lines()
        .find_map(|line| line.split_once("::/").map(|(_, path)| path))
        .and_then(|path| path.rsplit('/').next())
        .filter(|unit| unit.strip_suffix(".scope").is_some())
        .expect("Agent job must belong to a systemd scope");
    let prefix = format!("dsh-subprocess-{host}-");
    let suffix = unit
        .strip_prefix(&prefix)
        .expect("Agent job scope must be owned by the current Host");
    assert!(
        suffix.strip_suffix(".scope").is_some_and(|name| {
            name.len() == 12 && name.chars().all(|character| character.is_ascii_hexdigit())
        }),
        "unexpected Agent job scope: {unit}"
    );
    unit.to_owned()
}

#[cfg(target_os = "linux")]
fn assert_scope_absent(unit: &str) {
    let output = Command::new("systemctl")
        .args(["--user", "show", unit])
        .output()
        .expect("verify scope removal with systemctl");
    let details = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        details.contains("LoadState=not-found")
            || details.contains("NoSuchUnit")
            || (!output.status.success()
                && (details.contains("could not be found")
                    || details.contains("not found")
                    || details.contains("not loaded"))),
        "scope {unit} still exists or could not be checked: {details}"
    );
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
        if path.exists() {
            fs::remove_dir_all(&path).expect("remove stale test directory");
        }
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

fn bash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn run_git(project: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(project)
        .output()
        .expect("run test Git command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn create_session_at(client: &DshClient, directory: &Path) -> String {
    let project = client
        .open_project(&directory.to_string_lossy())
        .await
        .expect("open Session project");
    client
        .create_session(&project.project_id)
        .await
        .expect("create project-attached Session")
}

async fn next_stream_frame(stream: &mut DshStream) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timed out waiting for Session stream")
        .expect("read Session stream")
        .expect("Session stream ended unexpectedly")
}

async fn next_snapshot(stream: &mut DshStream) -> serde_json::Value {
    let snapshot = next_stream_frame(stream).await;
    assert_eq!(snapshot["type"], "snapshot");
    snapshot
}

async fn apply_through(
    stream: &mut DshStream,
    journal: &mut SessionJournal,
    event_type: &str,
) -> Vec<serde_json::Value> {
    let mut delivered = Vec::new();
    loop {
        let frame = next_stream_frame(stream).await;
        let events = journal
            .apply(&frame)
            .expect("apply ordered Session frame")
            .into_events();
        let found = events
            .iter()
            .any(|event| event["type"].as_str() == Some(event_type));
        delivered.extend(events);
        if found {
            return delivered;
        }
    }
}

async fn next_approval(client: &mut DshClient) -> ApprovalRequest {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), client.next_event())
            .await
            .expect("timed out waiting for Host approval")
            .expect("read Host event");
        if let HostEvent::Approval(approval) = event {
            return approval;
        }
    }
}

fn logical_prompt_message_ids(snapshot: &serde_json::Value, request_id: &str) -> Vec<String> {
    logical_prompt_event_ids(
        snapshot["records"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|record| &record["event"]),
        request_id,
    )
}

fn logical_prompt_event_ids<'a>(
    events: impl IntoIterator<Item = &'a serde_json::Value>,
    request_id: &str,
) -> Vec<String> {
    let mut ids = Vec::new();
    for event in events {
        let messages: Vec<&serde_json::Value> = match event["type"].as_str() {
            Some("user/message") => vec![&event["data"]],
            Some("agent/inbox/spliced") => event["data"]["inserted"]
                .as_array()
                .map_or_default(|messages| messages.iter().collect()),
            _ => Vec::new(),
        };
        for message in messages {
            if message
                .pointer("/source/rpcId")
                .and_then(|value| value.as_str())
                != Some(request_id)
            {
                continue;
            }
            let id = message["id"]
                .as_str()
                .expect("a prompt record must carry its message id")
                .to_owned();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

#[test]
fn prompt_dedup_counts_logical_messages_across_inbox_and_user_events() {
    let message = serde_json::json!({
        "id": "message-1",
        "source": { "kind": "user", "rpcId": "request-1" },
    });
    let snapshot = serde_json::json!({ "records": [
        { "event": { "type": "agent/inbox/spliced", "data": {
            "inserted": [message.clone()],
        }}},
        { "event": { "type": "user/message", "data": message }},
    ]});
    assert_eq!(
        logical_prompt_message_ids(&snapshot, "request-1"),
        vec!["message-1".to_owned()]
    );

    let mut duplicate = snapshot;
    duplicate["records"].as_array_mut().unwrap().push(
        serde_json::json!({ "event": { "type": "user/message", "data": {
            "id": "message-2",
            "source": { "kind": "user", "rpcId": "request-1" },
        }}}),
    );
    assert_eq!(logical_prompt_message_ids(&duplicate, "request-1").len(), 2);
}

#[tokio::test]
async fn public_web_client_works_against_real_host_without_a_model() {
    let directory = TestDirectory::new("public-web-client");
    let config = runtime_config(&directory, real_dsh_entrypoint());
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start real Host");
    let client_config = DshClientConfig {
        request_timeout: Duration::from_secs(20),
        stream_open_timeout: Duration::from_secs(20),
    };

    let invalid_token = "A".repeat(43);
    let invalid_auth = format!("{}connect?token={invalid_token}", runtime.web_url());
    let error = DshClient::connect(&invalid_auth, client_config)
        .await
        .err()
        .expect("invalid launch token must be rejected");
    assert!(matches!(
        error,
        ClientError::Authentication { status }
            if status == reqwest::StatusCode::UNAUTHORIZED
    ));
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains(&invalid_token));
    assert!(!rendered.contains(&invalid_auth));

    let client = DshClient::connect(runtime.auth_url(), client_config)
        .await
        .expect("authenticate and receive $events ready");
    assert_ne!(client.client_id(), "");
    let session_id = create_session_at(&client, &directory.path).await;
    let mut follow = client
        .follow(&session_id)
        .await
        .expect("open Session follow stream");
    let snapshot = follow
        .next()
        .await
        .expect("read opening snapshot")
        .expect("snapshot precedes stream end");
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["header"]["id"], session_id);
    let state = client.state(&session_id).await.expect("read Session state");
    assert!(state.is_object());

    let missing = client.state("metis-session-that-does-not-exist").await;
    assert!(matches!(
        missing,
        Err(ClientError::Remote { ref code, .. }) if !code.is_empty()
    ));
    client
        .cancel(&session_id)
        .await
        .expect("Host accepts cancellation");
    follow.cancel();
    client.close().await.expect("close WebSocket task");

    let dropped_client = DshClient::connect(runtime.auth_url(), client_config)
        .await
        .expect("connect a client retained for Drop cleanup");
    let mut retained_follow = dropped_client
        .follow(&session_id)
        .await
        .expect("open follow that outlives its client");
    let snapshot = retained_follow
        .next()
        .await
        .expect("read retained follow snapshot");
    assert!(matches!(snapshot, Some(value) if value["type"] == "snapshot"));
    drop(dropped_client);
    let closed = tokio::time::timeout(Duration::from_secs(1), retained_follow.next()).await;
    assert!(
        matches!(closed, Ok(Err(ClientError::ConnectionClosed))),
        "dropping the client must terminate a retained follow stream"
    );
    runtime.shutdown().expect("stop real Host");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn project_and_git_workflows_use_real_host() {
    let directory = TestDirectory::new("project-git");
    let project = directory.path.join("repo");
    fs::create_dir(&project).expect("create Git project");
    fs::write(project.join("tracked.txt"), "before\n").expect("write tracked file");
    fs::write(project.join("deleted.txt"), "delete me\n").expect("write deleted file");
    fs::write(project.join("rename-old.txt"), "rename me\n").expect("write renamed file");
    run_git(&project, &["init", "--quiet"]);
    run_git(&project, &["config", "user.name", "Metis Test"]);
    run_git(&project, &["config", "user.email", "metis@example.invalid"]);
    run_git(&project, &["add", "--", "."]);
    run_git(&project, &["commit", "--quiet", "-m", "initial"]);

    let outside = directory.write("outside.txt", "outside\n");
    let config = runtime_config(&directory, real_dsh_entrypoint());
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start real Host for project and Git calls");
    let client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect project and Git client");

    let opened = client
        .open_project(&project.to_string_lossy())
        .await
        .expect("open real Git project");
    assert_eq!(Path::new(&opened.path).file_name().unwrap(), "repo");
    assert_eq!(opened.recent_session_ids, Vec::<String>::new());
    assert!(!opened.has_more_sessions);
    let reopened = client
        .open_project(&project.join(".").to_string_lossy())
        .await
        .expect("reopening a canonical project is idempotent");
    assert_eq!(reopened.project_id, opened.project_id);
    let current = client
        .get_project(&opened.project_id)
        .await
        .expect("get opened project");
    assert_eq!(current.project_id, opened.project_id);
    assert_eq!(
        client
            .git_status(&opened.project_id)
            .await
            .expect("clean status"),
        ""
    );
    assert_eq!(
        client
            .git_diff(&opened.project_id)
            .await
            .expect("clean diff"),
        ""
    );

    let file = client
        .read_project_file(&opened.project_id, "tracked.txt")
        .await
        .expect("read project file");
    assert_eq!(file, "before\n");
    let session_id = client
        .create_session(&opened.project_id)
        .await
        .expect("create Session attached to project");
    let current = client
        .get_project(&opened.project_id)
        .await
        .expect("get project with attached Session");
    assert_eq!(current.recent_session_ids, vec![session_id]);

    let outside_path = outside.to_string_lossy().into_owned();
    for path in ["../outside.txt", outside_path.as_str()] {
        let error = client
            .read_project_file(&opened.project_id, path)
            .await
            .expect_err("project escapes must be rejected");
        assert!(
            matches!(error, ClientError::Remote { ref code, .. } if code == "project/path-outside"),
            "unexpected path escape error: {error}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, project.join("outside-link.txt"))
            .expect("create external symlink");
        let error = client
            .read_project_file(&opened.project_id, "outside-link.txt")
            .await
            .expect_err("external symlinks must be rejected");
        assert!(
            matches!(error, ClientError::Remote { ref code, .. } if code == "project/path-outside"),
            "unexpected symlink escape error: {error}"
        );
    }

    fs::remove_file(project.join("deleted.txt")).expect("delete tracked file");
    fs::rename(
        project.join("rename-old.txt"),
        project.join("rename-new.txt"),
    )
    .expect("rename tracked file");
    fs::write(project.join("added.txt"), "new file\n").expect("write new file");
    fs::write(project.join("tracked.txt"), "after\n").expect("stage tracked modification");
    run_git(&project, &["add", "-A"]);
    fs::write(project.join("tracked.txt"), "after\nstaged and unstaged\n")
        .expect("modify tracked file");

    let status = client
        .git_status(&opened.project_id)
        .await
        .expect("read real Git status");
    let entries: Vec<&str> = status.split('\0').collect();
    assert!(entries.contains(&"A  added.txt"), "status: {status:?}");
    assert!(entries.contains(&"D  deleted.txt"), "status: {status:?}");
    assert!(entries.contains(&"R  rename-new.txt"), "status: {status:?}");
    assert!(entries.contains(&"rename-old.txt"), "status: {status:?}");
    assert!(entries.contains(&"MM tracked.txt"), "status: {status:?}");

    let patch = client
        .git_diff(&opened.project_id)
        .await
        .expect("read real Git patch");
    assert!(patch.contains("new file mode"), "patch: {patch}");
    assert!(patch.contains("deleted file mode"), "patch: {patch}");
    assert!(
        patch.contains("rename from rename-old.txt"),
        "patch: {patch}"
    );
    assert!(patch.contains("rename to rename-new.txt"), "patch: {patch}");
    assert!(patch.contains("+staged and unstaged"), "patch: {patch}");

    let unborn = directory.path.join("unborn");
    fs::create_dir(&unborn).expect("create unborn Git project");
    run_git(&unborn, &["init", "--quiet"]);
    let unborn_project = client
        .open_project(&unborn.to_string_lossy())
        .await
        .expect("open unborn Git project");
    assert_eq!(
        client
            .git_status(&unborn_project.project_id)
            .await
            .expect("empty unborn status"),
        ""
    );
    assert_eq!(
        client
            .git_diff(&unborn_project.project_id)
            .await
            .expect("empty unborn diff"),
        ""
    );
    fs::write(unborn.join("first.txt"), "first\n").expect("write first unborn file");
    run_git(&unborn, &["add", "--", "first.txt"]);
    let patch = client
        .git_diff(&unborn_project.project_id)
        .await
        .expect("diff staged file against the empty tree");
    assert!(patch.contains("new file mode"), "patch: {patch}");
    assert!(patch.contains("+first"), "patch: {patch}");

    fs::write(project.join("too-large.txt"), vec![b'x'; 513 * 1024])
        .expect("write oversized text file");
    let error = client
        .read_project_file(&opened.project_id, "too-large.txt")
        .await
        .expect_err("oversized files must be rejected");
    assert!(
        matches!(error, ClientError::Remote { ref code, .. } if code == "project/read-failed"),
        "unexpected oversized-file error: {error}"
    );

    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let fifo = project.join("fifo");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("create project FIFO");
        assert!(status.success());
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client.read_project_file(&opened.project_id, "fifo"),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(ClientError::Remote { ref code, .. })) if code == "project/read-failed"),
            "FIFO must reject promptly: {result:?}"
        );

        let invalid_name = OsString::from_vec(vec![b'f', 0xff]);
        fs::write(project.join(invalid_name), "invalid UTF-8 name\n")
            .expect("write non-UTF-8 filename");
        let error = client
            .git_status(&opened.project_id)
            .await
            .expect_err("non-UTF-8 Git output must not be lossy");
        assert!(
            matches!(error, ClientError::Remote { ref code, .. } if code == "git/command-failed"),
            "unexpected non-UTF-8 Git error: {error}"
        );

        let replaced = directory.path.join("replaced");
        fs::create_dir(&replaced).expect("create replaceable project");
        let replaced_project = client
            .open_project(&replaced.to_string_lossy())
            .await
            .expect("open replaceable project");
        fs::rename(&replaced, directory.path.join("moved")).expect("move project root");
        std::os::unix::fs::symlink(&directory.path, &replaced)
            .expect("replace project root with an external symlink");
        let error = client
            .read_project_file(&replaced_project.project_id, "outside.txt")
            .await
            .expect_err("replacement project roots must be rejected");
        assert!(
            matches!(error, ClientError::Remote { ref code, .. } if code == "project/unavailable"),
            "unexpected replacement-root error: {error}"
        );
    }

    let error = client
        .git_status("missing-project")
        .await
        .expect_err("unknown projects must not reach Git");
    assert!(
        matches!(error, ClientError::Remote { ref code, .. } if code == "project/not-found"),
        "unexpected unknown-project error: {error}"
    );
    client.close().await.expect("close project and Git client");
    runtime.shutdown().expect("stop project and Git Host");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn multi_client_reconnect_preserves_history_and_approval_order() {
    let directory = TestDirectory::new("multi-client-reconnect");
    let config = approval_runtime_config(&directory);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start multi-client Host");
    let client_a = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect first client");
    let first_client_id = client_a.client_id().to_owned();
    let mut client_b = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect second client with the same launch URL");
    assert_ne!(client_a.client_id(), client_b.client_id());

    let project = client_a
        .open_project(&directory.path.to_string_lossy())
        .await
        .expect("open multi-client project");
    let session_id = client_a
        .create_session(&project.project_id)
        .await
        .expect("create multi-client Session");
    let mut follow_a = client_a.follow(&session_id).await.expect("follow with A");
    let mut follow_b = client_b.follow(&session_id).await.expect("follow with B");
    let mut journal_a = SessionJournal::new(&session_id);
    let mut journal_b = SessionJournal::new(&session_id);
    journal_a
        .apply(&next_snapshot(&mut follow_a).await)
        .expect("apply A baseline");
    journal_b
        .apply(&next_snapshot(&mut follow_b).await)
        .expect("apply B baseline");

    client_a
        .rename_session(&session_id, "one")
        .await
        .expect("write first durable title");
    let events_a = apply_through(&mut follow_a, &mut journal_a, "session/title").await;
    let events_b = apply_through(&mut follow_b, &mut journal_b, "session/title").await;
    assert_eq!(events_a.last().unwrap()["data"]["title"], "one");
    assert_eq!(events_b.last().unwrap()["data"]["title"], "one");
    assert_eq!(
        events_a.last().unwrap()["seq"],
        events_b.last().unwrap()["seq"]
    );
    let old_cursor = journal_a.cursor().unwrap();

    drop(follow_a);
    drop(client_a);
    client_b
        .rename_session(&session_id, "two")
        .await
        .expect("write first disconnected event");
    apply_through(&mut follow_b, &mut journal_b, "session/title").await;
    client_b
        .rename_session(&session_id, "three")
        .await
        .expect("write second disconnected event");
    apply_through(&mut follow_b, &mut journal_b, "session/title").await;

    let mut reconnected_a = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("reconnect A with a fresh Peer");
    assert_ne!(reconnected_a.client_id(), first_client_id);
    let mut reconnected_follow = reconnected_a
        .follow(&session_id)
        .await
        .expect("open A's replacement follow");
    let snapshot = next_snapshot(&mut reconnected_follow).await;
    let replacement = journal_a
        .apply(&snapshot)
        .expect("replace reconnect window")
        .into_events();
    let missed: Vec<_> = replacement
        .into_iter()
        .filter(|event| event["seq"].as_i64().unwrap() > old_cursor)
        .collect();
    assert_eq!(
        missed
            .iter()
            .map(|event| event["seq"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![old_cursor + 1, old_cursor + 2]
    );
    assert_eq!(
        missed
            .iter()
            .map(|event| event["data"]["title"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["two", "three"]
    );
    let page = client_b
        .session_page(&session_id, old_cursor, None)
        .await
        .expect("read an explicit historical page");
    assert_eq!(page["cursor"], old_cursor);
    assert_eq!(
        page["records"].as_array().unwrap().last().unwrap()["event"]["seq"],
        old_cursor
    );

    client_b
        .rename_session(&session_id, "after-reconnect")
        .await
        .expect("write handoff event");
    let handoff_a = apply_through(&mut reconnected_follow, &mut journal_a, "session/title").await;
    let handoff_b = apply_through(&mut follow_b, &mut journal_b, "session/title").await;
    assert_eq!(
        handoff_a.last().unwrap()["seq"],
        handoff_b.last().unwrap()["seq"]
    );

    let request_id = "multi-client-approval-request";
    reconnected_a
        .prompt(&session_id, request_id, "METIS_TEST_APPROVAL_CANCEL")
        .await
        .expect("trigger deterministic approval");
    let (approval_a, approval_b) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            next_approval(&mut reconnected_a),
            next_approval(&mut client_b)
        )
    })
    .await
    .expect("both clients receive the same approval");
    assert_eq!(approval_a.event_id, approval_b.event_id);
    assert_eq!(approval_a.agent_id, session_id);
    assert_eq!(approval_b.agent_id, session_id);
    reconnected_a
        .answer(&approval_a, ApprovalOutcome::AllowedOnce)
        .await
        .expect("first answer wins");
    let cancelled = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let HostEvent::Cancelled { event_id } = client_b
                .next_event()
                .await
                .expect("read approval cancellation")
            {
                break event_id;
            }
        }
    })
    .await
    .expect("losing client receives cancellation");
    assert_eq!(cancelled, approval_a.event_id);
    client_b
        .answer(&approval_b, ApprovalOutcome::Rejected)
        .await
        .expect("a late answer from the losing online client is a no-op");

    let approval_events_a =
        apply_through(&mut reconnected_follow, &mut journal_a, "approval/decided").await;
    let approval_events_b = apply_through(&mut follow_b, &mut journal_b, "approval/decided").await;
    let seqs_a: Vec<i64> = approval_events_a
        .iter()
        .map(|event| event["seq"].as_i64().unwrap())
        .collect();
    let seqs_b: Vec<i64> = approval_events_b
        .iter()
        .map(|event| event["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs_a, seqs_b);
    for events in [&approval_events_a, &approval_events_b] {
        assert_eq!(
            logical_prompt_event_ids(events.iter(), request_id).len(),
            1,
            "events: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "approval/asked")
                .count(),
            1
        );
        let decided: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "approval/decided")
            .collect();
        assert_eq!(decided.len(), 1, "events: {events:?}");
        assert_eq!(decided[0]["data"]["outcome"], "allowed-once");
    }
    let settled = client_b
        .wait(&session_id, Duration::from_secs(10))
        .await
        .expect("wait for cancelled turn to settle");
    assert_eq!(settled["state"]["running"], false);
    client_b
        .rename_session(&session_id, "approval-settled")
        .await
        .expect("write a barrier after the late approval answer");
    let mut final_follow = client_b
        .follow(&session_id)
        .await
        .expect("read final approval history");
    let final_snapshot = next_snapshot(&mut final_follow).await;
    let mut final_journal = SessionJournal::new(&session_id);
    let final_events = final_journal
        .apply(&final_snapshot)
        .expect("apply final approval history")
        .into_events();
    assert_eq!(
        logical_prompt_event_ids(final_events.iter(), request_id).len(),
        1
    );
    let final_decisions: Vec<_> = final_events
        .iter()
        .filter(|event| event["type"] == "approval/decided")
        .collect();
    assert_eq!(final_decisions.len(), 1, "final events: {final_events:?}");
    assert_eq!(final_decisions[0]["data"]["outcome"], "allowed-once");

    let mut paged_follow = client_b
        .follow_with_limit(&session_id, 1)
        .await
        .expect("open a deliberately paginated follow");
    let paged_snapshot = next_snapshot(&mut paged_follow).await;
    assert_eq!(paged_snapshot["hasMore"], true);
    let mut paged_journal = SessionJournal::new(&session_id);
    let mut complete_history = paged_journal
        .apply(&paged_snapshot)
        .expect("apply limited history window")
        .into_events();
    let cursor = paged_journal.cursor().unwrap();
    let mut page_count = 0;
    while paged_journal.has_more() {
        page_count += 1;
        assert!(page_count < 100, "history pagination did not terminate");
        let before = paged_journal.window_start().unwrap();
        let page = client_b
            .session_page(&session_id, cursor, Some(before))
            .await
            .expect("read older history at a fixed cutoff");
        let older = paged_journal
            .prepend_page(&page)
            .expect("prepend contiguous older history");
        complete_history.splice(0..0, older);
    }
    assert_eq!(
        complete_history
            .iter()
            .map(|event| event["seq"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        (0..=cursor).collect::<Vec<_>>()
    );
    assert_eq!(
        logical_prompt_event_ids(complete_history.iter(), request_id).len(),
        1
    );
    paged_follow.cancel();
    final_follow.cancel();

    reconnected_follow.cancel();
    follow_b.cancel();
    reconnected_a.close().await.expect("close reconnected A");
    client_b.close().await.expect("close B");
    runtime.shutdown().expect("stop multi-client Host");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn host_restart_recovers_history_and_settles_pending_approval() {
    let directory = TestDirectory::new("host-restart-recovery");
    let config = approval_runtime_config(&directory);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start initial recovery Host");
    let mut client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect initial recovery client");
    let project = client
        .open_project(&directory.path.to_string_lossy())
        .await
        .expect("open persistent project");
    let session_id = client
        .create_session(&project.project_id)
        .await
        .expect("create persistent Session");
    let mut follow = client
        .follow(&session_id)
        .await
        .expect("follow persistent Session");
    let mut journal = SessionJournal::new(&session_id);
    journal
        .apply(&next_snapshot(&mut follow).await)
        .expect("apply initial recovery baseline");
    let request_id = "restart-pending-approval-request";
    client
        .prompt(&session_id, request_id, "METIS_TEST_APPROVAL")
        .await
        .expect("trigger approval left pending across shutdown");
    let approval = next_approval(&mut client).await;
    apply_through(&mut follow, &mut journal, "approval/asked").await;
    drop(follow);
    drop(client);
    runtime.shutdown().expect("gracefully stop initial Host");

    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("restart Host with the same DSH_HOME");
    let client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect client after Host restart");
    let restored = client
        .open_project(&directory.path.to_string_lossy())
        .await
        .expect("restore persistent project");
    assert_eq!(restored.project_id, project.project_id);
    assert!(restored.recent_session_ids.contains(&session_id));
    let state = client
        .state(&session_id)
        .await
        .expect("read cold Session state after restart");
    assert_eq!(state["active"], false);
    assert_eq!(state["running"], false);
    assert_eq!(state["pendingInput"], serde_json::json!([]));

    let mut follow = client
        .follow(&session_id)
        .await
        .expect("follow restored Session");
    let snapshot = next_snapshot(&mut follow).await;
    assert_eq!(snapshot["assistantStream"]["revision"], 0);
    assert!(snapshot["assistantStream"].get("activeAttempt").is_none());
    let mut restored_journal = SessionJournal::new(&session_id);
    let history = restored_journal
        .apply(&snapshot)
        .expect("apply restored history")
        .into_events();
    assert_eq!(
        logical_prompt_event_ids(history.iter(), request_id).len(),
        1,
        "history: {history:?}"
    );
    assert_eq!(
        history
            .iter()
            .filter(|event| event["type"] == "approval/asked")
            .count(),
        1
    );
    let decided: Vec<_> = history
        .iter()
        .filter(|event| event["type"] == "approval/decided")
        .collect();
    assert_eq!(decided.len(), 1, "history: {history:?}");
    assert_eq!(decided[0]["data"]["outcome"], "unavailable");

    let error = client
        .answer(&approval, ApprovalOutcome::AllowedOnce)
        .await
        .expect_err("local generation guard must reject the old approval");
    assert!(matches!(error, ClientError::Protocol(_)));
    client
        .prompt(&session_id, request_id, "METIS_TEST_APPROVAL")
        .await
        .expect("retry the persisted request id");
    client
        .rename_session(&session_id, "restart-retry-settled")
        .await
        .expect("write a barrier after the persisted request retry");
    let state = client
        .state(&session_id)
        .await
        .expect("read settled retry state");
    assert_eq!(state["running"], false);
    assert_eq!(state["pendingInput"], serde_json::json!([]));
    let mut retry_follow = client
        .follow(&session_id)
        .await
        .expect("read final request-retry history");
    let retry_snapshot = next_snapshot(&mut retry_follow).await;
    let mut retry_journal = SessionJournal::new(&session_id);
    let retry_history = retry_journal
        .apply(&retry_snapshot)
        .expect("apply request-retry history")
        .into_events();
    assert_eq!(
        logical_prompt_event_ids(retry_history.iter(), request_id).len(),
        1
    );
    assert_eq!(
        retry_history
            .iter()
            .filter(|event| event["type"] == "approval/asked")
            .count(),
        1
    );
    let retry_decisions: Vec<_> = retry_history
        .iter()
        .filter(|event| event["type"] == "approval/decided")
        .collect();
    assert_eq!(retry_decisions.len(), 1);
    assert_eq!(retry_decisions[0]["data"]["outcome"], "unavailable");
    retry_follow.cancel();

    follow.cancel();
    client.close().await.expect("close recovery client");
    runtime.shutdown().expect("stop restarted Host");
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn host_restart_drops_live_turn_output_and_job() {
    let directory = TestDirectory::new("host-restart-live-work");
    let config = restart_runtime_config(&directory);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start controlled live-work Host");
    let client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect controlled live-work client");
    let project = client
        .open_project(&directory.path.to_string_lossy())
        .await
        .expect("open live-work project");
    let session_id = client
        .create_session(&project.project_id)
        .await
        .expect("create live-work Session");
    client
        .prompt(
            &session_id,
            "controlled-live-work-request",
            "METIS_RESTART_LIVE",
        )
        .await
        .expect("start controlled live work");
    let pid = wait_for_job_pid(&directory.path);
    wait_until(
        || {
            let output = runtime.output_snapshot();
            output.contains("[metis-test] bash process started")
                && output.contains("[metis-test] assistant stream held open")
        },
        "controlled bash and assistant stream",
    );

    let mut live_follow = client
        .follow(&session_id)
        .await
        .expect("follow active output");
    let live_snapshot = next_snapshot(&mut live_follow).await;
    let mut journal = SessionJournal::new(&session_id);
    let events = journal
        .apply(&live_snapshot)
        .expect("apply active live-work snapshot")
        .into_events();
    let registered = events
        .iter()
        .find(|event| event["type"] == "job/registered")
        .expect("job registration event");
    let job_id = registered["data"]["jobId"].as_str().unwrap().to_owned();
    assert_eq!(registered["data"]["owner"], session_id);
    let active = &live_snapshot["assistantStream"]["activeAttempt"];
    assert!(active.is_object(), "snapshot: {live_snapshot}");
    assert!(active["nextIndex"].as_u64().unwrap() > 0);
    assert!(active.to_string().contains("METIS_LIVE_CHUNK"));

    let running = client.state(&session_id).await.expect("read running state");
    assert_eq!(running["running"], true);
    drop(live_follow);
    drop(client);
    runtime.shutdown().expect("stop Host with live work");
    wait_until(
        || processes_have_exited(&[pid]),
        "controlled bash process cleanup",
    );

    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("restart controlled live-work Host");
    let client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect after live-work restart");
    let state = client
        .state(&session_id)
        .await
        .expect("read restored live-work state");
    assert_eq!(state["active"], false);
    assert_eq!(state["running"], false);
    let mut follow = client
        .follow(&session_id)
        .await
        .expect("follow restored live work");
    let snapshot = next_snapshot(&mut follow).await;
    assert_eq!(snapshot["assistantStream"]["revision"], 0);
    assert!(snapshot["assistantStream"].get("activeAttempt").is_none());
    let mut restored_journal = SessionJournal::new(&session_id);
    let history = restored_journal
        .apply(&snapshot)
        .expect("apply live-work history")
        .into_events();
    let registrations: Vec<_> = history
        .iter()
        .filter(|event| event["type"] == "job/registered")
        .collect();
    assert_eq!(registrations.len(), 1, "history: {history:?}");
    assert_eq!(registrations[0]["data"]["jobId"], job_id);
    assert!(
        history
            .iter()
            .filter(|event| matches!(
                event["type"].as_str(),
                Some("assistant/message" | "assistant/attempt")
            ))
            .any(|event| event.to_string().contains("METIS_LIVE_CHUNK")),
        "settled history must retain delivered output: {history:?}"
    );
    wait_until(
        || {
            runtime
                .output_snapshot()
                .contains(&format!("[metis-test] jobs {session_id} []"))
        },
        "empty job registry after Session resume",
    );
    let state = client
        .state(&session_id)
        .await
        .expect("read quiescent resumed state");
    assert_eq!(state["running"], false);
    let mut settled_follow = client
        .follow(&session_id)
        .await
        .expect("read settled assistant baseline");
    let settled_snapshot = next_snapshot(&mut settled_follow).await;
    assert_eq!(settled_snapshot["assistantStream"]["revision"], 0);
    assert!(
        settled_snapshot["assistantStream"]
            .get("activeAttempt")
            .is_none()
    );
    settled_follow.cancel();

    follow.cancel();
    client
        .close()
        .await
        .expect("close restored live-work client");
    runtime.shutdown().expect("stop restored live-work Host");
}

#[tokio::test]
async fn approval_fixture_round_trip_without_model() {
    let directory = TestDirectory::new("approval-fixture");
    let config = approval_runtime_config(&directory);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .expect("start Host with approval fixture");
    let mut client = DshClient::connect(runtime.auth_url(), DshClientConfig::default())
        .await
        .expect("connect approval fixture client");
    let session_id = create_session_at(&client, &directory.path).await;
    client
        .prompt(
            &session_id,
            "approval-fixture-request",
            "METIS_TEST_APPROVAL_CANCEL",
        )
        .await
        .expect("admit approval fixture prompt");
    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.next_event().await {
                Ok(HostEvent::Approval(approval)) => break approval,
                Ok(_) => {}
                Err(error) => panic!("approval fixture event stream failed: {error}"),
            }
        }
    })
    .await;
    let Ok(approval) = approval else {
        let output = runtime.output_snapshot();
        eprintln!(
            "approval fixture markers: loaded={}, requested={}, resolved={}",
            output.contains("[metis-test] approval fixture loaded"),
            output.contains("[metis-test] approval requested"),
            output.contains("[metis-test] approval resolved")
        );
        for line in output.lines().filter(|line| line.contains("[metis-test]")) {
            eprintln!("{line}");
        }
        panic!("approval fixture did not deliver an approval");
    };
    assert_eq!(approval.agent_id, session_id);
    assert_eq!(approval.request["toolName"], "bash");
    client
        .answer(&approval, ApprovalOutcome::AllowedOnce)
        .await
        .expect("answer approval fixture");
    wait_until(
        || {
            runtime
                .output_snapshot()
                .contains("[metis-test] approval resolved")
        },
        "approval fixture resolution",
    );
    client.close().await.expect("close approval fixture client");
    runtime.shutdown().expect("stop approval fixture Host");
}

#[tokio::test]
#[ignore = "requires METIS_TEST_REAL_MODEL=1 and configured dsh model credentials"]
#[allow(clippy::too_many_lines)]
async fn real_model_prompt_retry_streams_persists_and_cancels() {
    const REQUEST_ID: &str = "metis-rust-real-model-request-v1";
    const SECOND_REQUEST_ID: &str = "metis-rust-real-model-bash-sleep-v1";
    const MARKER: &str = "METIS_RUST_OK";

    assert_eq!(
        env::var("METIS_TEST_REAL_MODEL").ok().as_deref(),
        Some("1"),
        "explicitly set METIS_TEST_REAL_MODEL=1; model credentials come from the dsh environment"
    );
    let directory = TestDirectory::new("real-model-web-client");
    let config = approval_runtime_config(&directory);
    let mut runtime = DshSupervisor::system()
        .start(&config)
        .unwrap_or_else(|_| panic!("real-model Host startup failed"));
    let client_config = DshClientConfig {
        request_timeout: Duration::from_secs(120),
        stream_open_timeout: Duration::from_secs(30),
    };
    let mut client = DshClient::connect(runtime.auth_url(), client_config)
        .await
        .unwrap_or_else(|_| panic!("real-model client connection failed"));
    eprintln!("real-model: client connected");
    let session_id = create_session_at(&client, &directory.path).await;
    eprintln!("real-model: session created");
    let mut follow = client
        .follow(&session_id)
        .await
        .unwrap_or_else(|_| panic!("real-model follow failed"));
    let opening = follow.next().await;
    assert!(
        matches!(opening, Ok(Some(value)) if value["type"] == "snapshot"),
        "real-model follow omitted its snapshot"
    );

    for _ in 0..2 {
        client
            .prompt(
                &session_id,
                REQUEST_ID,
                "Reply with exactly METIS_RUST_OK and nothing else. Do not use tools.",
            )
            .await
            .unwrap_or_else(|_| panic!("real-model prompt admission failed"));
    }

    eprintln!("real-model: deduplicated prompt admitted");
    let mut saw_assistant_chunk = false;
    let mut saw_persisted_output = false;
    let mut stream_closed = false;
    let mut stream_failed = false;
    let mut frame_count = 0_u64;
    let mut event_types = Vec::new();
    let mut assistant_frame_types = Vec::new();
    let mut turn_end_kinds = Vec::new();
    let mut failure_codes = Vec::new();
    let observed = tokio::time::timeout(Duration::from_secs(120), async {
        while !(saw_assistant_chunk && saw_persisted_output) {
            let frame = match follow.next().await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    stream_closed = true;
                    return false;
                }
                Err(_) => {
                    stream_failed = true;
                    return false;
                }
            };
            frame_count += 1;
            if frame["type"] == "assistant-stream" {
                let kind = frame["frame"]["type"]
                    .as_str()
                    .unwrap_or("missing")
                    .to_owned();
                if !assistant_frame_types.contains(&kind) {
                    assistant_frame_types.push(kind);
                }
                if frame["frame"]["type"] == "chunk" {
                    saw_assistant_chunk = true;
                }
            }
            if frame["type"] != "event" {
                continue;
            }
            let event_type = frame["event"]["type"]
                .as_str()
                .unwrap_or("missing")
                .to_owned();
            if !event_types.contains(&event_type) {
                event_types.push(event_type.clone());
            }
            if event_type == "turn/end" {
                let kind = frame["event"]["data"]["reason"]["kind"]
                    .as_str()
                    .unwrap_or("missing")
                    .to_owned();
                turn_end_kinds.push(kind);
            }
            for pointer in [
                "/event/data/error/code",
                "/event/data/reason/error/code",
                "/event/data/failure/code",
            ] {
                if let Some(code) = frame.pointer(pointer).and_then(|value| value.as_str()) {
                    failure_codes.push(code.to_owned());
                }
            }
            if event_type == "assistant/message" && frame.to_string().contains(MARKER) {
                saw_persisted_output = true;
            }
            if event_type == "turn/end" && !(saw_assistant_chunk && saw_persisted_output) {
                return false;
            }
        }
        true
    })
    .await;
    if !matches!(observed, Ok(true)) {
        eprintln!(
            "real-model diagnostic: frames={frame_count}, assistant_chunk={saw_assistant_chunk}, persisted_marker={saw_persisted_output}, stream_closed={stream_closed}, stream_failed={stream_failed}"
        );
        eprintln!(
            "real-model diagnostic: event_types={event_types:?}, assistant_frames={assistant_frame_types:?}, turn_ends={turn_end_kinds:?}, failure_codes={failure_codes:?}"
        );
        match client.state(&session_id).await {
            Ok(state) => eprintln!(
                "real-model diagnostic: running={:?}, pendingInput={:?}, cursor={:?}",
                state["running"], state["pendingInput"], state["cursor"]
            ),
            Err(_) => eprintln!("real-model diagnostic: state query failed"),
        }
    }
    assert!(
        matches!(observed, Ok(true)),
        "real-model run did not produce streaming chunks and persisted marker output"
    );
    eprintln!("real-model: first streaming and persisted output observed");
    let first_idle = client
        .wait(&session_id, Duration::from_secs(30))
        .await
        .unwrap_or_else(|_| panic!("real-model first idle wait failed"));
    assert!(
        first_idle.to_string().contains(MARKER),
        "real-model marker was absent from first session/wait output"
    );
    assert!(
        first_idle["reason"] == "idle" && first_idle["state"]["running"] == false,
        "real-model first turn did not reach idle"
    );
    follow.cancel();
    drop(follow);
    let mut follow = client
        .follow(&session_id)
        .await
        .unwrap_or_else(|_| panic!("real-model follow reopen failed"));
    let Ok(Some(snapshot)) = follow.next().await else {
        panic!("real-model reopened follow omitted its snapshot");
    };
    assert!(
        snapshot["type"] == "snapshot" && snapshot["hasMore"] == false,
        "real-model reopened follow did not contain a complete snapshot"
    );
    let prompt_message_ids = logical_prompt_message_ids(&snapshot, REQUEST_ID);
    assert_eq!(
        prompt_message_ids.len(),
        1,
        "real-model requestId produced multiple logical messages in the complete snapshot"
    );
    eprintln!("real-model: first turn idle and prompt deduplication confirmed");

    let marker_path = directory.path.join("bash-tool-started.marker");
    assert!(
        !marker_path.exists(),
        "real-model bash marker existed before the second prompt"
    );
    let bash_command = format!(
        "touch {} && sleep 60",
        bash_quote(&marker_path.to_string_lossy())
    );
    let second_prompt = format!(
        "METIS_TEST_APPROVAL. Call the bash tool exactly once to execute this command unchanged: `{bash_command}`. This tool call is mandatory. Do not call any other tool, do not execute any other command, and do not give a final response until the sleep finishes."
    );
    client
        .prompt(&session_id, SECOND_REQUEST_ID, &second_prompt)
        .await
        .unwrap_or_else(|_| panic!("real-model bash-sleep prompt admission failed"));
    eprintln!("real-model: bash prompt admitted; waiting for approval");
    let mut second_event_types = Vec::new();
    let mut second_host_events = Vec::new();
    let approval = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            tokio::select! {
                event = client.next_event() => {
                    match event {
                        Ok(HostEvent::Approval(approval)) => break Some(approval),
                        Ok(HostEvent::Emit { event, .. }) => {
                            if !second_host_events.contains(&event) {
                                second_host_events.push(event);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break None,
                    }
                }
                frame = follow.next() => {
                    match frame {
                        Ok(Some(frame)) => {
                            let kind = if frame["type"] == "event" {
                                frame["event"]["type"].as_str().unwrap_or("missing")
                            } else {
                                frame["type"].as_str().unwrap_or("missing")
                            }.to_owned();
                            if !second_event_types.contains(&kind) {
                                second_event_types.push(kind);
                            }
                        }
                        Ok(None) | Err(_) => break None,
                    }
                }
            }
        }
    })
    .await;
    let Ok(Some(approval)) = approval else {
        eprintln!(
            "real-model diagnostic: approval wait ended with marker_created={}, event_types={second_event_types:?}, host_events={second_host_events:?}",
            marker_path.exists()
        );
        match client.state(&session_id).await {
            Ok(state) => eprintln!(
                "real-model diagnostic: approval wait state running={:?}, pendingInput={:?}, cursor={:?}",
                state["running"], state["pendingInput"], state["cursor"]
            ),
            Err(_) => eprintln!("real-model diagnostic: approval wait state query failed"),
        }
        panic!("real-model bash-sleep prompt did not produce an approval");
    };
    assert!(
        approval.agent_id == session_id,
        "real-model approval agentId did not match the Session"
    );
    assert!(
        approval.request["toolName"] == "bash",
        "real-model approval was not for the bash tool"
    );
    eprintln!("real-model: bash approval received");
    client
        .answer(&approval, ApprovalOutcome::AllowedOnce)
        .await
        .unwrap_or_else(|_| panic!("real-model approval answer failed"));
    eprintln!("real-model: approval accepted; waiting for bash startup");

    // No more follow data is needed. Cancel the observation stream so its
    // bounded queue cannot overflow while approval and cancellation continue.
    follow.cancel();
    let bash_started = tokio::time::timeout(Duration::from_secs(30), async {
        while !marker_path.exists() {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
                event = client.next_event() => {
                    if event.is_err() {
                        return false;
                    }
                }
            }
        }
        true
    })
    .await;
    assert!(
        matches!(bash_started, Ok(true)),
        "real-model approval answer did not start the bash command"
    );
    eprintln!("real-model: bash started; cancelling active turn");
    if let Err(error) = client.cancel(&session_id).await {
        eprintln!("real-model diagnostic: cancellation error={error}");
        match client.state(&session_id).await {
            Ok(state) => eprintln!(
                "real-model diagnostic: cancellation state running={:?}, pendingInput={:?}, cursor={:?}",
                state["running"], state["pendingInput"], state["cursor"]
            ),
            Err(state_error) => {
                eprintln!("real-model diagnostic: cancellation state error={state_error}");
            }
        }
        panic!("real-model bash-sleep cancellation failed");
    }
    let final_idle = client
        .wait(&session_id, Duration::from_secs(30))
        .await
        .unwrap_or_else(|_| panic!("real-model final idle wait failed"));
    assert!(
        final_idle["reason"] == "idle" && final_idle["state"]["running"] == false,
        "real-model cancelled bash-sleep task did not reach idle"
    );
    eprintln!("real-model: cancelled turn reached idle");
    client
        .close()
        .await
        .unwrap_or_else(|_| panic!("real-model client shutdown failed"));
    runtime
        .shutdown()
        .unwrap_or_else(|_| panic!("real-model Host shutdown failed"));
}
