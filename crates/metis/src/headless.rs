//! Persistent Host lifecycle used by the headless command-line entry point.
//!
//! The connection file is the handoff between the supervised Host and clients.
//! It contains launch credentials, so it is never copied to terminal output or
//! logs and is removed after the listener stops.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;

use crate::{
    Error, Result,
    runtime::{DshRuntime, DshRuntimeConfig, DshSupervisor},
};

/// Name of the per-home file containing the current Host launch credentials.
pub const CONNECTION_FILE_NAME: &str = "connection.json";
const HOME_LOCK_FILE_NAME: &str = ".host.lock";

/// Runtime configuration and the interval used to notice an unexpected exit.
#[derive(Clone, Debug)]
pub struct HeadlessConfig {
    pub runtime: DshRuntimeConfig,
    pub poll_interval: Duration,
}

/// A running persistent Host and its current connection file.
pub struct Headless {
    runtime: DshRuntime,
    connection_file: Option<PathBuf>,
    poll_interval: Duration,
    // The operating system owns lock cleanup on abnormal process exit.
    _home_lock: File,
}

impl Headless {
    /// Start the Host and publish its connection file only after it is ready.
    /// One persistent Host exclusively owns its state directory at a time.
    pub fn start(supervisor: &DshSupervisor, config: &HeadlessConfig) -> Result<Self> {
        let home_lock = acquire_home(&config.runtime.home)?;
        let runtime = supervisor.start(&config.runtime)?;
        let connection_file = config.runtime.home.join(CONNECTION_FILE_NAME);
        write_connection_file(
            &connection_file,
            runtime.web_url(),
            runtime.auth_url(),
            runtime.id(),
        )?;
        Ok(Self {
            runtime,
            connection_file: Some(connection_file),
            poll_interval: config.poll_interval,
            _home_lock: home_lock,
        })
    }

    /// Path to the current launch credentials. The file is user-private on Unix.
    #[must_use]
    pub fn connection_file(&self) -> &Path {
        self.connection_file
            .as_deref()
            .expect("a running Host has a connection file")
    }

    /// Wait for a caller-provided shutdown signal or an unexpected Host exit.
    /// Both paths clean up the runtime and retire the connection file.
    pub async fn wait<F>(mut self, shutdown: F) -> Result<()>
    where
        F: Future<Output = io::Result<()>>,
    {
        tokio::pin!(shutdown);
        let outcome = loop {
            if let Some(status) = self.runtime.try_wait()? {
                break Err(Error::RuntimeDshExit(status));
            }
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(Error::RuntimeSignal)?;
                    break Ok(());
                }
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        };

        let cleanup = self.runtime.shutdown();
        let remove = self.remove_connection_file();
        cleanup?;
        remove?;
        outcome
    }

    fn remove_connection_file(&mut self) -> Result<()> {
        let Some(path) = self.connection_file.take() else {
            return Ok(());
        };
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => {
                self.connection_file = Some(path.clone());
                Err(Error::RuntimeConnection { path, source })
            }
        }
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        let _ = self.runtime.shutdown();
        let _ = self.remove_connection_file();
    }
}

fn acquire_home(home: &Path) -> Result<File> {
    fs::create_dir_all(home).map_err(|source| Error::RuntimePrepare {
        path: home.to_owned(),
        source,
    })?;
    let lock_path = home.join(HOME_LOCK_FILE_NAME);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| Error::RuntimePrepare {
            path: lock_path.clone(),
            source,
        })?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(Error::RuntimeAlreadyRunning {
                home: home.to_owned(),
            });
        }
        Err(TryLockError::Error(source)) => {
            return Err(Error::RuntimePrepare {
                path: lock_path,
                source,
            });
        }
    }

    // Ownership makes a file left by a crashed predecessor stale. Remove it
    // before boot so clients cannot connect using obsolete credentials.
    let stale = home.join(CONNECTION_FILE_NAME);
    match fs::remove_file(&stale) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::RuntimeConnection {
                path: stale,
                source,
            });
        }
    }
    Ok(lock)
}

fn temporary_connection_path(path: &Path, instance: u32, attempt: u8) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the fixed connection file name is UTF-8");
    path.with_file_name(format!(
        ".{name}.{}.{}.{}.tmp",
        std::process::id(),
        instance,
        attempt
    ))
}

fn create_temporary_file(path: &Path, instance: u32) -> Result<(PathBuf, File)> {
    for attempt in 0..100 {
        let temporary = temporary_connection_path(path, instance, attempt);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(Error::RuntimeConnection {
                    path: path.to_owned(),
                    source,
                });
            }
        }
    }
    Err(Error::RuntimeConnection {
        path: path.to_owned(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a private connection temporary file",
        ),
    })
}

fn write_connection_file(path: &Path, web_url: &str, auth_url: &str, instance: u32) -> Result<()> {
    let (temporary, mut file) = create_temporary_file(path, instance)?;
    let result = (|| {
        serde_json::to_writer(
            &mut file,
            &json!({
                "webUrl": web_url,
                "authUrl": auth_url,
            }),
        )
        .map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);

        // Unix rename replaces atomically. Windows requires retiring the old
        // file first; readers still get either a missing file or complete JSON.
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&temporary, path)
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(Error::RuntimeConnection {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_directory() -> TestDirectory {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "metis-headless-test-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create headless test directory");
        TestDirectory(path)
    }

    #[test]
    fn connection_file_is_complete_private_and_does_not_clobber_temporaries() {
        let directory = temporary_directory();
        let path = directory.0.join(super::CONNECTION_FILE_NAME);
        let occupied = super::temporary_connection_path(&path, 42, 0);
        fs::write(&occupied, "another publisher").expect("occupy temporary name");
        fs::write(&path, "stale connection").expect("write stale connection file");
        super::write_connection_file(
            &path,
            "http://127.0.0.1:1234/",
            "http://127.0.0.1:1234/connect?credential=example",
            42,
        )
        .expect("publish connection file");

        let value: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&path).expect("read published connection file"),
        )
        .expect("connection file is complete JSON");
        assert_eq!(value["webUrl"], "http://127.0.0.1:1234/");
        assert_eq!(
            value["authUrl"],
            "http://127.0.0.1:1234/connect?credential=example"
        );
        assert_eq!(
            fs::read_to_string(&occupied).expect("preserve occupied temporary"),
            "another publisher"
        );
        assert!(!super::temporary_connection_path(&path, 42, 1).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path)
                    .expect("connection file metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn temporary_symlinks_are_never_followed_or_removed() {
        let directory = temporary_directory();
        let path = directory.0.join(super::CONNECTION_FILE_NAME);
        let outside = directory.0.join("outside");
        fs::write(&outside, "preserve me").expect("write outside file");
        std::os::unix::fs::symlink(&outside, super::temporary_connection_path(&path, 7, 0))
            .expect("place hostile temporary symlink");

        super::write_connection_file(&path, "http://localhost/", "http://localhost/connect", 7)
            .expect("publish beside hostile temporary");
        assert_eq!(
            fs::read_to_string(outside).expect("read outside file"),
            "preserve me"
        );
        assert!(super::temporary_connection_path(&path, 7, 0).is_symlink());
    }

    #[test]
    fn home_lock_rejects_overlap_and_retires_stale_credentials() {
        let directory = temporary_directory();
        let home = directory.0.join("home");
        fs::create_dir(&home).expect("create Host home");
        let stale = home.join(super::CONNECTION_FILE_NAME);
        fs::write(&stale, "obsolete").expect("write stale credentials");

        let first = super::acquire_home(&home).expect("acquire first home lock");
        assert!(!stale.exists());
        let error = super::acquire_home(&home).expect_err("second owner must be rejected");
        assert!(matches!(error, crate::Error::RuntimeAlreadyRunning { .. }));
        drop(first);
        let reacquired = super::acquire_home(&home).expect("released home lock is reusable");
        drop(reacquired);
    }
}
