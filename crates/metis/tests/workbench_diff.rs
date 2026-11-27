//! Verify the Git patch contract at the real GPUI Diff boundary.

#![cfg(feature = "workbench")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, Point, TestAppContext, Window, WindowBounds,
    WindowHandle, WindowOptions,
    base::Root,
    component::diff::{Diff, DiffFile, DiffFileStatus, DiffMode, DiffState},
    div,
    prelude::*,
    px, size,
    test::{ElementSnapshot, TestSupportExt as _, TestWindowExt as _},
};
use metis::{
    client::{DshClient, DshClientConfig},
    runtime::{DshRuntimeConfig, DshSupervisor},
};

static NEXT_REPOSITORY: AtomicU64 = AtomicU64::new(0);

struct TestRepository {
    path: PathBuf,
}

impl TestRepository {
    fn new() -> Self {
        let sequence = NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "metis-workbench-diff-{}-{sequence}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).expect("remove stale test repository");
        }
        fs::create_dir_all(&path).expect("create isolated test repository");
        Self { path }
    }

    fn git(&self, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command
            .args(["--no-pager", "--no-optional-locks"])
            .args(args)
            .current_dir(&self.path)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        for name in [
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_NAMESPACE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_PREFIX",
            "GIT_WORK_TREE",
        ] {
            command.env_remove(name);
        }
        let output = command.output().expect("run test Git command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("Git patch is UTF-8")
    }

    fn write(&self, path: &str, contents: &str) {
        fs::write(self.path.join(path), contents).expect("write repository fixture");
    }
}

impl Drop for TestRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Review {
    state: Entity<DiffState>,
}

impl Render for Review {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("review")
            .test_support()
            .size_full()
            .child(Diff::new(&self.state).size_full())
    }
}

fn open_review(
    cx: &mut TestAppContext,
    files: Vec<DiffFile>,
) -> (WindowHandle<Root>, Entity<DiffState>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1000.), px(700.)),
        };
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |_, cx| {
                cx.new(|cx| Review {
                    state: cx.new(|cx| DiffState::new(files, cx).with_context_lines(None)),
                })
            },
        )
        .expect("open Diff test window");
        let state = view.read(cx).state.clone();
        (window.downcast::<Root>().expect("Base Root"), state)
    })
}

fn host_workspace_patch() -> String {
    let repository = TestRepository::new();
    repository.git(&["init", "--quiet"]);
    repository.git(&["config", "user.name", "Metis Diff Test"]);
    repository.git(&["config", "user.email", "metis@example.invalid"]);
    repository.git(&["config", "commit.gpgsign", "false"]);
    repository.write("tracked.txt", "before\n");
    repository.write("deleted.txt", "remove\n");
    repository.write("rename-old.txt", "same\n");
    repository.git(&["add", "-A"]);
    repository.git(&["commit", "--quiet", "-m", "initial"]);

    repository.write("added.txt", "new file\n");
    repository.write("tracked.txt", "after\n");
    fs::remove_file(repository.path.join("deleted.txt")).expect("delete tracked file");
    fs::rename(
        repository.path.join("rename-old.txt"),
        repository.path.join("rename-new.txt"),
    )
    .expect("rename tracked file");
    repository.git(&["add", "-A"]);

    let mut host = DshSupervisor::system()
        .start(&DshRuntimeConfig {
            metis_executable: PathBuf::from(env!("CARGO_BIN_EXE_metis")),
            node_executable: PathBuf::from("node"),
            host_entrypoint: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("packages/metis-host/lib/host.js"),
            home: repository.path.join("dsh-home"),
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(15),
        })
        .expect("start real Host for Git Diff");
    let tokio = tokio::runtime::Runtime::new().expect("create Host client executor");
    let patch = tokio.block_on(async {
        let client = DshClient::connect(host.auth_url(), DshClientConfig::default())
            .await
            .expect("connect Git Diff client");
        let project = client
            .open_project(&repository.path.to_string_lossy())
            .await
            .expect("open real Git project");
        client
            .git_diff(&project.project_id)
            .await
            .expect("read Host Git patch")
    });
    host.shutdown().expect("stop Git Diff Host");
    patch
}

fn assert_source(
    window: &mut Window,
    file: usize,
    side: &'static str,
    row: usize,
    text: &str,
) -> ElementSnapshot {
    let source = window
        .within(("diff-file", file))
        .within((side, row))
        .find("source");
    assert!(source.visible(), "{text:?} source row is invisible");
    assert!(source.bounds().size.width > px(0.));
    assert!(source.bounds().size.height > px(0.));
    assert_eq!(source.label(), Some(text));
    source
}

#[gpui_kit::test]
fn real_git_patch_renders_every_change_type_in_unified_and_split_modes(cx: &mut TestAppContext) {
    let patch = host_workspace_patch();
    let files = DiffFile::parse(&patch).expect("parse real Git workspace patch");
    assert_eq!(files.len(), 4, "patch: {patch}");
    let file = |path: &str| {
        files
            .iter()
            .find(|file| file.path().as_str() == path)
            .unwrap_or_else(|| panic!("missing {path} in patch: {patch}"))
    };
    assert_eq!(file("added.txt").status(), DiffFileStatus::Added);
    assert_eq!(file("added.txt").original_path(), None);
    assert_eq!(file("deleted.txt").status(), DiffFileStatus::Deleted);
    assert_eq!(file("deleted.txt").modified_path(), None);
    assert_eq!(file("tracked.txt").status(), DiffFileStatus::Modified);
    assert_eq!(file("rename-new.txt").status(), DiffFileStatus::Renamed);
    assert_eq!(
        file("rename-new.txt")
            .original_path()
            .map(gpui_kit::SharedString::as_str),
        Some("rename-old.txt")
    );

    let paths = files
        .iter()
        .map(|file| file.path().clone())
        .collect::<Vec<_>>();
    let (handle, state) = open_review(cx, files);
    cx.update_window(handle.into(), |_, window, cx| {
        for mode in [DiffMode::Unified, DiffMode::Split] {
            state.update(cx, |state, cx| state.set_mode(mode, cx));
            window.render_frame(cx);
            assert_eq!(state.read(cx).mode(), mode);

            for (index, path) in paths.iter().enumerate() {
                state.update(cx, |state, cx| state.scroll_to_file(path, cx));
                window.render_frame(cx);
                assert!(
                    window
                        .within(("diff-file", index))
                        .find("collapse-file")
                        .visible()
                );

                match path.as_str() {
                    "added.txt" => {
                        assert_source(window, index, "new", 0, "new file");
                    }
                    "deleted.txt" => {
                        assert_source(window, index, "old", 0, "remove");
                    }
                    "rename-new.txt" => {
                        let header = window.within(("diff-file", index)).find("collapse-file");
                        assert!(
                            header
                                .label()
                                .is_some_and(|label| label.contains("rename-new.txt"))
                        );
                        assert_eq!(
                            state.read(cx).files()[index]
                                .original_path()
                                .map(gpui_kit::SharedString::as_str),
                            Some("rename-old.txt")
                        );
                    }
                    "tracked.txt" => {
                        let original = assert_source(window, index, "old", 0, "before");
                        let modified = assert_source(window, index, "new", 0, "after");
                        match mode {
                            DiffMode::Unified => {
                                assert_eq!(original.bounds().left(), modified.bounds().left());
                                assert!(original.bounds().bottom() <= modified.bounds().top());
                            }
                            DiffMode::Split => {
                                assert_eq!(original.bounds().top(), modified.bounds().top());
                                assert!(original.bounds().left() < modified.bounds().left());
                            }
                            _ => unreachable!("the test selects only Unified and Split"),
                        }
                    }
                    unexpected => panic!("unexpected file in real patch: {unexpected}"),
                }
            }
        }
    })
    .unwrap();
}
