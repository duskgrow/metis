//! Verify streaming Markdown and tail following at the real `TextView` boundary.

#![cfg(feature = "workbench")]

use std::sync::{Arc, Mutex};

use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, FollowMode, ListOffset, Point, ScrollDelta,
    TestAppContext, Window, WindowBounds, WindowHandle, WindowOptions,
    base::{Root, test_support},
    component::text::{TextView, TextViewState},
    div, point,
    prelude::*,
    px, size,
    test::{TestSupportExt as _, TestWindowExt as _},
};

struct CodeBlockContents {
    language: Option<String>,
    code: String,
}

#[derive(Default)]
struct CodeBlockObservation {
    latest: Mutex<Option<CodeBlockContents>>,
}

type LatestCodeBlock = Arc<CodeBlockObservation>;

struct Markdown {
    state: Entity<TextViewState>,
    latest_code_block: LatestCodeBlock,
}

impl Render for Markdown {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let latest_code_block = Arc::clone(&self.latest_code_block);
        div().id("markdown").test_support().size_full().child(
            TextView::new(&self.state)
                .scrollable(true)
                // Observe the real fenced block without replacing its renderer.
                .code_block_actions(move |block, _, _| {
                    *latest_code_block
                        .latest
                        .lock()
                        .expect("lock code block observer") = Some(CodeBlockContents {
                        language: block.lang().map(|language| language.to_string()),
                        code: block.code().to_string(),
                    });
                    div()
                })
                .size_full(),
        )
    }
}

fn open_markdown(
    cx: &mut TestAppContext,
    source: &str,
) -> (WindowHandle<Root>, Entity<TextViewState>, LatestCodeBlock) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(480.), px(160.)),
        };
        let latest_code_block = LatestCodeBlock::default();
        let observed = Arc::clone(&latest_code_block);
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |_, cx| {
                cx.new(|cx| Markdown {
                    state: cx.new(|cx| TextViewState::markdown(source, cx)),
                    latest_code_block: observed,
                })
            },
        )
        .expect("open Markdown test window");
        let state = view.read(cx).state.clone();
        state
            .read(cx)
            .list_state()
            .set_follow_mode(FollowMode::Tail);
        (
            window.downcast::<Root>().expect("Base Root"),
            state,
            latest_code_block,
        )
    })
}

fn settle(cx: &mut TestAppContext, handle: WindowHandle<Root>) {
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

fn append(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    state: &Entity<TextViewState>,
    expected_source: &mut String,
    chunk: &str,
) {
    cx.update_window(handle.into(), |_, _, cx| {
        state.update(cx, |state, cx| state.push_str(chunk, cx));
    })
    .unwrap();
    expected_source.push_str(chunk);
    settle(cx, handle);
}

fn assert_source_and_follow(
    state: &Entity<TextViewState>,
    cx: &mut TestAppContext,
    expected_source: &str,
) -> ListOffset {
    state.read_with(cx, |state, _| {
        assert_eq!(state.rendered_text().source(), expected_source);
        assert!(state.list_state().is_following_tail());
        assert_eq!(state.list_state().is_scrolled_to_end(), Some(true));
        state.list_state().logical_scroll_top()
    })
}

fn assert_code_block(observed: &LatestCodeBlock, expected_code: &str) {
    let observed = observed.latest.lock().expect("lock code block observer");
    let observed = observed.as_ref().expect("Markdown code block was rendered");
    assert_eq!(observed.language.as_deref(), Some("rust"));
    assert_eq!(observed.code.trim_end(), expected_code);
}

fn assert_reading_position(
    state: &Entity<TextViewState>,
    cx: &mut TestAppContext,
    expected_source: &str,
    expected_position: ListOffset,
    expected_text: &str,
) {
    state.read_with(cx, |state, _| {
        assert_eq!(state.rendered_text().source(), expected_source);
        assert!(!state.list_state().is_following_tail());
        assert_eq!(state.list_state().logical_scroll_top(), expected_position);
        assert!(state.rendered_text().as_str().contains(expected_text));
    });
}

#[gpui_kit::test]
fn streamed_markdown_updates_code_and_preserves_reading_position(cx: &mut TestAppContext) {
    let mut source = (0..18)
        .map(|index| format!("Paragraph {index}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    source.push_str("\n\nStreaming **bol");

    let (handle, state, latest_code_block) = open_markdown(cx, &source);
    settle(cx, handle);
    let initial_bottom = assert_source_and_follow(&state, cx, &source);

    append(
        cx,
        handle,
        &state,
        &mut source,
        "d**\n\n```rust\nfn main() {",
    );
    let opened_code_bottom = assert_source_and_follow(&state, cx, &source);
    assert_ne!(opened_code_bottom, initial_bottom);
    assert_code_block(&latest_code_block, "fn main() {");

    append(
        cx,
        handle,
        &state,
        &mut source,
        "\n    println!(\"streaming\");",
    );
    let growing_code_bottom = assert_source_and_follow(&state, cx, &source);
    assert_ne!(growing_code_bottom, opened_code_bottom);
    assert_code_block(
        &latest_code_block,
        "fn main() {\n    println!(\"streaming\");",
    );
    state.read_with(cx, |state, _| {
        let rendered = state.rendered_text();
        assert!(rendered.as_str().contains("Streaming bold"));
        assert!(rendered.as_str().contains("println!(\"streaming\");"));
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let painted = test_support::texts(window);
        assert!(
            painted
                .iter()
                .any(|text| { text.visible() && text.text().contains("println!(\"streaming\");") }),
            "code block was not painted: {painted:?}"
        );
    })
    .unwrap();

    let bottom_before_scroll =
        state.read_with(cx, |state, _| state.list_state().logical_scroll_top());
    cx.update_window(handle.into(), |_, window, cx| {
        window.scroll("markdown", ScrollDelta::Pixels(point(px(0.), px(80.))), cx);
    })
    .unwrap();
    settle(cx, handle);
    let reading_position = state.read_with(cx, |state, _| {
        assert!(!state.list_state().is_following_tail());
        assert_eq!(state.list_state().is_scrolled_to_end(), Some(false));
        let position = state.list_state().logical_scroll_top();
        assert_ne!(position, bottom_before_scroll);
        position
    });

    append(
        cx,
        handle,
        &state,
        &mut source,
        "\n    println!(\"paused\");",
    );
    assert_reading_position(
        &state,
        cx,
        &source,
        reading_position,
        "println!(\"paused\");",
    );

    append(
        cx,
        handle,
        &state,
        &mut source,
        "\n}\n```\n\nAfter scrolling",
    );
    assert_reading_position(&state, cx, &source, reading_position, "After scrolling");

    cx.update_window(handle.into(), |_, window, cx| {
        window.scroll(
            "markdown",
            ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
            cx,
        );
    })
    .unwrap();
    settle(cx, handle);
    assert_source_and_follow(&state, cx, &source);
    assert_code_block(
        &latest_code_block,
        "fn main() {\n    println!(\"streaming\");\n    println!(\"paused\");\n}",
    );
}
