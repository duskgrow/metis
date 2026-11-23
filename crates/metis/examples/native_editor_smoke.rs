#[cfg(target_os = "linux")]
mod linux {
    use std::{
        io::{self, Write as _},
        process,
        time::Duration,
    };

    use gpui_kit::{
        App, AppContext as _, Bounds, Context, Entity, EntityInputHandler as _, Point, QuitMode,
        Render, TitlebarOptions, Window, WindowBounds, WindowOptions, WindowingModes, assets,
        component::{
            ActiveTheme as _,
            input::{Editor, EditorState},
        },
        div,
        prelude::*,
        px, size,
    };
    use serde_json::{Value, json};

    struct SmokeView {
        state: Entity<EditorState>,
    }

    impl Render for SmokeView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .gap_4()
                        .p_4()
                        .children(["你", "好", "中", "文"].map(pixel_probe)),
                )
                .child(
                    div()
                        .flex_1()
                        .w_full()
                        .child(Editor::new(&self.state).size_full()),
                )
        }
    }

    fn pixel_probe(text: &'static str) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w(px(64.))
            .h(px(64.))
            .text_2xl()
            .child(text)
    }

    fn emit_json(value: &Value) {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{value}").expect("write smoke JSON line");
        stdout.flush().expect("flush smoke JSON line");
    }

    fn snapshot(state: &Entity<EditorState>, window: &mut Window, cx: &mut App) -> Value {
        let (value, marked, preedit, selection) = state.update(cx, |state, cx| {
            let marked_range = state.marked_text_range(window, cx);
            let preedit = marked_range.clone().and_then(|range| {
                let mut adjusted_range = None;
                state.text_for_range(range, &mut adjusted_range, window, cx)
            });
            let selection = state
                .selected_text_range(false, window, cx)
                .expect("Editor always exposes a selection")
                .range;
            (
                state.value().to_string(),
                marked_range.is_some(),
                preedit,
                selection,
            )
        });
        json!({
            "type": "snapshot",
            "value": value,
            "marked": marked,
            "preedit": preedit,
            "selection_start": selection.start,
            "selection_end": selection.end,
        })
    }

    pub fn run() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(io::stderr)
            .init();
        let pid = process::id();
        let title = format!("Metis Native Editor Smoke {pid}");
        let app_id = format!("com.duskgrow.metis.native_editor_smoke.{pid}");

        gpui_kit::platform::linux(WindowingModes::WAYLAND)
            .with_assets(assets::Assets)
            .with_quit_mode(QuitMode::Explicit)
            .run(move |cx| {
                gpui_kit::init(cx);
                let (window, view) = gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Point::default(),
                            size: size(px(800.), px(450.)),
                        })),
                        titlebar: Some(TitlebarOptions {
                            title: Some(title.into()),
                            ..Default::default()
                        }),
                        app_id: Some(app_id.clone()),
                        focus: true,
                        show: true,
                        is_resizable: false,
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| SmokeView {
                            state: cx.new(|cx| EditorState::new(window, cx).language("plaintext")),
                        })
                    },
                )
                .expect("open native Wayland Editor window");
                let state = view.read(cx).state.clone();

                window
                    .update(cx, |_, window, cx| {
                        state.update(cx, |state, cx| state.focus(window, cx));
                        window.activate_window();
                    })
                    .expect("focus native Editor");

                emit_json(&json!({"type": "ready", "pid": pid, "app_id": app_id}));

                cx.spawn(async move |cx| {
                    let mut previous: Option<Value> = None;
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(50))
                            .await;
                        if let Ok(current) =
                            window.update(cx, |_, window, cx| snapshot(&state, window, cx))
                        {
                            if previous.as_ref() != Some(&current) {
                                emit_json(&current);
                                previous = Some(current);
                            }
                        } else {
                            cx.update(|cx| cx.quit());
                            break;
                        }
                    }
                })
                .detach();

                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_secs(120))
                        .await;
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("native_editor_smoke requires Linux Wayland");
    std::process::exit(2);
}
