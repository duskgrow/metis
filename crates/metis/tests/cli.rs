//! End-to-end CLI tests: verify stdout/stderr separation and exit-code
//! discipline through the compiled binary.

use assert_cmd::Command;

fn cli() -> Command {
    Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("binary built by cargo")
}

/// The `--help` output is guarded by a snapshot: changes to arguments or
/// wording show up as a diff in code review.
/// Update with `just snapshot-review` (human approval); CI is read-only.
#[test]
fn help_output() {
    let assert = cli().arg("--help").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    // clap derives the usage-line program name from argv[0], which carries an
    // .exe suffix on Windows; normalize so one snapshot serves all platforms.
    let stdout = stdout.replace(
        &format!("{}.exe", env!("CARGO_PKG_NAME")),
        env!("CARGO_PKG_NAME"),
    );
    insta::assert_snapshot!(stdout);
}

#[test]
fn version_is_available_without_opening_the_workbench() {
    cli()
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("metis {}\n", env!("CARGO_PKG_VERSION")));
}

#[cfg(all(feature = "workbench", target_os = "linux"))]
#[test]
fn workbench_requires_a_graphical_session() {
    let assert = cli()
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("no graphical session detected"),
        "stderr should explain the missing display, got: {stderr}"
    );
}

#[cfg(all(feature = "workbench", target_os = "linux"))]
#[test]
fn unreachable_display_fails_with_a_diagnostic() {
    let assert = cli()
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .env("DISPLAY", ":65535")
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("failed to start the Metis workbench"),
        "stderr should explain the display connection failure, got: {stderr}"
    );
}

#[cfg(all(not(feature = "workbench"), unix))]
mod headless {
    use std::{
        env, fs,
        io::{BufRead, BufReader, Read},
        net::TcpStream,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use metis::{
        client::{DshClient, DshClientConfig},
        headless::CONNECTION_FILE_NAME,
    };

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
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

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn host_command_serves_git_and_stops_gracefully() {
        let directory = TestDirectory(
            env::temp_dir().join(format!("metis-headless-cli-{}", std::process::id())),
        );
        let _ = fs::remove_dir_all(&directory.0);
        let project = directory.0.join("repo");
        fs::create_dir_all(&project).expect("create CLI test project");
        fs::write(project.join("tracked.txt"), "before\n").expect("write tracked file");
        run_git(&project, &["init", "--quiet"]);
        run_git(&project, &["config", "user.name", "Metis Test"]);
        run_git(&project, &["config", "user.email", "metis@example.invalid"]);
        run_git(&project, &["add", "--", "."]);
        run_git(&project, &["commit", "--quiet", "-m", "initial"]);
        fs::write(project.join("tracked.txt"), "after\n").expect("modify tracked file");

        let home = directory.0.join("home");
        let host_entrypoint = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/metis-host/lib/host.js")
            .canonicalize()
            .expect("built Metis Host entry point");
        let child = Command::new(env!("CARGO_BIN_EXE_metis"))
            .arg("host")
            .arg("--home")
            .arg(&home)
            .arg("--node")
            .arg("node")
            .arg("--host-entrypoint")
            .arg(host_entrypoint)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn headless Metis command");
        let mut child = ChildGuard(child);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let stdout = child.0.stdout.take().expect("capture CLI stdout");
        let stdout_reader = thread::spawn(move || {
            let mut output = String::new();
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("read headless CLI stdout");
                if Path::new(&line).file_name() == Some(std::ffi::OsStr::new(CONNECTION_FILE_NAME))
                {
                    ready_sender.send(line.clone()).ok();
                }
                output.push_str(&line);
                output.push('\n');
            }
            output
        });
        let stderr = child.0.stderr.take().expect("capture CLI stderr");
        let stderr_reader = thread::spawn(move || {
            let mut output = String::new();
            let mut stderr = stderr;
            stderr
                .read_to_string(&mut output)
                .expect("read headless CLI stderr");
            output
        });
        let ready = ready_receiver.recv_timeout(Duration::from_secs(30));
        let ready = match ready {
            Ok(ready) => ready,
            Err(error) => {
                let _ = child.0.kill();
                let _ = child.0.wait();
                let stdout = stdout_reader.join().expect("join CLI stdout reader");
                let stderr = stderr_reader.join().expect("join CLI stderr reader");
                panic!("headless CLI did not become ready: {error}\n{stdout}{stderr}");
            }
        };

        let connection_file = PathBuf::from(&ready);
        assert_eq!(connection_file, home.join(CONNECTION_FILE_NAME));
        let connection: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&connection_file).expect("read CLI connection file"),
        )
        .expect("CLI connection file is valid JSON");
        let auth_url = connection["authUrl"].as_str().expect("connection authUrl");
        let web_url = connection["webUrl"].as_str().expect("connection webUrl");
        assert!(!ready.contains(auth_url));
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&connection_file)
                .expect("connection file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let client = DshClient::connect(
            auth_url,
            DshClientConfig {
                request_timeout: Duration::from_secs(20),
                stream_open_timeout: Duration::from_secs(20),
            },
        )
        .await
        .expect("connect through CLI-published credentials");
        let project = client
            .open_project(&project.to_string_lossy())
            .await
            .expect("open CLI test project");
        let status = client
            .git_status(&project.project_id)
            .await
            .expect("read CLI Git status");
        assert!(status.contains("tracked.txt"), "status: {status:?}");
        let patch = client
            .git_diff(&project.project_id)
            .await
            .expect("read CLI Git diff");
        assert!(patch.contains("+after"), "patch: {patch}");
        client.close().await.expect("close CLI test client");

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.0.id().try_into().expect("CLI PID fits in i32")),
            nix::sys::signal::Signal::SIGINT,
        )
        .expect("interrupt headless CLI");
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("poll headless CLI") {
                break status;
            }
            assert!(Instant::now() < deadline, "headless CLI did not stop");
            thread::sleep(Duration::from_millis(20));
        };
        let stdout = stdout_reader.join().expect("join CLI stdout reader");
        let stderr = stderr_reader.join().expect("join CLI stderr reader");
        assert!(status.success(), "CLI failed: {status}\n{stdout}{stderr}");
        assert_eq!(stdout, format!("{}\n", connection_file.display()));
        assert!(!connection_file.exists());
        let address = web_url
            .strip_prefix("http://")
            .and_then(|url| url.split('/').next())
            .expect("loopback Host address");
        assert!(TcpStream::connect(address).is_err());
    }

    #[test]
    fn host_command_handles_sigterm_during_startup() {
        let directory = TestDirectory(env::temp_dir().join(format!(
            "metis-headless-startup-signal-{}",
            std::process::id()
        )));
        let _ = fs::remove_dir_all(&directory.0);
        fs::create_dir_all(&directory.0).expect("create startup signal test directory");
        let host = directory.0.join("delayed-host.cjs");
        fs::write(
            &host,
            r#"
process.stdin.on("data", () => process.exit(0));
process.stdin.on("end", () => process.exit(0));
process.stdin.resume();
setTimeout(() => {
  const server = require("node:net").createServer();
  server.listen(0, "127.0.0.1", () => {
    const url = `http://127.0.0.1:${server.address().port}/`;
    process.stdout.write(`METIS_READY\t${url}\t${url}connect?token=test-token\n`);
  });
}, 500);
"#,
        )
        .expect("write delayed Host fixture");
        let stdout = fs::File::create(directory.0.join("stdout")).expect("create stdout log");
        let stderr = fs::File::create(directory.0.join("stderr")).expect("create stderr log");
        let home = directory.0.join("home");
        let child = Command::new(env!("CARGO_BIN_EXE_metis"))
            .arg("host")
            .arg("--home")
            .arg(&home)
            .arg("--node")
            .arg("node")
            .arg("--host-entrypoint")
            .arg(host)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn delayed-start Host command");
        let mut child = ChildGuard(child);
        thread::sleep(Duration::from_millis(100));
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.0.id().try_into().expect("CLI PID fits in i32")),
            nix::sys::signal::Signal::SIGTERM,
        )
        .expect("terminate starting CLI");
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("poll delayed-start CLI") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "starting CLI did not stop after SIGTERM"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "startup signal failed: {status}\n{}{}",
            fs::read_to_string(directory.0.join("stdout")).expect("read stdout log"),
            fs::read_to_string(directory.0.join("stderr")).expect("read stderr log")
        );
        assert!(!home.join(CONNECTION_FILE_NAME).exists());
    }
}

#[cfg(not(feature = "workbench"))]
#[test]
fn headless_build_requires_the_host_subcommand() {
    let assert = cli().assert().failure().code(1);
    let output = assert.get_output();
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty on failure"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no entry point selected for the Metis headless build"),
        "stderr should contain the error message, got: {stderr}"
    );
    assert!(
        stderr.contains("metis host --home <PATH>"),
        "stderr should point to the Host command, got: {stderr}"
    );
}
