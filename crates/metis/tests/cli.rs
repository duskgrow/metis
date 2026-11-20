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

#[cfg(not(feature = "workbench"))]
#[test]
fn headless_build_reports_unimplemented_runtime() {
    let assert = cli().assert().failure().code(1);
    let output = assert.get_output();
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty on failure"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Metis headless runtime is not implemented yet"),
        "stderr should contain the error message, got: {stderr}"
    );
}
