//! Probe untrusted Markdown policy at the real production `AgentMarkdown` boundary.

#![cfg(feature = "workbench")]

#[path = "../src/workbench/markdown.rs"]
mod markdown;

use std::sync::{Arc, Mutex};

use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, Modifiers, MouseButton, Point, TestAppContext,
    VisualTestContext, Window, WindowBounds, WindowHandle, WindowOptions,
    base::{Root, test_support},
    component::text::TextViewState,
    div, point,
    prelude::*,
    px, size,
    test::TestWindowExt as _,
};
use markdown::{AgentMarkdown, LinkConfirmation, MarkdownEvent};

#[derive(Default)]
struct SecurityRecord {
    link_clicks: Vec<String>,
    rejected_links: Vec<String>,
    cancelled: Vec<String>,
    rejected_confirmations: Vec<String>,
    pending: Option<String>,
    image_requests: Vec<String>,
    image_loads: Vec<String>,
}

type SharedRecord = Arc<Mutex<SecurityRecord>>;

struct MarkdownProbe {
    state: Entity<TextViewState>,
    record: SharedRecord,
    truncate_confirmation: bool,
}

fn observe(record: &SharedRecord) -> impl Fn(MarkdownEvent) + 'static {
    let record = Arc::clone(record);
    move |event| {
        let mut record = record.lock().expect("lock security record");
        match event {
            MarkdownEvent::LinkClicked(target) => record.link_clicks.push(target),
            MarkdownEvent::LinkRejected(target) => record.rejected_links.push(target),
            MarkdownEvent::Cancelled(target) => record.cancelled.push(target),
            MarkdownEvent::ConfirmationRejected(target) => {
                record.rejected_confirmations.push(target);
            }
            MarkdownEvent::ImageRequested(uri) => record.image_requests.push(uri),
            MarkdownEvent::ImageRejected(uri) => record.image_loads.push(uri),
            MarkdownEvent::ConfirmationRequested(_) => {}
        }
    }
}

impl Render for MarkdownProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let pending = self
            .record
            .lock()
            .expect("lock security record")
            .pending
            .clone();
        let confirmation_record = Arc::clone(&self.record);
        let markdown = AgentMarkdown::new(&self.state)
            .observe(observe(&self.record))
            .on_confirmation(move |target, window, _| {
                confirmation_record
                    .lock()
                    .expect("lock security record")
                    .pending = Some(target);
                window.refresh();
            });

        div()
            .relative()
            .size_full()
            .child(markdown)
            .children(pending.map(|target| {
                let shown_target = if self.truncate_confirmation {
                    format!("{}…", target.chars().take(24).collect::<String>())
                } else {
                    target.clone()
                };
                let close_record = Arc::clone(&self.record);
                LinkConfirmation::new(target)
                    .presented_target(shown_target)
                    .observe(observe(&self.record))
                    .on_close(move |window, _| {
                        close_record
                            .lock()
                            .expect("lock security record")
                            .pending
                            .take();
                        window.refresh();
                    })
            }))
    }
}

fn open_markdown(
    cx: &mut TestAppContext,
    source: &str,
    truncate_confirmation: bool,
) -> (WindowHandle<Root>, Entity<TextViewState>, SharedRecord) {
    cx.update(|cx| {
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(900.), px(640.)),
                })),
                ..Default::default()
            },
            cx,
            |_, cx| {
                cx.new(|cx| MarkdownProbe {
                    state: cx.new(|cx| TextViewState::markdown(source, cx)),
                    record: SharedRecord::default(),
                    truncate_confirmation,
                })
            },
        )
        .expect("open Markdown security window");
        let state = view.read(cx).state.clone();
        let record = Arc::clone(&view.read(cx).record);
        (window.downcast::<Root>().expect("Base Root"), state, record)
    })
}

fn visual(handle: WindowHandle<Root>, cx: &TestAppContext) -> VisualTestContext {
    VisualTestContext::from_window(handle.into(), cx)
}

fn settle(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
    });
    cx.run_until_parked();
}

fn click_first_link(cx: &mut VisualTestContext) {
    cx.simulate_click(point(px(10.), px(10.)), Modifiers::default());
    settle(cx);
}

#[gpui_kit::test]
fn workbench_markdown_security_links_never_open_without_explicit_primary_activation(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let target = "https://example.com/complete";
    let (handle, _, record) = open_markdown(cx, &format!("[link]({target})"), false);
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    assert_eq!(cx.opened_url(), None);
    assert_eq!(
        record
            .lock()
            .expect("lock security record")
            .link_clicks
            .len(),
        0
    );

    cx.simulate_mouse_down(
        point(px(10.), px(10.)),
        MouseButton::Right,
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(px(10.), px(10.)),
        MouseButton::Right,
        Modifiers::default(),
    );
    settle(&mut cx);
    assert_eq!(cx.opened_url(), None);
    assert_eq!(
        record.lock().expect("lock security record").rejected_links,
        [target]
    );

    for target in [
        "http://example.com/insecure",
        "data:text/plain,secret",
        "/relative/path",
        "relative/path",
        "mailto:agent@example.com",
        "unknown+probe:payload",
    ] {
        let (handle, _, record) = open_markdown(&mut cx, &format!("[link]({target})"), false);
        let mut link_cx = visual(handle, &cx);
        settle(&mut link_cx);
        click_first_link(&mut link_cx);
        let record = record.lock().expect("lock security record");
        assert_eq!(record.link_clicks, [target]);
        assert_eq!(record.rejected_links, [target]);
        assert!(record.pending.is_none());
        drop(record);
        link_cx.update(|window, _| assert!(window.try_find("confirm-link").is_none()));
        assert_eq!(link_cx.opened_url(), None);
    }
}

#[gpui_kit::test]
fn workbench_markdown_security_https_confirmation_rejects_cancel_and_truncated_targets(
    cx: &mut TestAppContext,
) {
    const TARGET: &str = "https://example.com/a/complete/target?query=1#fragment";
    cx.update(gpui_kit::init);

    let (handle, _, record) = open_markdown(cx, &format!("[link]({TARGET})"), false);
    let mut cancel_cx = visual(handle, cx);
    settle(&mut cancel_cx);
    click_first_link(&mut cancel_cx);
    cancel_cx.update(|window, cx| window.click("cancel-link", cx));
    settle(&mut cancel_cx);
    assert_eq!(
        record.lock().expect("lock security record").cancelled,
        [TARGET]
    );
    assert_eq!(cancel_cx.opened_url(), None);

    let (handle, _, record) = open_markdown(cx, &format!("[link]({TARGET})"), true);
    let mut truncated_cx = visual(handle, cx);
    settle(&mut truncated_cx);
    click_first_link(&mut truncated_cx);
    truncated_cx.update(|window, cx| {
        let painted = test_support::texts(window);
        assert!(painted.iter().any(|text| text.text().contains('…')));
        assert!(!painted.iter().any(|text| text.text() == TARGET));
        window.click("confirm-link", cx);
    });
    settle(&mut truncated_cx);
    assert_eq!(
        record
            .lock()
            .expect("lock security record")
            .rejected_confirmations,
        [TARGET]
    );
    assert_eq!(truncated_cx.opened_url(), None);
}

#[gpui_kit::test]
fn workbench_markdown_security_only_complete_confirmed_https_reaches_open_url(
    cx: &mut TestAppContext,
) {
    const TARGET: &str = "https://example.com/a/complete/target?query=1#fragment";
    cx.update(gpui_kit::init);
    let (handle, _, record) = open_markdown(cx, &format!("[link]({TARGET})"), false);
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    click_first_link(&mut cx);
    assert_eq!(cx.opened_url(), None);
    assert_eq!(
        record
            .lock()
            .expect("lock security record")
            .pending
            .as_deref(),
        Some(TARGET)
    );

    cx.update(|window, cx| {
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        let painted = test_support::texts(window);
        let target = painted
            .iter()
            .find(|text| text.text() == TARGET)
            .expect("complete target is painted");
        assert!(target.visible());
        assert!(viewport.contains(&target.bounds().origin));
        assert!(viewport.contains(&target.bounds().bottom_right()));
        window.click("confirm-link", cx);
    });
    settle(&mut cx);
    assert_eq!(cx.opened_url().as_deref(), Some(TARGET));
    assert!(
        record
            .lock()
            .expect("lock security record")
            .pending
            .is_none()
    );
}

#[gpui_kit::test]
fn workbench_markdown_security_raw_html_is_painted_as_text_without_image_resolution(
    cx: &mut TestAppContext,
) {
    const SOURCE: &str = r#"<div>block</div>

before <b>bold</b> <img src="https://example.com/raw.png"> after"#;
    cx.update(gpui_kit::init);
    let (handle, state, record) = open_markdown(cx, SOURCE, false);
    let mut cx = visual(handle, cx);
    settle(&mut cx);

    let rendered = state.read_with(&cx, |state, _| state.rendered_text().as_str().to_string());
    assert!(rendered.contains("<div>block</div>"), "{rendered}");
    assert!(rendered.contains("<b>bold</b>"), "{rendered}");
    assert!(rendered.contains(r#"<img src="https://example.com/raw.png">"#));
    cx.update(|window, _| {
        let painted = test_support::texts(window);
        assert!(painted.iter().any(|text| text.text().contains("<b>")));
        assert!(painted.iter().any(|text| text.text().contains("<img src=")));
    });
    assert_eq!(
        record
            .lock()
            .expect("lock security record")
            .image_requests
            .len(),
        0
    );
    assert_eq!(cx.opened_url(), None);
}

#[gpui_kit::test]
fn workbench_markdown_security_every_image_uses_side_effect_free_rejection_source(
    cx: &mut TestAppContext,
) {
    const REMOTE: &str = "https://example.com/image.png";
    const FILE: &str = "file:///tmp/metis-secret.png";
    const RELATIVE: &str = "./metis-secret.png";
    const DATA: &str = "data:image/png;base64,iVBORw0KGgo=";
    const SOURCE: &str = "![remote](https://example.com/image.png)\n\n![file](file:///tmp/metis-secret.png)\n\n![relative](./metis-secret.png)\n\n![data](data:image/png;base64,iVBORw0KGgo=)";

    cx.update(gpui_kit::init);
    let (handle, _, record) = open_markdown(cx, SOURCE, false);
    let mut cx = visual(handle, cx);
    settle(&mut cx);

    let record = record.lock().expect("lock security record");
    for uri in [REMOTE, FILE, RELATIVE, DATA] {
        assert!(
            record.image_requests.iter().any(|request| request == uri),
            "missing rejection request for {uri}: {:?}",
            record.image_requests
        );
        assert!(
            record.image_loads.iter().any(|load| load == uri),
            "image did not consume its custom None source for {uri}: {:?}",
            record.image_loads
        );
    }
    drop(record);
    assert_eq!(cx.opened_url(), None);
}
