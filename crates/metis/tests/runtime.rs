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
    client::{ApprovalOutcome, ClientError, DshClient, DshClientConfig, HostEvent},
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
            host_entrypoint: script,
            home: directory.join("dsh-home"),
            startup_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(15),
        })
        .expect("fixture workbench starts its runtime");
    fs::write(directory.join("ready"), runtime.id().to_string()).expect("publish watchdog PID");

    // Keep Drop from running: the outer test terminates this process directly.
    // Keeping the runtime binding alive is intentional.
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
    let config = runtime_config(directory, real_dsh_entrypoint());
    fs::create_dir_all(&config.home).expect("create approval fixture DSH_HOME");
    let fixture = workspace_root().join("packages/metis-host/test/approval-fixture.js");
    let fixture = fixture.to_string_lossy().replace('\'', "''");
    fs::write(
        config.home.join("cordis.patch.yml"),
        format!("- insert:\n    - id: metis-test-approval\n      name: '{fixture}'\n"),
    )
    .expect("write approval fixture home patch");
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

fn logical_prompt_message_ids(snapshot: &serde_json::Value, request_id: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for record in snapshot["records"].as_array().into_iter().flatten() {
        let event = &record["event"];
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
