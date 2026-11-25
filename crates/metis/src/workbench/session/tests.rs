use std::ops::Range;

use gpui_kit::{
    Bounds, Entity, Focusable as _, Point, TestAppContext, VisualTestContext, WindowBounds,
    WindowHandle, WindowOptions,
    base::{Root, test_support},
    component::{Theme, ThemeMode},
    px, size,
    test::TestWindowExt as _,
};

use super::*;
use crate::workbench::{
    identity::{ProjectKey, WorktreeKey},
    stats::{ActivityInput, ContextBreakdown, ContextInput, UsageInput},
};

fn session(id: &str) -> SessionKey {
    SessionKey::new(
        WorktreeKey::new(ProjectKey::new("host", "project"), "worktree"),
        id,
    )
}

fn models() -> Vec<ModelSpec> {
    vec![
        ModelSpec {
            id: "model-a".to_owned(),
            label: "Model A".to_owned(),
            default_effort: Some("Low".to_owned()),
            efforts: vec!["Low".to_owned(), "High".to_owned()],
        },
        ModelSpec {
            id: "model-b".to_owned(),
            label: "Model B".to_owned(),
            default_effort: Some("High".to_owned()),
            efforts: vec!["Low".to_owned(), "High".to_owned()],
        },
    ]
}

fn session_projection(id: &str) -> SessionProjection {
    let key = session(id);
    let call = ToolCallProjection {
        key: ToolCallKey::new(key.clone(), "call-1"),
        title: "read_file".to_owned(),
        arguments: "/workspace/a/long/path.rs".to_owned(),
        execution: ExecutionState::NotExecuted,
        approval: ApprovalState::NotRequested,
        output: None,
    };
    SessionProjection {
        key: key.clone(),
        title: format!("Session {id}"),
        generation: 1,
        execution: ExecutionState::NotExecuted,
        items: Rc::new(vec![
            TimelineItem::User {
                id: "user-1".to_owned(),
                text: "请检查这个文件".to_owned(),
            },
            TimelineItem::Model {
                id: "model-1".to_owned(),
                markdown: "我会先阅读文件，再保留现有行为。".to_owned(),
            },
            TimelineItem::Tool(call),
        ]),
        queue: Vec::new(),
        permission: SessionPermissionPolicy::AskEachTime,
        models: models(),
        initial_model: Some("model-a".to_owned()),
        stats: StatsInput {
            activity: Some(ActivityInput {
                turns: 2,
                steps: 3,
                llm_ms: 1200,
                tool_ms: 300,
                ttft_ms: 400,
                ttft_steps: 2,
                decode_ms: 1000,
                decode_tokens: 20,
            }),
            usage: Some(UsageInput {
                uncached_input: 100,
                cache_read: 50,
                cache_write: 0,
                output: 25,
            }),
            context: Some(ContextInput {
                context_window: Some(1000),
                projected_tokens: 250,
                breakdown: Some(ContextBreakdown {
                    system: 50,
                    tools: 50,
                    messages: 150,
                }),
            }),
        },
        subagents: Vec::new(),
    }
}

fn preview() -> WorkbenchProjection {
    WorkbenchProjection {
        connection: ConnectionState::Online,
        sessions: vec![session_projection("one"), session_projection("two")],
    }
}

fn pending_preview() -> WorkbenchProjection {
    let mut projection = preview();
    let TimelineItem::Tool(call) = &mut Rc::make_mut(&mut projection.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    call.approval = ApprovalState::Pending;
    projection
}

fn decided_preview() -> WorkbenchProjection {
    let mut projection = pending_preview();
    let TimelineItem::Tool(call) = &mut Rc::make_mut(&mut projection.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    call.approval = ApprovalState::Decided(ApprovalDecision::AllowOnce);
    call.execution = ExecutionState::Running;
    call.output = Some("Host output arrives on the original call.".to_owned());
    projection
}

fn open_preview(
    cx: &mut TestAppContext,
    projection: WorkbenchProjection,
) -> (
    WindowHandle<Root>,
    Entity<Workbench>,
    Rc<RefCell<Vec<SessionIntent>>>,
) {
    cx.update(gpui_kit::init);
    let intents = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&intents);
    let effects: EffectHandler = Rc::new(RefCell::new(move |intent| {
        recorded.borrow_mut().push(intent);
        EffectResult::Recorded
    }));
    cx.update(|cx| {
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1100.), px(720.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let view = cx.new(|cx| Workbench::new(projection, &effects, window, cx));
                view.update(cx, |this, cx| this.mount(window, cx));
                view
            },
        )
        .expect("open Session preview");
        (window.downcast::<Root>().expect("Base Root"), view, intents)
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

fn input<C: gpui_kit::AppContext>(
    view: &Entity<Workbench>,
    cx: &C,
) -> Entity<gpui_kit::component::input::TextareaState> {
    let composer = view
        .read_with(cx, |view, _| view.composer.clone())
        .expect("Composer");
    composer.read_with(cx, super::composer::Composer::input_state)
}

fn scroller<C: gpui_kit::AppContext>(
    view: &Entity<Workbench>,
    cx: &C,
) -> Entity<gpui_kit::component::message_scroller::MessageScrollerState> {
    let timeline = view
        .read_with(cx, |view, _| view.timeline.clone())
        .expect("Timeline");
    timeline.read_with(cx, super::timeline::SessionTimeline::scroller_state)
}

fn set_draft(
    cx: &mut VisualTestContext,
    view: &Entity<Workbench>,
    text: &str,
    selection: Range<usize>,
) {
    let input = input(view, cx);
    cx.update(|window, cx| {
        input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.set_selected_range(selection, cx);
            input.focus(window, cx);
        });
    });
    settle(cx);
}

fn replace(cx: &mut VisualTestContext, view: &Entity<Workbench>, projection: WorkbenchProjection) {
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.replace_projection(projection, window, cx);
        });
    });
    settle(cx);
}

#[test]
fn session_action_availability_is_explicit_for_send_queue_offline_and_empty_draft() {
    assert_eq!(
        super::composer::primary_action(
            ConnectionState::Online,
            ExecutionState::NotExecuted,
            false,
            true,
            "draft",
        ),
        Some(super::composer::ComposerAction::Send)
    );
    assert_eq!(
        super::composer::primary_action(
            ConnectionState::Online,
            ExecutionState::Running,
            false,
            true,
            "draft",
        ),
        Some(super::composer::ComposerAction::Queue)
    );
    assert_eq!(
        super::composer::primary_action(
            ConnectionState::Online,
            ExecutionState::NotExecuted,
            true,
            true,
            "draft",
        ),
        Some(super::composer::ComposerAction::Queue)
    );
    assert_eq!(
        super::composer::primary_action(
            ConnectionState::Offline,
            ExecutionState::NotExecuted,
            false,
            true,
            "draft",
        ),
        None
    );
    assert_eq!(
        super::composer::primary_action(
            ConnectionState::Online,
            ExecutionState::NotExecuted,
            false,
            true,
            "  ",
        ),
        None
    );
}

#[test]
fn session_controller_first_focus_and_overlay_return_are_pure_intents() {
    let projection = preview();
    let snapshot = navigation_snapshot(&projection, projection.sessions[0].key.clone());
    let mut controller = MainViewController::new(&projection.sessions[0], snapshot);
    let Some(FocusIntent::Focus(initial)) = controller.take_focus_intent() else {
        panic!("initial focus intent");
    };
    assert_eq!(initial.scope, FocusScope::Shell);
    assert_eq!(initial.anchor.region, Region::Composer);

    controller.model_menu_changed(true, Some(&projection.sessions[0]));
    assert_eq!(controller.focus.overlay_depth(), 1);
    let Some(FocusIntent::Focus(overlay)) = controller.take_focus_intent() else {
        panic!("overlay focus intent");
    };
    assert_eq!(
        overlay.scope,
        FocusScope::Overlay("model-effort".to_owned())
    );
    controller.model_menu_changed(false, Some(&projection.sessions[0]));
    assert_eq!(controller.focus.overlay_depth(), 0);
    let Some(FocusIntent::CloseOverlay { restore, .. }) = controller.take_focus_intent() else {
        panic!("overlay close intent");
    };
    assert_eq!(restore.anchor.region, Region::Composer);
}

#[gpui_kit::test]
fn session_send_and_queue_record_intents_without_clearing_draft_or_faking_host(
    cx: &mut TestAppContext,
) {
    let (handle, view, intents) = open_preview(cx, preview());
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "draft to send", 2..7);
    cx.update(|window, cx| {
        assert_eq!(window.find("composer-submit").label(), Some("Send"));
        window.click("composer-submit", cx);
    });
    settle(&mut cx);
    assert_eq!(
        intents.borrow().as_slice(),
        &[SessionIntent::Send {
            session: session("one"),
            text: "draft to send".to_owned(),
            model: "model-a".to_owned(),
            effort: Some("Low".to_owned()),
        }]
    );
    assert_eq!(
        input(&view, &cx).read_with(&cx, |input, _| input.value()),
        "draft to send"
    );

    let mut running = preview();
    running.sessions[0].execution = ExecutionState::Running;
    replace(&mut cx, &view, running);
    cx.update(|window, cx| {
        assert_eq!(window.find("composer-submit").label(), Some("Queue"));
        window.click("composer-submit", cx);
    });
    settle(&mut cx);
    assert_eq!(intents.borrow().len(), 2);
    match &intents.borrow()[1] {
        SessionIntent::Queue {
            session: key,
            item_id,
            text,
            model,
            effort,
        } => {
            assert_eq!(key, &session("one"));
            assert!(item_id.contains("queue-intent:1"));
            assert_eq!(text, "draft to send");
            assert_eq!(model, "model-a");
            assert_eq!(effort.as_deref(), Some("Low"));
        }
        other => panic!("expected Queue intent, got {other:?}"),
    }

    let mut offline = preview();
    offline.connection = ConnectionState::Offline;
    offline.sessions[0].execution = ExecutionState::Running;
    replace(&mut cx, &view, offline);
    cx.update(|window, cx| window.click("composer-submit", cx));
    settle(&mut cx);
    assert_eq!(intents.borrow().len(), 2, "Offline cannot emit Queue");
    assert_eq!(
        input(&view, &cx).read_with(&cx, |input, _| input.value()),
        "draft to send"
    );
}

#[gpui_kit::test]
fn user_messages_have_no_author_label_and_use_locatable_bubbles(cx: &mut TestAppContext) {
    let long_message =
        "请检查这个文件并保留现有行为，确保长中文内容能够在用户气泡中自然换行。".repeat(4);
    let mut projection = preview();
    let TimelineItem::User { text, .. } = &mut Rc::make_mut(&mut projection.sessions[0].items)[0]
    else {
        panic!("user fixture");
    };
    *text = long_message.clone();
    let (handle, _view, _) = open_preview(cx, projection);
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    cx.update(|window, _| {
        let bubble = window.find("user-message-user-1");
        assert!(bubble.visible());
        let bubble = bubble.bounds();
        let row = window.find("user-message-row-user-1").bounds();
        let timeline = window.find("session-timeline-region").bounds();
        assert!(
            bubble.size.width <= timeline.size.width * 0.8 + px(1.),
            "bubble exceeds 80% of the Timeline: {bubble:?} vs {timeline:?}"
        );
        let bubble_right = bubble.origin.x + bubble.size.width;
        let row_right = row.origin.x + row.size.width;
        assert!(
            bubble_right >= row_right - px(1.) && bubble_right <= row_right + px(1.),
            "bubble is not aligned to the row's right edge: {bubble:?} vs {row:?}"
        );
        assert!(
            bubble.size.height > px(50.),
            "long Chinese text did not wrap: {bubble:?}"
        );
        let painted = test_support::texts(window);
        assert!(
            painted
                .iter()
                .any(|text| text.text().contains(long_message.as_str()))
        );
        assert!(!painted.iter().any(|text| text.text() == "You"));
    });
}

#[gpui_kit::test]
fn approval_dock_precedes_the_editor_and_stays_outside_the_tool_card(cx: &mut TestAppContext) {
    let (handle, _view, _) = open_preview(cx, pending_preview());
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    cx.update(|window, _| {
        assert!(window.find("approval-allow-call-1").visible());
        assert_ne!(window.find("approval-allow-call-1").disabled(), Some(true));
        assert!(window.find("approval-reject-call-1").visible());
        for button in ["approval-allow-call-1", "approval-reject-call-1"] {
            assert!(
                window
                    .within("approval-dock-call-1")
                    .try_find(button)
                    .is_some(),
                "{button} must be inside the decision dock"
            );
        }
        let dock = window.find("approval-dock-call-1").bounds();
        let editor = window.find("composer-editor").bounds();
        assert!(
            dock.origin.y + dock.size.height <= editor.origin.y + px(1.),
            "decision dock must be above the editor: {dock:?} vs {editor:?}"
        );
        assert!(
            window
                .within("session-composer-region")
                .try_find("approval-allow-call-1")
                .is_some()
        );
        for button in ["approval-allow-call-1", "approval-reject-call-1"] {
            assert!(window.within("tool-call-call-1").try_find(button).is_none());
        }
        let painted = test_support::texts(window);
        for expected in [
            "Approval for this tool call",
            "read_file",
            "/workspace/a/long/path.rs",
        ] {
            assert!(
                painted.iter().any(|text| text.text().contains(expected)),
                "missing approval content {expected:?}: {painted:?}"
            );
        }
    });
}

#[gpui_kit::test]
fn session_approval_intent_updates_the_same_tool_card_without_an_execution_card(
    cx: &mut TestAppContext,
) {
    let (handle, view, intents) = open_preview(cx, pending_preview());
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    let before = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().items.len()
    });
    assert_eq!(
        scroller(&view, &cx).read_with(&cx, |state, _| state.item_count()),
        before
    );
    cx.update(|window, cx| window.click("approval-allow-call-1", cx));
    settle(&mut cx);
    assert_eq!(
        intents.borrow().as_slice(),
        &[SessionIntent::Approval {
            call: ToolCallKey::new(session("one"), "call-1"),
            decision: ApprovalDecision::AllowOnce,
            generation: 1,
        }]
    );
    assert!(
        matches!(
            view.read_with(&cx, |view, _| view.current_projection().unwrap().items[2]
                .clone()),
            TimelineItem::Tool(ToolCallProjection {
                approval: ApprovalState::Pending,
                ..
            })
        ),
        "an intent must not become a local approval fact"
    );

    let mut submitting = pending_preview();
    let TimelineItem::Tool(call) = &mut Rc::make_mut(&mut submitting.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    call.approval = ApprovalState::Submitting;
    replace(&mut cx, &view, submitting);
    cx.update(|window, _| {
        assert_eq!(window.find("approval-allow-call-1").disabled(), Some(true));
        assert_eq!(window.find("approval-reject-call-1").disabled(), Some(true));
    });

    let mut offline = pending_preview();
    offline.connection = ConnectionState::Offline;
    replace(&mut cx, &view, offline);
    cx.update(|window, _| {
        assert_eq!(window.find("approval-allow-call-1").disabled(), Some(true));
        assert_eq!(window.find("approval-reject-call-1").disabled(), Some(true));
    });

    replace(&mut cx, &view, decided_preview());
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .items
            .len()),
        before
    );
    cx.update(|window, cx| {
        assert_eq!(window.find_all("tool-call-1-summary").len(), 1);
        assert!(window.try_find("approval-allow-call-1").is_none());
        window.click("tool-call-1-summary", cx);
    });
    settle(&mut cx);
    cx.update(|window, _| {
        let painted = test_support::texts(window);
        assert!(painted.iter().any(|text| {
            text.text()
                .contains("Host output arrives on the original call.")
        }));
    });
}

#[gpui_kit::test]
fn decided_approval_restores_the_original_tool_summary_focus(cx: &mut TestAppContext) {
    let (handle, view, _) = open_preview(cx, pending_preview());
    let mut cx = visual(handle, cx);
    cx.update(|window, _| window.activate_window());
    settle(&mut cx);
    cx.update(|window, _| {
        assert_eq!(window.find("approval-dock-call-1").focused(), Some(true));
    });

    replace(&mut cx, &view, decided_preview());
    cx.update(|window, _| {
        assert_eq!(window.find("tool-call-1-summary").focused(), Some(true));
    });
}

#[gpui_kit::test]
fn decided_approval_restores_tool_summary_from_decision_button_focus(cx: &mut TestAppContext) {
    let (handle, view, _) = open_preview(cx, pending_preview());
    let mut cx = visual(handle, cx);
    cx.update(|window, _| window.activate_window());
    settle(&mut cx);
    set_draft(&mut cx, &view, "keep this draft", 2..7);
    cx.update(|window, cx| {
        for _ in 0..8 {
            window.press("shift-tab", cx);
            if window.find("approval-allow-call-1").focused() == Some(true)
                || window.find("approval-reject-call-1").focused() == Some(true)
            {
                break;
            }
        }
        assert!(
            window.find("approval-allow-call-1").focused() == Some(true)
                || window.find("approval-reject-call-1").focused() == Some(true),
            "keyboard focus did not reach a decision button"
        );
    });
    settle(&mut cx);
    let saved = view.read_with(&cx, |view, _| {
        view.controller.as_ref().unwrap().navigation.focus().clone()
    });
    assert_eq!(saved.region, Region::Approval, "saved focus: {saved:?}");

    replace(&mut cx, &view, decided_preview());
    cx.update(|window, _| {
        assert_eq!(window.find("tool-call-1-summary").focused(), Some(true));
    });
}

#[gpui_kit::test]
fn decided_approval_preserves_a_focused_draft_and_its_selection(cx: &mut TestAppContext) {
    let (handle, view, _) = open_preview(cx, pending_preview());
    let mut cx = visual(handle, cx);
    cx.update(|window, _| window.activate_window());
    settle(&mut cx);
    set_draft(&mut cx, &view, "keep this draft", 2..7);
    let input = input(&view, &cx);
    let saved = view.read_with(&cx, |view, _| {
        view.controller.as_ref().unwrap().navigation.focus().clone()
    });
    assert_eq!(saved.region, Region::Composer, "saved focus: {saved:?}");

    replace(&mut cx, &view, decided_preview());
    assert_eq!(
        input.read_with(&cx, |input, _| input.value()),
        "keep this draft"
    );
    assert_eq!(
        input.read_with(&cx, |input, _| input.selected_range()),
        2..7
    );
    cx.update(|window, cx| {
        assert!(input.read(cx).focus_handle(cx).is_focused(window));
    });
}

#[gpui_kit::test]
fn session_menu_keyboard_maps_checked_submenus_and_model_default_effort(cx: &mut TestAppContext) {
    let (handle, view, intents) = open_preview(cx, preview());
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    let composer = view
        .read_with(&cx, |view, _| view.composer.clone())
        .unwrap();
    let focus = composer.read_with(&cx, super::composer::Composer::menu_focus);
    cx.update(|window, cx| {
        window.focus(&focus, cx);
        window.render_frame(cx);
        assert_eq!(window.find("model-effort").focused(), Some(true));
        window.press("enter", cx);
        let mut menu = window.within("popup-menu");
        menu.press("down", cx);
        menu.press("left", cx);
        window.render_frame(cx);
        let submenu = window.within("submenu");
        assert_eq!(submenu.find(0usize).checked(), Some(true));
        assert_eq!(submenu.find(1usize).checked(), Some(false));
        window.press("down", cx);
        window.press("enter", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert_eq!(window.find("model-effort").label(), Some("Model B + High"));
        assert_eq!(window.find("model-effort").focused(), Some(true));
        window.press("enter", cx);
        let mut menu = window.within("popup-menu");
        menu.press("down", cx);
        menu.press("down", cx);
        menu.press("left", cx);
        window.render_frame(cx);
        let submenu = window.within("submenu");
        assert_eq!(submenu.find(0usize).checked(), Some(false));
        assert_eq!(submenu.find(1usize).checked(), Some(true));
        window.press("enter", cx);
    });
    settle(&mut cx);
    cx.update(|window, _| {
        assert_eq!(window.find("model-effort").label(), Some("Model B + Low"));
        assert!(window.try_find("popup-menu").is_none());
    });
    set_draft(&mut cx, &view, "model intent", 0..4);
    cx.update(|window, cx| window.click("composer-submit", cx));
    settle(&mut cx);
    assert_eq!(
        intents.borrow().as_slice(),
        &[SessionIntent::Send {
            session: session("one"),
            text: "model intent".to_owned(),
            model: "model-b".to_owned(),
            effort: Some("Low".to_owned()),
        }]
    );
}

#[gpui_kit::test]
fn session_theme_switch_preserves_draft_selection_scroll_focus_and_entities(
    cx: &mut TestAppContext,
) {
    let mut projection = preview();
    let extra = (0..24)
        .map(|index| TimelineItem::User {
            id: format!("history-{index}"),
            text: format!("历史消息 {index} · a long reading row"),
        })
        .collect::<Vec<_>>();
    Rc::make_mut(&mut projection.sessions[0].items).splice(0..0, extra);
    let (handle, view, _) = open_preview(cx, projection);
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "draft selection", 2..8);
    let input = input(&view, &cx);
    let scroller = scroller(&view, &cx);
    scroller.update(&mut cx, |scroller, cx| {
        scroller.scroll_to_item(5, cx);
    });
    settle(&mut cx);
    let input_id = input.entity_id();
    let scroller_id = scroller.entity_id();
    let before_scroll = scroller.read_with(&cx, |scroller, _| scroller.logical_scroll_top());
    let before_focus = view.read_with(&cx, |view, _| {
        view.controller
            .as_ref()
            .unwrap()
            .focus
            .shell_anchor()
            .clone()
    });
    let before_view = view.read_with(&cx, |view, _| {
        view.controller
            .as_ref()
            .unwrap()
            .navigation
            .current_view()
            .clone()
    });
    cx.update(|window, cx| window.click("theme-toggle", cx));
    settle(&mut cx);

    cx.update(|_, cx| assert_eq!(Theme::global(cx).mode, ThemeMode::Dark));
    assert_eq!(input.entity_id(), input_id);
    assert_eq!(scroller.entity_id(), scroller_id);
    assert_eq!(
        input.read_with(&cx, |input, _| input.value()),
        "draft selection"
    );
    assert_eq!(
        input.read_with(&cx, |input, _| input.selected_range()),
        2..8
    );
    assert_eq!(
        scroller.read_with(&cx, |scroller, _| scroller.logical_scroll_top().item_ix),
        before_scroll.item_ix
    );
    assert_eq!(
        view.read_with(&cx, |view, _| {
            view.controller
                .as_ref()
                .unwrap()
                .focus
                .shell_anchor()
                .clone()
        }),
        before_focus
    );
    assert_eq!(
        view.read_with(&cx, |view, _| {
            view.controller
                .as_ref()
                .unwrap()
                .navigation
                .current_view()
                .clone()
        }),
        before_view
    );
    cx.update(|window, cx| {
        assert!(input.read(cx).focus_handle(cx).is_focused(window));
    });
}

#[gpui_kit::test]
fn session_switch_restores_each_sessions_draft_selection_and_input_entity(cx: &mut TestAppContext) {
    let (handle, view, _) = open_preview(cx, preview());
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "first draft", 1..5);
    let first_input = input(&view, &cx);
    let first_id = first_input.entity_id();
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("two"), window, cx));
    });
    settle(&mut cx);
    set_draft(&mut cx, &view, "second draft", 0..3);
    let second_id = input(&view, &cx).entity_id();
    assert_ne!(first_id, second_id);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("one"), window, cx));
    });
    settle(&mut cx);
    let restored = input(&view, &cx);
    assert_eq!(restored.entity_id(), first_id);
    assert_eq!(
        restored.read_with(&cx, |input, _| input.value()),
        "first draft"
    );
    assert_eq!(
        restored.read_with(&cx, |input, _| input.selected_range()),
        1..5
    );
}

#[path = "review_tests.rs"]
mod review_tests;
