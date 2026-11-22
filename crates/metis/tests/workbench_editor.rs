//! Verify code highlighting, editing and Host persistence at the real Editor boundary.

#![cfg(feature = "workbench")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, HighlightStyle, Point, TestAppContext, Window,
    WindowBounds, WindowHandle, WindowOptions,
    base::Root,
    component::{
        highlighter::{HighlightTheme, SyntaxHighlighter},
        input::{Editor, EditorState, Rope},
    },
    div,
    prelude::*,
    px, size,
    test::TestWindowExt as _,
};
use metis::{
    client::{DshClient, DshClientConfig},
    runtime::{DshRuntime, DshRuntimeConfig, DshSupervisor},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "metis-workbench-editor-{}-{sequence}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).expect("remove stale test directory");
        }
        fs::create_dir_all(&path).expect("create isolated test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct CodeEditor {
    state: Entity<EditorState>,
}

impl Render for CodeEditor {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(Editor::new(&self.state).size_full())
    }
}

fn open_editor(cx: &mut TestAppContext, source: &str) -> (WindowHandle<Root>, Entity<EditorState>) {
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
            |window, cx| {
                cx.new(|cx| CodeEditor {
                    state: cx.new(|cx| {
                        EditorState::new(window, cx)
                            .language("rust")
                            .default_value(source)
                    }),
                })
            },
        )
        .expect("open Editor test window");
        let state = view.read(cx).state.clone();
        (window.downcast::<Root>().expect("Base Root"), state)
    })
}

fn start_host(directory: &TestDirectory) -> (DshRuntime, tokio::runtime::Runtime) {
    let host = DshSupervisor::system()
        .start(&DshRuntimeConfig {
            metis_executable: PathBuf::from(env!("CARGO_BIN_EXE_metis")),
            node_executable: PathBuf::from("node"),
            host_entrypoint: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("packages/metis-host/lib/host.js"),
            home: directory.path.join("dsh-home"),
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(15),
        })
        .expect("start real Host for Editor");
    let tokio = tokio::runtime::Runtime::new().expect("create Host client executor");
    (host, tokio)
}

fn assert_rust_highlighting(source: &str) {
    let rope = Rope::from_str(source);
    let mut highlighter = SyntaxHighlighter::new("rust");
    assert!(highlighter.update(None, &rope, None));
    assert!(highlighter.tree().is_some(), "Rust must be parsed");
    let theme = HighlightTheme::default_dark();
    let styles = highlighter.styles(&(0..source.len()), theme.as_ref());
    let style_at = |needle: &str| {
        let offset = source
            .find(needle)
            .expect("highlight fixture contains token");
        styles
            .iter()
            .find(|(range, _)| range.start <= offset && offset < range.end)
            .map_or_else(
                || panic!("missing style for {needle:?}: {styles:?}"),
                |(_, style)| *style,
            )
    };
    let keyword = style_at("fn");
    let string = style_at("\"Hello, Metis!\"");
    assert_ne!(keyword, HighlightStyle::default());
    assert_ne!(string, HighlightStyle::default());
    assert_ne!(keyword.color, string.color);
}

#[gpui_kit::test]
fn real_editor_highlights_edits_and_saves_a_host_file(cx: &mut TestAppContext) {
    const SOURCE: &str = "fn main() {\n    println!(\"Hello, Metis!\");\n}";
    const EDIT: &str = " // saved by Editor";

    let directory = TestDirectory::new();
    let project = directory.path.join("project");
    fs::create_dir(&project).expect("create Editor project");
    fs::write(project.join("main.rs"), SOURCE).expect("write Rust fixture");

    let (mut host, tokio) = start_host(&directory);
    let client = tokio
        .block_on(DshClient::connect(
            host.auth_url(),
            DshClientConfig::default(),
        ))
        .expect("connect Editor client");
    let opened = tokio
        .block_on(client.open_project(&project.to_string_lossy()))
        .expect("open Editor project");
    let loaded = tokio
        .block_on(client.read_project_file(&opened.project_id, "main.rs"))
        .expect("load Rust source through Host");
    assert_eq!(loaded, SOURCE);
    assert_rust_highlighting(&loaded);

    let (handle, state) = open_editor(cx, &loaded);
    let edited = cx
        .update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let input = window.find(("input", state.entity_id()));
            assert!(input.visible());
            assert_eq!(input.value(), Some(SOURCE));
            assert_eq!(state.read(cx).language_name(), "rust");
            // Fold candidates come from the highlighter installed in this Editor.
            assert!(state.read(cx).display_map().is_fold_candidate(0));

            window.click(("input", state.entity_id()), cx);
            state.update(cx, |state, cx| {
                let end = state.text().len();
                state.set_selected_range(end..end, cx);
            });
            window.render_frame(cx);
            window.input(EDIT, cx);

            let expected = format!("{SOURCE}{EDIT}");
            assert_eq!(state.read(cx).value(), expected);
            let input = window.find(("input", state.entity_id()));
            assert_eq!(input.value(), Some(expected.as_str()));
            expected
        })
        .unwrap();

    tokio
        .block_on(client.write_project_file(&opened.project_id, "main.rs", &edited))
        .expect("save Editor contents through Host");
    let saved = tokio
        .block_on(client.read_project_file(&opened.project_id, "main.rs"))
        .expect("read saved Editor contents through Host");
    assert_eq!(saved, format!("{SOURCE}{EDIT}"));
    assert_eq!(
        fs::read_to_string(project.join("main.rs")).expect("read saved file from disk"),
        saved
    );

    tokio.block_on(client.close()).expect("close Editor client");
    host.shutdown().expect("stop Editor Host");
}
