use super::*;

#[gpui_kit::test]
fn deleted_session_falls_back_prunes_native_state_and_cannot_emit_old_effects(
    cx: &mut TestAppContext,
) {
    let (handle, view, intents) = open_preview(cx, preview());
    let mut cx = visual(handle, cx);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("two"), window, cx));
    });
    settle(&mut cx);
    set_draft(&mut cx, &view, "second draft", 0..3);
    let second_input = input(&view, &cx).downgrade();
    let second_scroller = scroller(&view, &cx).downgrade();

    let mut running = preview();
    running.sessions[1].execution = ExecutionState::Running;
    replace(&mut cx, &view, running);
    cx.update(|window, cx| window.click("composer-submit", cx));
    settle(&mut cx);
    assert_eq!(intents.borrow().len(), 1, "fixture emits one Queue intent");

    let mut remaining = preview();
    remaining.sessions.remove(1);
    remaining.connection = ConnectionState::Offline;
    replace(&mut cx, &view, remaining);
    assert_eq!(
        view.read_with(&cx, |view, _| {
            view.controller
                .as_ref()
                .and_then(MainViewController::current_session)
                .cloned()
        }),
        Some(session("one"))
    );
    assert_ne!(
        input(&view, &cx).read_with(&cx, |input, _| input.value()),
        "second draft"
    );
    assert_eq!(
        view.read_with(&cx, |view, _| {
            view.timeline
                .as_ref()
                .expect("Timeline")
                .read_with(&cx, |timeline, _| timeline.retained_session_count())
        }),
        1
    );
    assert_eq!(
        view.read_with(&cx, |view, _| {
            view.composer
                .as_ref()
                .expect("Composer")
                .read_with(&cx, |composer, _| composer.retained_session_count())
        }),
        1
    );
    assert!(
        view.read_with(&cx, |view, _| {
            view.controller
                .as_ref()
                .unwrap()
                .navigation
                .session_local(&session("two"))
                .is_none()
        }),
        "deleted Session local state must be pruned"
    );
    cx.update(|window, cx| window.click("composer-submit", cx));
    settle(&mut cx);
    assert_eq!(
        intents.borrow().len(),
        1,
        "Offline cannot emit an old effect"
    );
    assert!(
        second_input.upgrade().is_none(),
        "deleted Textarea was released"
    );
    assert!(
        second_scroller.upgrade().is_none(),
        "deleted MessageScroller was released"
    );

    replace(
        &mut cx,
        &view,
        WorkbenchProjection {
            connection: ConnectionState::Offline,
            sessions: Vec::new(),
        },
    );
    assert!(view.read_with(&cx, |view, _| view.controller.is_none()));
    assert!(view.read_with(&cx, |view, _| view.timeline.is_none()));
    assert!(view.read_with(&cx, |view, _| view.composer.is_none()));
    cx.update(|window, _| assert!(window.try_find("composer-submit").is_none()));
}

#[gpui_kit::test]
fn stale_or_deleted_approval_closure_is_revalidated_before_recording(cx: &mut TestAppContext) {
    let (handle, view, intents) = open_preview(cx, pending_preview());
    let mut cx = visual(handle, cx);
    let composer = view
        .read_with(&cx, |view, _| view.composer.clone())
        .expect("Composer");
    let call = ToolCallKey::new(session("one"), "call-1");

    let mut stale = pending_preview();
    stale.sessions[0].generation = 2;
    let TimelineItem::Tool(tool) = &mut Rc::make_mut(&mut stale.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    tool.approval = ApprovalState::Stale;
    replace(&mut cx, &view, stale);
    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 2, cx);
    });
    assert!(
        intents.borrow().is_empty(),
        "stale closure emitted an effect"
    );

    let mut deleted = pending_preview();
    deleted.sessions[0].generation = 3;
    Rc::make_mut(&mut deleted.sessions[0].items).remove(2);
    replace(&mut cx, &view, deleted);
    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 3, cx);
    });
    assert!(
        intents.borrow().is_empty(),
        "deleted call emitted an effect"
    );

    let mut offline = pending_preview();
    offline.connection = ConnectionState::Offline;
    offline.sessions[0].generation = 4;
    replace(&mut cx, &view, offline);
    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 4, cx);
    });
    assert!(
        intents.borrow().is_empty(),
        "Offline closure emitted an effect"
    );

    let mut current = pending_preview();
    current.sessions[0].generation = 5;
    replace(&mut cx, &view, current);
    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 5, cx);
    });
    assert_eq!(
        intents.borrow().as_slice(),
        &[SessionIntent::Approval {
            call,
            decision: ApprovalDecision::AllowOnce,
            generation: 5,
        }]
    );
}

#[gpui_kit::test]
fn approval_decision_revalidates_generation_call_identity_and_submission(cx: &mut TestAppContext) {
    let (handle, view, intents) = open_preview(cx, pending_preview());
    let mut cx = visual(handle, cx);
    let composer = view
        .read_with(&cx, |view, _| view.composer.clone())
        .expect("Composer");
    let call = ToolCallKey::new(session("one"), "call-1");
    let mut generation_two = pending_preview();
    generation_two.sessions[0].generation = 2;
    replace(&mut cx, &view, generation_two.clone());

    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 1, cx);
    });
    assert!(
        intents.borrow().is_empty(),
        "a gen1 closure must not authorize the same call in gen2"
    );

    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 2, cx);
    });
    assert_eq!(
        intents.borrow().as_slice(),
        &[SessionIntent::Approval {
            call: call.clone(),
            decision: ApprovalDecision::AllowOnce,
            generation: 2,
        }]
    );

    let mut submitting = generation_two.clone();
    let TimelineItem::Tool(tool) = &mut Rc::make_mut(&mut submitting.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    tool.approval = ApprovalState::Submitting;
    replace(&mut cx, &view, submitting);
    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 2, cx);
    });
    assert_eq!(
        intents.borrow().len(),
        1,
        "Submitting must not grow intents from a direct old decision"
    );

    let mut other_call = generation_two;
    let TimelineItem::Tool(tool) = &mut Rc::make_mut(&mut other_call.sessions[0].items)[2] else {
        panic!("tool fixture");
    };
    tool.key = ToolCallKey::new(session("one"), "call-2");
    replace(&mut cx, &view, other_call);
    composer.update(&mut cx, |composer, cx| {
        composer.decide(call.clone(), ApprovalDecision::AllowOnce, 2, cx);
    });
    assert_eq!(
        intents.borrow().len(),
        1,
        "an old key must not decide a different pending call"
    );
}

#[gpui_kit::test]
fn unchanged_markdown_is_not_set_again_when_projection_or_render_repeats(cx: &mut TestAppContext) {
    let (handle, view, _) = open_preview(cx, preview());
    let mut cx = visual(handle, cx);
    let timeline = view
        .read_with(&cx, |view, _| view.timeline.clone())
        .expect("Timeline");
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        0
    );

    replace(&mut cx, &view, preview());
    settle(&mut cx);
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        0,
        "identical content must not be parsed or updated again"
    );

    let mut changed = preview();
    let TimelineItem::Model { markdown, .. } = &mut Rc::make_mut(&mut changed.sessions[0].items)[1]
    else {
        panic!("Markdown fixture");
    };
    *markdown = format!("{markdown}\n\nOnly this message changed.").into();
    replace(&mut cx, &view, changed.clone());
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        1
    );
    replace(&mut cx, &view, changed);
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        1,
        "an unrelated replacement must not update either Markdown state"
    );
}

fn set_markdown(projection: &mut WorkbenchProjection, session_index: usize, text: &str) {
    let TimelineItem::Model { markdown, .. } =
        &mut Rc::make_mut(&mut projection.sessions[session_index].items)[1]
    else {
        panic!("Markdown fixture");
    };
    *markdown = text.into();
}

#[gpui_kit::test]
fn interleaved_sessions_compare_markdown_with_their_own_previous_snapshot(cx: &mut TestAppContext) {
    let mut initial = preview();
    set_markdown(&mut initial, 0, "shared");
    set_markdown(&mut initial, 1, "B old");
    let (handle, view, _) = open_preview(cx, initial.clone());
    let mut cx = visual(handle, cx);
    let timeline = view
        .read_with(&cx, |view, _| view.timeline.clone())
        .expect("Timeline");

    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("two"), window, cx));
    });
    settle(&mut cx);
    assert_eq!(
        timeline.read_with(&cx, |timeline, cx| timeline.markdown_source("model-1", cx)),
        "B old"
    );
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("one"), window, cx));
    });
    settle(&mut cx);

    let mut b_updated = initial.clone();
    set_markdown(&mut b_updated, 1, "shared");
    replace(&mut cx, &view, b_updated.clone());
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        0
    );
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("two"), window, cx));
    });
    settle(&mut cx);
    assert_eq!(
        timeline.read_with(&cx, |timeline, cx| timeline.markdown_source("model-1", cx)),
        "shared",
        "B must update even when A has the same text as B's new projection"
    );
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        1
    );

    let mut a_updated = b_updated.clone();
    set_markdown(&mut a_updated, 0, "A new");
    replace(&mut cx, &view, a_updated.clone());
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("one"), window, cx));
    });
    settle(&mut cx);
    assert_eq!(
        timeline.read_with(&cx, |timeline, cx| timeline.markdown_source("model-1", cx)),
        "A new"
    );
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        2,
        "each changed Session updates exactly once"
    );

    replace(&mut cx, &view, a_updated.clone());
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_session(session("two"), window, cx));
        view.update(cx, |view, cx| view.open_session(session("one"), window, cx));
    });
    settle(&mut cx);
    assert_eq!(
        timeline.read_with(&cx, |timeline, _| timeline.markdown_update_count()),
        2,
        "unchanged content and Session switching must not set text again"
    );
}

#[gpui_kit::test]
fn unavailable_model_disables_send_instead_of_preserving_a_fake_capability(
    cx: &mut TestAppContext,
) {
    let mut projection = preview();
    projection.sessions[0].models.clear();
    projection.sessions[0].initial_model = None;
    let (handle, view, intents) = open_preview(cx, projection);
    let mut cx = visual(handle, cx);
    set_draft(&mut cx, &view, "cannot send", 0..3);
    cx.update(|window, cx| {
        assert_eq!(
            window.find("model-effort").label(),
            Some("Model unavailable")
        );
        window.click("composer-submit", cx);
    });
    settle(&mut cx);
    assert!(intents.borrow().is_empty());
}

#[test]
fn action_availability_requires_a_valid_model() {
    assert_eq!(
        super::composer::primary_action(
            ConnectionState::Online,
            ExecutionState::NotExecuted,
            false,
            false,
            "draft",
        ),
        None
    );
}
