use std::ops::Range;

use gpui_kit::{
    Bounds, Entity, Focusable as _, Point, TestAppContext, VisualTestContext, WindowBounds,
    WindowHandle, WindowOptions,
    base::{Root, test_support},
    component::{Theme, ThemeMode, tree::TreeItem},
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

fn rehome(session: &mut SessionProjection, project: &str, worktree: &str) {
    let key = SessionKey::new(
        WorktreeKey::new(ProjectKey::new("host", project), worktree),
        session.key.session_id.clone(),
    );
    session.key = key.clone();
    for item in Rc::make_mut(&mut session.items).iter_mut() {
        if let TimelineItem::Tool(call) = item {
            call.key = ToolCallKey::new(key.clone(), call.key.call_id.clone());
        }
    }
    for subagent in &mut session.subagents {
        subagent.key.parent = key.clone();
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

fn queued_preview() -> WorkbenchProjection {
    let mut projection = pending_preview();
    projection.sessions[0].queue = vec![
        QueueItemProjection {
            id: "q1".to_owned(),
            text: "排队：先核对 Host 快照".to_owned(),
        },
        QueueItemProjection {
            id: "q2".to_owned(),
            text: "排队：保留现有测试".to_owned(),
        },
    ];
    projection.sessions[1].queue = vec![QueueItemProjection {
        id: "q1".to_owned(),
        text: "第二个 Session 的 Queue".to_owned(),
    }];
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

fn queue_input<C: gpui_kit::AppContext>(
    view: &Entity<Workbench>,
    cx: &C,
    item_id: &str,
) -> Entity<gpui_kit::component::input::TextareaState> {
    let composer = view
        .read_with(cx, |view, _| view.composer.clone())
        .expect("Composer");
    composer
        .read_with(cx, |composer, cx| composer.queue_input(item_id, cx))
        .unwrap_or_else(|| panic!("Queue item {item_id}"))
}

fn set_queue_text(cx: &mut VisualTestContext, view: &Entity<Workbench>, item_id: &str, text: &str) {
    let input = queue_input(view, cx, item_id);
    cx.update(|window, cx| {
        input.update(cx, |input, cx| {
            let end = input.value().len();
            input.set_selected_range(0..end, cx);
            input.focus(window, cx);
        });
        window.render_frame(cx);
        window.input(text, cx);
    });
    settle(cx);
}

fn scroll_composer(cx: &mut VisualTestContext, delta: f32) {
    cx.update(|window, cx| {
        window.scroll(
            "session-composer-region",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(delta))),
            cx,
        );
    });
    settle(cx);
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
fn session_send_records_intent_without_clearing_draft_or_faking_host(cx: &mut TestAppContext) {
    let (handle, view, intents) = open_preview(cx, preview());
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "draft to send", 2..7);
    let timeline_len = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().items.len()
    });
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
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .items
            .len()),
        timeline_len,
        "a locally recorded Send must not synthesize a user message"
    );
}

#[gpui_kit::test]
fn session_queue_clears_recorded_draft_and_preserves_offline_draft(cx: &mut TestAppContext) {
    let mut running = preview();
    running.sessions[0].execution = ExecutionState::Running;
    let (handle, view, intents) = open_preview(cx, running);
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "draft to queue", 0..5);
    let timeline_len = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().items.len()
    });
    let before_scroll = scroller(&view, &cx).read_with(&cx, |state, _| state.logical_scroll_top());
    cx.update(|window, cx| {
        assert_eq!(window.find("composer-submit").label(), Some("Queue"));
        window.click("composer-submit", cx);
    });
    settle(&mut cx);
    assert_eq!(intents.borrow().len(), 1);
    match &intents.borrow()[0] {
        SessionIntent::Queue {
            session: key,
            item_id,
            text,
            model,
            effort,
        } => {
            assert_eq!(key, &session("one"));
            assert!(item_id.contains("queue-intent:1"));
            assert_eq!(text, "draft to queue");
            assert_eq!(model, "model-a");
            assert_eq!(effort.as_deref(), Some("Low"));
        }
        other => panic!("expected Queue intent, got {other:?}"),
    }
    assert_eq!(
        input(&view, &cx).read_with(&cx, |input, _| input.value()),
        ""
    );
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .items
            .len()),
        timeline_len,
        "Queue must not append a user message"
    );
    assert_eq!(
        scroller(&view, &cx).read_with(&cx, |state, _| state.logical_scroll_top()),
        before_scroll,
        "Queue must not move the reading position"
    );
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .queue
            .len()),
        1
    );

    let mut offline = preview();
    offline.connection = ConnectionState::Offline;
    offline.sessions[0].execution = ExecutionState::Running;
    replace(&mut cx, &view, offline);
    set_draft(&mut cx, &view, "offline draft", 0..7);
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .queue
            .clone()),
        vec![QueueItemProjection {
            id: format!("{}:queue-intent:1", session_id(&session("one"))),
            text: "draft to queue".to_owned(),
        }],
        "an empty snapshot must not erase a locally recorded Queue item"
    );
    cx.update(|window, cx| window.click("composer-submit", cx));
    settle(&mut cx);
    assert_eq!(intents.borrow().len(), 1, "Offline cannot emit Queue");
    assert_eq!(
        input(&view, &cx).read_with(&cx, |input, _| input.value()),
        "offline draft"
    );
}

#[gpui_kit::test]
fn queue_allocation_skips_preloaded_intent_ids(cx: &mut TestAppContext) {
    let mut projection = preview();
    projection.sessions[0].execution = ExecutionState::Running;
    let existing_id = format!("{}:queue-intent:1", session_id(&session("one")));
    projection.sessions[0].queue = vec![QueueItemProjection {
        id: existing_id.clone(),
        text: "existing queued prompt".to_owned(),
    }];
    let (handle, view, intents) = open_preview(cx, projection);
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "new queued prompt", 0..3);
    scroll_composer(&mut cx, -1000.);
    cx.update(|window, cx| window.click("composer-submit", cx));
    settle(&mut cx);

    assert_eq!(intents.borrow().len(), 1);
    match &intents.borrow()[0] {
        SessionIntent::Queue { item_id, .. } => {
            assert_ne!(item_id, &existing_id);
            assert_eq!(
                item_id,
                &format!("{}:queue-intent:2", session_id(&session("one")))
            );
        }
        other => panic!("expected Queue intent, got {other:?}"),
    }
    let queue = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().queue.clone()
    });
    assert_eq!(queue.len(), 2);
    assert_ne!(queue[0].id, queue[1].id);
}

#[gpui_kit::test]
fn queue_edit_updates_only_target_item_and_preserves_other_sessions(cx: &mut TestAppContext) {
    let (handle, view, intents) = open_preview(cx, queued_preview());
    let mut cx = visual(handle, cx);
    cx.update(|window, _| window.activate_window());
    settle(&mut cx);
    let first = queue_input(&view, &cx, "q1");
    let second = queue_input(&view, &cx, "q2");
    let first_id = first.entity_id();
    let second_id = second.entity_id();
    let timeline_len = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().items.len()
    });

    set_queue_text(
        &mut cx,
        &view,
        "q1",
        "排队：先核对 Host 快照，再决定是否扩大测试。",
    );
    assert_eq!(queue_input(&view, &cx, "q1").entity_id(), first_id);
    assert_eq!(queue_input(&view, &cx, "q2").entity_id(), second_id);
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .queue
            .clone()),
        vec![
            QueueItemProjection {
                id: "q1".to_owned(),
                text: "排队：先核对 Host 快照，再决定是否扩大测试。".to_owned(),
            },
            QueueItemProjection {
                id: "q2".to_owned(),
                text: "排队：保留现有测试".to_owned(),
            },
        ]
    );

    cx.update(|window, cx| {
        first.update(cx, |input, cx| {
            input.set_selected_range(3..9, cx);
            input.focus(window, cx);
        });
    });
    settle(&mut cx);
    assert_eq!(
        view.read_with(&cx, |view, _| {
            view.controller.as_ref().unwrap().navigation.focus().clone()
        }),
        FocusAnchor::new(MainView::Session(session("one")), Region::Queue).with_item("q1")
    );

    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("two"), window, cx));
    });
    settle(&mut cx);
    let other = queue_input(&view, &cx, "q1");
    assert_ne!(other.entity_id(), first_id);
    assert_eq!(
        other.read_with(&cx, |input, _| input.value()),
        "第二个 Session 的 Queue"
    );

    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("one"), window, cx));
    });
    settle(&mut cx);
    assert_eq!(queue_input(&view, &cx, "q1").entity_id(), first_id);
    assert_eq!(queue_input(&view, &cx, "q2").entity_id(), second_id);
    assert_eq!(
        first.read_with(&cx, |input, _| input.value()),
        "排队：先核对 Host 快照，再决定是否扩大测试。"
    );
    assert_eq!(
        first.read_with(&cx, |input, _| input.selected_range()),
        3..9
    );
    cx.update(|window, cx| {
        assert!(first.read(cx).focus_handle(cx).is_focused(window));
    });

    replace(&mut cx, &view, queued_preview());
    assert_eq!(
        first.read_with(&cx, |input, _| input.value()),
        "排队：先核对 Host 快照，再决定是否扩大测试。"
    );
    assert_eq!(
        first.read_with(&cx, |input, _| input.selected_range()),
        3..9
    );
    cx.update(|window, cx| {
        assert!(first.read(cx).focus_handle(cx).is_focused(window));
    });
    assert!(intents.borrow().is_empty(), "editing Queue is local");
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .items
            .len()),
        timeline_len
    );
}

#[gpui_kit::test]
fn queue_cancel_removes_only_target_item_and_preserves_other_sessions(cx: &mut TestAppContext) {
    let (handle, view, intents) = open_preview(cx, queued_preview());
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    let retained = queue_input(&view, &cx, "q1").entity_id();
    scroll_composer(&mut cx, -1000.);
    cx.update(|window, cx| window.click("queue-cancel:q2", cx));
    settle(&mut cx);
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .queue
            .clone()),
        vec![QueueItemProjection {
            id: "q1".to_owned(),
            text: "排队：先核对 Host 快照".to_owned(),
        }]
    );
    assert_eq!(queue_input(&view, &cx, "q1").entity_id(), retained);
    cx.update(|window, cx| {
        let input = queue_input(&view, cx, "q1");
        assert!(input.read(cx).focus_handle(cx).is_focused(window));
    });
    scroll_composer(&mut cx, 1000.);
    cx.update(|window, _| {
        let painted = test_support::texts(window);
        assert!(
            painted.iter().any(|text| text.text() == "Queue · 1"),
            "{painted:?}"
        );
    });

    let queue_editor = queue_input(&view, &cx, "q1");
    cx.update(|window, cx| {
        queue_editor.update(cx, |input, cx| input.focus(window, cx));
    });
    settle(&mut cx);
    cx.update(|window, cx| window.click("queue-cancel:q1", cx));
    settle(&mut cx);
    assert!(view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().queue.is_empty()
    }));
    assert_eq!(
        view.read_with(&cx, |view, _| view.projection.sessions[1].queue.clone()),
        vec![QueueItemProjection {
            id: "q1".to_owned(),
            text: "第二个 Session 的 Queue".to_owned(),
        }]
    );
    cx.update(|window, cx| {
        assert!(window.try_find("session-queue").is_none());
        assert!(
            input(&view, cx)
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        );
    });
    assert!(intents.borrow().is_empty(), "cancelling Queue is local");
}

#[gpui_kit::test]
fn approval_completion_does_not_replay_queue_or_append_messages(cx: &mut TestAppContext) {
    let (handle, view, intents) = open_preview(cx, queued_preview());
    let mut cx = visual(handle, cx);
    settle(&mut cx);
    set_queue_text(&mut cx, &view, "q1", "本地编辑后的 Queue");
    scroll_composer(&mut cx, -1000.);
    cx.update(|window, cx| window.click("queue-cancel:q2", cx));
    settle(&mut cx);
    scroll_composer(&mut cx, 1000.);
    let timeline_len = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().items.len()
    });
    let queues = view.read_with(&cx, |view, _| {
        view.projection
            .sessions
            .iter()
            .map(|session| session.queue.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        queues[0],
        vec![QueueItemProjection {
            id: "q1".to_owned(),
            text: "本地编辑后的 Queue".to_owned(),
        }]
    );
    cx.update(|window, cx| window.click("approval-allow-call-1", cx));
    settle(&mut cx);
    assert_eq!(intents.borrow().len(), 1);

    let mut submitting = queued_preview();
    let TimelineItem::Tool(call) = &mut Rc::make_mut(&mut submitting.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    call.approval = ApprovalState::Submitting;

    let mut running = queued_preview();
    running.sessions[0].execution = ExecutionState::Running;
    let TimelineItem::Tool(call) = &mut Rc::make_mut(&mut running.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    call.approval = ApprovalState::Decided(ApprovalDecision::AllowOnce);
    call.execution = ExecutionState::Running;

    let mut succeeded = queued_preview();
    succeeded.sessions[0].execution = ExecutionState::Succeeded;
    let TimelineItem::Tool(call) = &mut Rc::make_mut(&mut succeeded.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    call.approval = ApprovalState::Decided(ApprovalDecision::AllowOnce);
    call.execution = ExecutionState::Succeeded;
    call.output = Some("Host confirmed completion.".to_owned());

    for next in [submitting, running, succeeded] {
        replace(&mut cx, &view, next);
        assert_eq!(
            intents.borrow().len(),
            1,
            "completion must not replay Queue"
        );
        assert_eq!(
            view.read_with(&cx, |view, _| {
                view.projection
                    .sessions
                    .iter()
                    .map(|session| session.queue.clone())
                    .collect::<Vec<_>>()
            }),
            queues
        );
        assert_eq!(
            view.read_with(&cx, |view, _| view
                .current_projection()
                .unwrap()
                .items
                .len()),
            timeline_len,
            "completion must not append queued user messages"
        );
    }
    cx.update(|window, _| {
        assert_eq!(window.find("composer-submit").label(), Some("Send"));
    });
}

#[gpui_kit::test]
fn reconnect_does_not_replay_queue_or_revive_stale_approval(cx: &mut TestAppContext) {
    let mut offline = queued_preview();
    offline.connection = ConnectionState::Offline;
    let (handle, view, intents) = open_preview(cx, offline);
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "重连期间保留草稿", 0..4);
    let queue = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().queue.clone()
    });
    let timeline_len = view.read_with(&cx, |view, _| {
        view.current_projection().unwrap().items.len()
    });
    cx.update(|window, cx| {
        assert_eq!(window.find("approval-allow-call-1").disabled(), Some(true));
        window.click("approval-allow-call-1", cx);
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        window.scroll(
            "session-composer-region",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-1000.))),
            cx,
        );
    });
    settle(&mut cx);
    cx.update(|window, cx| {
        assert_eq!(window.find("composer-submit").disabled(), Some(true));
        window.click("composer-submit", cx);
    });
    settle(&mut cx);
    assert!(intents.borrow().is_empty());

    let mut reconnected = queued_preview();
    reconnected.sessions[0].generation = 8;
    reconnected.sessions[0].execution = ExecutionState::Unknown;
    let TimelineItem::Tool(call) = &mut Rc::make_mut(&mut reconnected.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    call.approval = ApprovalState::Stale;
    call.execution = ExecutionState::Unknown;
    call.output = Some("Execution: Unknown · Not replayed".to_owned());
    replace(&mut cx, &view, reconnected);
    assert!(
        intents.borrow().is_empty(),
        "reconnect must not replay intents"
    );
    assert_eq!(
        input(&view, &cx).read_with(&cx, |input, _| input.value()),
        "重连期间保留草稿"
    );
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .queue
            .clone()),
        queue
    );
    assert_eq!(
        view.read_with(&cx, |view, _| view
            .current_projection()
            .unwrap()
            .items
            .len()),
        timeline_len
    );
    cx.update(|window, _| {
        assert!(window.try_find("approval-allow-call-1").is_none());
    });

    for key in [session("two"), session("one")] {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.open_session(key, window, cx));
        });
        settle(&mut cx);
    }
    assert!(intents.borrow().is_empty());
    assert_eq!(
        view.read_with(&cx, |view, _| view.projection.connection),
        ConnectionState::Online
    );
    assert!(view.read_with(&cx, |view, _| {
        matches!(
            view.current_projection().unwrap().items[2],
            TimelineItem::Tool(ToolCallProjection {
                approval: ApprovalState::Stale,
                execution: ExecutionState::Unknown,
                ..
            })
        )
    }));
    cx.update(|window, cx| window.click("tool-call-1-summary", cx));
    settle(&mut cx);
    cx.update(|window, _| {
        assert!(
            test_support::texts(window)
                .iter()
                .any(|text| { text.text().contains("Execution: Unknown · Not replayed") })
        );
    });
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

#[gpui_kit::test]
fn native_navigation_reveal_and_refresh_preserve_manual_collapse(cx: &mut TestAppContext) {
    let mut projection = preview();
    let mut deployment = session_projection("deployment");
    rehome(&mut deployment, "atlas", "main");
    let mut review = session_projection("review");
    rehome(&mut review, "atlas", "review");
    let review_key = review.key.clone();
    projection.sessions.extend([deployment, review]);
    let review_id = session_id(&review_key);
    let atlas_id = logical_id("project", &["host", "atlas"]);

    let (handle, view, _) = open_preview(cx, projection.clone());
    let mut cx = visual(handle, cx);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.open_session(review_key, window, cx);
        });
    });
    settle(&mut cx);

    let tree = view.read_with(&cx, |view, cx| view.navigation.read(cx).tree_state(cx));
    let review_item = TreeItem::new(review_id.clone(), "");
    let atlas_item = TreeItem::new(atlas_id.clone(), "");
    tree.read_with(&cx, |tree, _| {
        assert!(tree.index_of(&review_item.id).is_some());
        assert_eq!(
            tree.selected_item().map(|item| item.id.to_string()),
            Some(review_id)
        );
    });

    cx.update(|window, cx| {
        tree.update(cx, |tree, cx| {
            tree.set_selected_item(Some(&atlas_item), cx);
            tree.focus(window, cx);
        });
        window.dispatch_action(
            Box::new(gpui_kit::base::actions::Confirm { secondary: false }),
            cx,
        );
    });
    settle(&mut cx);
    tree.read_with(&cx, |tree, _| {
        assert!(tree.index_of(&review_item.id).is_none());
        let atlas = tree
            .entry(
                tree.index_of(&atlas_item.id)
                    .expect("visible atlas Project"),
            )
            .expect("atlas entry");
        assert!(!atlas.is_expanded());
    });

    replace(&mut cx, &view, projection);
    tree.read_with(&cx, |tree, _| {
        assert!(
            tree.index_of(&review_item.id).is_none(),
            "a background refresh must not reopen the manually collapsed Project"
        );
        let atlas = tree
            .entry(
                tree.index_of(&atlas_item.id)
                    .expect("visible atlas Project"),
            )
            .expect("atlas entry");
        assert!(!atlas.is_expanded());
    });
}

#[path = "review_tests.rs"]
mod review_tests;
