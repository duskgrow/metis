//! Shared real-Host fixtures for process-level integration tests. Each test
//! target uses only a subset of these helpers, so unused items are expected.
#![allow(dead_code)]

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use metis::{client::DshClient, runtime::DshRuntimeConfig};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub struct TestDirectory {
    pub path: PathBuf,
}

impl TestDirectory {
    pub fn new(name: &str) -> Self {
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

    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
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

pub fn workspace_root() -> PathBuf {
    // Deliberately not canonicalized: on Windows `canonicalize` returns a
    // verbatim `\\?\` path, which Node's module loader reduces to the bare
    // drive root and rejects with EISDIR.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn runtime_config(directory: &TestDirectory, host_entrypoint: PathBuf) -> DshRuntimeConfig {
    DshRuntimeConfig {
        metis_executable: PathBuf::from(env!("CARGO_BIN_EXE_metis")),
        node_executable: PathBuf::from("node"),
        host_entrypoint,
        home: directory.path.join("dsh-home"),
        startup_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_secs(15),
    }
}

pub fn real_dsh_entrypoint() -> PathBuf {
    workspace_root().join("packages/metis-host/lib/host.js")
}

pub fn approval_runtime_config(directory: &TestDirectory) -> DshRuntimeConfig {
    fixture_runtime_config(directory, "approval-fixture.js")
}

pub fn fixture_runtime_config(directory: &TestDirectory, fixture: &str) -> DshRuntimeConfig {
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

pub async fn create_session_at(client: &DshClient, directory: &Path) -> String {
    let project = client
        .open_project(&directory.to_string_lossy())
        .await
        .expect("open Session project");
    client
        .create_session(&project.project_id)
        .await
        .expect("create project-attached Session")
}
