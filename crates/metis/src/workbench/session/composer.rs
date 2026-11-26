use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, Render, RenderOnce,
    Subscription, WeakEntity, Window,
    base::{Disableable as _, TestSupportExt as _},
    component::{
        button::{Button, ButtonVariants as _},
        input::{InputEvent, Textarea, TextareaState},
        label::Label,
        menu::DropdownMenu as _,
    },
    div,
    prelude::*,
};

use super::{
    ApprovalDecision, ApprovalState, ConnectionState, EffectHandler, EffectResult, ExecutionState,
    FocusAnchor, MainView, ModelSpec, QueueItemProjection, Region, SessionIntent, SessionKey,
    SessionPermissionPolicy, SessionProjection, StatsInput, ToolCallKey, ToolCallProjection,
    Workbench, logical_id, session_id,
};
use crate::workbench::stats::{ContextSegmentKind, StatsProjection};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StatsDomain {
    Activity,
    Usage,
    Context,
}

struct QueueItemState {
    id: String,
    input: Entity<TextareaState>,
    _input_subscription: Subscription,
}

struct ComposerSession {
    input: Entity<TextareaState>,
    queue: Vec<QueueItemState>,
    queue_focus: FocusHandle,
    menu_focus: FocusHandle,
    stats_focus: BTreeMap<StatsDomain, FocusHandle>,
    approval_focus: FocusHandle,
    model_id: Option<String>,
    effort: Option<String>,
    next_queue_id: u64,
    open_stats: BTreeSet<StatsDomain>,
    _input_subscription: Subscription,
    _queue_focus_subscription: Subscription,
    _approval_focus_subscription: Subscription,
}

type OpenChangeHandler = Rc<dyn Fn(&bool, &mut Window, &mut App)>;

pub(super) struct Composer {
    session: SessionKey,
    connection: ConnectionState,
    generation: u64,
    execution: ExecutionState,
    approval: Option<ToolCallProjection>,
    permission: SessionPermissionPolicy,
    models: Vec<ModelSpec>,
    stats: StatsInput,
    states: BTreeMap<SessionKey, ComposerSession>,
    feedback: Option<String>,
    parent: WeakEntity<Workbench>,
    effects: EffectHandler,
}

impl Composer {
    pub(super) fn new(
        projection: &SessionProjection,
        connection: ConnectionState,
        parent: WeakEntity<Workbench>,
        effects: EffectHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session: projection.key.clone(),
            connection,
            generation: projection.generation,
            execution: projection.execution,
            approval: projection.pending_approval().cloned(),
            permission: projection.permission,
            models: projection.models.clone(),
            stats: projection.stats.clone(),
            states: BTreeMap::new(),
            feedback: None,
            parent,
            effects,
        };
        this.set_session(projection, connection, window, cx);
        this
    }

    pub(super) fn set_session(
        &mut self,
        projection: &SessionProjection,
        connection: ConnectionState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session = projection.key.clone();
        self.connection = connection;
        self.generation = projection.generation;
        self.execution = projection.execution;
        self.approval = projection.pending_approval().cloned();
        self.permission = projection.permission;
        self.models.clone_from(&projection.models);
        self.stats = projection.stats.clone();
        self.feedback = None;
        self.ensure_state(projection, window, cx);
        cx.notify();
    }

    fn new_queue_item(
        session: &SessionKey,
        item: &QueueItemProjection,
        parent: WeakEntity<Workbench>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> QueueItemState {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 4)
                .placeholder("Queued message")
                .default_value(item.text.clone())
        });
        let changed_input = input.clone();
        let changed_session = session.clone();
        let changed_item_id = item.id.clone();
        let changed_parent = parent.clone();
        let focused_session = session.clone();
        let focused_item_id = item.id.clone();
        let subscription = cx.subscribe(&input, move |_, _, event, cx| match event {
            InputEvent::Change => {
                let input = changed_input.clone();
                let session = changed_session.clone();
                let item_id = changed_item_id.clone();
                let parent = changed_parent.clone();
                cx.defer(move |cx| {
                    let text = input.read(cx).value().to_string();
                    if let Some(parent) = parent.upgrade() {
                        parent.update(cx, |parent, _| {
                            parent.queue_item_changed(&session, &item_id, text);
                        });
                    }
                });
                cx.notify();
            }
            InputEvent::Blur => cx.notify(),
            InputEvent::Focus => {
                let anchor =
                    FocusAnchor::new(MainView::Session(focused_session.clone()), Region::Queue)
                        .with_item(focused_item_id.clone());
                let parent = parent.clone();
                cx.defer(move |cx| {
                    if let Some(parent) = parent.upgrade() {
                        parent.update(cx, |parent, cx| parent.note_focus(anchor, cx));
                    }
                });
            }
            InputEvent::PressEnter { .. } => {}
        });
        QueueItemState {
            id: item.id.clone(),
            input,
            _input_subscription: subscription,
        }
    }

    fn queue_focus_scope(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (FocusHandle, Subscription) {
        let focus = cx.focus_handle();
        let subscription = cx.on_focus_in(&focus, window, |this, window, cx| {
            let Some(state) = this.states.get(&this.session) else {
                return;
            };
            let focus = state.queue_focus.clone();
            if !focus.contains_focused(window, cx) {
                return;
            }
            let item_id = state.queue.iter().find_map(|item| {
                item.input
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
                    .then(|| item.id.clone())
            });
            let mut anchor =
                FocusAnchor::new(MainView::Session(this.session.clone()), Region::Queue);
            if let Some(item_id) = item_id {
                anchor = anchor.with_item(item_id);
            }
            let parent = this.parent.clone();
            window.defer(cx, move |window, cx| {
                if !focus.contains_focused(window, cx) {
                    return;
                }
                if let Some(parent) = parent.upgrade() {
                    parent.update(cx, |parent, cx| parent.note_focus(anchor, cx));
                }
            });
        });
        (focus, subscription)
    }

    fn ensure_state(
        &mut self,
        projection: &SessionProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.states.contains_key(&projection.key) {
            return;
        }
        let key = projection.key.clone();
        let draft = String::new();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder("给 Agent 发消息")
                .default_value(draft)
        });

        let subscription_key = key.clone();
        let input_subscription = cx.subscribe(&input, move |this, _, event, cx| match event {
            InputEvent::Change | InputEvent::Blur => cx.notify(),
            InputEvent::Focus => {
                let anchor = FocusAnchor::new(
                    MainView::Session(subscription_key.clone()),
                    Region::Composer,
                );
                let parent = this.parent.clone();
                cx.defer(move |cx| {
                    if let Some(parent) = parent.upgrade() {
                        parent.update(cx, |parent, cx| parent.note_focus(anchor, cx));
                    }
                });
            }
            InputEvent::PressEnter { .. } => {}
        });
        let approval_focus = cx.focus_handle();
        let approval_focus_subscription =
            cx.on_focus_in(&approval_focus, window, |this, window, cx| {
                let Some(state) = this.states.get(&this.session) else {
                    return;
                };
                let focus = state.approval_focus.clone();
                if !focus.contains_focused(window, cx) {
                    return;
                }
                let Some(call) = this.approval.as_ref() else {
                    return;
                };
                let anchor =
                    FocusAnchor::new(MainView::Session(this.session.clone()), Region::Approval)
                        .with_item(call.key.call_id.clone());
                let parent = this.parent.clone();
                window.defer(cx, move |window, cx| {
                    if !focus.contains_focused(window, cx) {
                        return;
                    }
                    if let Some(parent) = parent.upgrade() {
                        parent.update(cx, |parent, cx| parent.note_focus(anchor, cx));
                    }
                });
            });
        let (queue_focus, queue_focus_subscription) = Self::queue_focus_scope(window, cx);
        let initial_model = projection
            .initial_model
            .clone()
            .or_else(|| projection.models.first().map(|model| model.id.clone()));
        let (model_id, effort) = initial_model.map_or((None, None), |model_id| {
            let effort = projection
                .models
                .iter()
                .find(|model| model.id == model_id)
                .and_then(|model| model.default_effort.clone());
            (Some(model_id), effort)
        });
        let stats_focus = [
            StatsDomain::Activity,
            StatsDomain::Usage,
            StatsDomain::Context,
        ]
        .into_iter()
        .map(|domain| (domain, cx.focus_handle()))
        .collect();
        let queue = projection
            .queue
            .iter()
            .map(|item| Self::new_queue_item(&key, item, self.parent.clone(), window, cx))
            .collect();
        self.states.insert(
            key,
            ComposerSession {
                input,
                queue,
                queue_focus,
                menu_focus: cx.focus_handle(),
                stats_focus,
                approval_focus,
                model_id,
                effort,
                next_queue_id: 0,
                open_stats: BTreeSet::new(),
                _input_subscription: input_subscription,
                _queue_focus_subscription: queue_focus_subscription,
                _approval_focus_subscription: approval_focus_subscription,
            },
        );
    }

    #[cfg(test)]
    pub(super) fn input_state(&self, _: &App) -> Entity<TextareaState> {
        self.states
            .get(&self.session)
            .expect("current Composer state")
            .input
            .clone()
    }

    pub(super) fn menu_focus(&self, _: &App) -> FocusHandle {
        self.states
            .get(&self.session)
            .expect("current Composer state")
            .menu_focus
            .clone()
    }

    pub(super) fn input_focus(&self, cx: &App) -> FocusHandle {
        self.states
            .get(&self.session)
            .expect("current Composer state")
            .input
            .read(cx)
            .focus_handle(cx)
    }

    pub(super) fn queue_focus(&self, item_id: Option<&str>, cx: &App) -> FocusHandle {
        let state = self
            .states
            .get(&self.session)
            .expect("current Composer state");
        let item = item_id
            .and_then(|item_id| state.queue.iter().find(|item| item.id == item_id))
            .or_else(|| item_id.and_then(|_| state.queue.first()));
        item.map_or_else(
            || state.queue_focus.clone(),
            |item| item.input.read(cx).focus_handle(cx),
        )
    }

    pub(super) fn approval_focus(&self, _: &App) -> FocusHandle {
        self.states
            .get(&self.session)
            .expect("current Composer state")
            .approval_focus
            .clone()
    }

    fn queue_snapshot(&self, session: &SessionKey, cx: &App) -> Option<Vec<QueueItemProjection>> {
        self.states.get(session).map(|state| {
            state
                .queue
                .iter()
                .map(|item| QueueItemProjection {
                    id: item.id.clone(),
                    text: item.input.read(cx).value().to_string(),
                })
                .collect()
        })
    }

    // Queue contents are client-local, so injected snapshots must not erase edits.
    #[cfg(test)]
    pub(super) fn apply_local_queues(&self, projection: &mut super::WorkbenchProjection, cx: &App) {
        for session in &mut projection.sessions {
            if let Some(queue) = self.queue_snapshot(&session.key, cx) {
                session.queue = queue;
            }
        }
    }

    fn schedule_queue_sync(
        &self,
        removed_item: Option<String>,
        focus_item: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(queue) = self.queue_snapshot(&self.session, cx) else {
            return;
        };
        let session = self.session.clone();
        let parent = self.parent.clone();
        window.defer(cx, move |window, cx| {
            if let Some(parent) = parent.upgrade() {
                parent.update(cx, |parent, cx| {
                    parent.replace_queue(session, queue, removed_item, focus_item, window, cx);
                });
            }
        });
    }

    fn cancel_queue_item(&mut self, item_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(&self.session) else {
            return;
        };
        let Some(index) = state.queue.iter().position(|item| item.id == item_id) else {
            return;
        };
        state.queue.remove(index);
        let focus_item = state
            .queue
            .get(index)
            .or_else(|| state.queue.last())
            .map(|item| item.id.clone());
        self.schedule_queue_sync(Some(item_id.to_owned()), focus_item, window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn queue_input(&self, item_id: &str, _: &App) -> Option<Entity<TextareaState>> {
        self.states
            .get(&self.session)?
            .queue
            .iter()
            .find_map(|item| (item.id == item_id).then(|| item.input.clone()))
    }

    fn current_selection(&self) -> Option<(String, Option<String>)> {
        let state = self.states.get(&self.session)?;
        let model_id = state.model_id.clone()?;
        let model = self.models.iter().find(|model| model.id == model_id)?;
        let effort = if model.efforts.is_empty() {
            None
        } else {
            let effort = state.effort.as_ref().or(model.default_effort.as_ref())?;
            if !model.efforts.contains(effort) {
                return None;
            }
            Some(effort.clone())
        };
        Some((model_id, effort))
    }

    #[cfg(test)]
    pub(super) fn prune(&mut self, live: &BTreeSet<SessionKey>) {
        self.states.retain(|key, _| live.contains(key));
        self.feedback = None;
    }

    #[cfg(test)]
    pub(super) fn retained_session_count(&self) -> usize {
        self.states.len()
    }

    fn select_model(&mut self, model_id: &str, cx: &mut Context<Self>) {
        let Some(model) = self.models.iter().find(|model| model.id == model_id) else {
            return;
        };
        if let Some(state) = self.states.get_mut(&self.session) {
            state.model_id = Some(model.id.clone());
            state.effort.clone_from(&model.default_effort);
            cx.notify();
        }
    }

    fn select_effort(&mut self, effort: &str, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(&self.session) else {
            return;
        };
        let valid = state
            .model_id
            .as_ref()
            .and_then(|model_id| self.models.iter().find(|model| &model.id == model_id))
            .is_some_and(|model| model.efforts.iter().any(|item| item == effort));
        if valid {
            state.effort = Some(effort.to_owned());
            cx.notify();
        }
    }

    fn toggle_stats(&mut self, domain: StatsDomain, open: bool, cx: &mut Context<Self>) {
        if let Some(state) = self.states.get_mut(&self.session) {
            if open {
                state.open_stats.insert(domain);
            } else {
                state.open_stats.remove(&domain);
            }
            cx.notify();
        }
    }

    pub(super) fn decide(
        &mut self,
        call: ToolCallKey,
        decision: ApprovalDecision,
        expected_generation: u64,
        cx: &mut Context<Self>,
    ) {
        let is_current_pending = expected_generation == self.generation
            && call.session == self.session
            && self.approval.as_ref().is_some_and(|current| {
                current.key == call && matches!(current.approval, ApprovalState::Pending)
            });
        if self.connection != ConnectionState::Online || !is_current_pending {
            self.feedback = Some("Approval is stale or Offline; no decision was sent.".to_owned());
            cx.notify();
            return;
        }
        let result = (self.effects.borrow_mut())(SessionIntent::Approval {
            call,
            decision,
            generation: self.generation,
        });
        self.feedback = Some(match result {
            #[cfg(test)]
            EffectResult::Recorded => {
                "Approval intent recorded locally; Host acceptance and execution remain unknown."
                    .to_owned()
            }
            EffectResult::Unavailable => {
                "No Host connection is attached; the approval was not sent.".to_owned()
            }
        });
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selection = self.current_selection();
        let Some(state) = self.states.get_mut(&self.session) else {
            return;
        };
        let input = state.input.clone();
        let text = input.read(cx).value().to_string();
        let Some(action) = primary_action(
            self.connection,
            self.execution,
            self.approval.is_some(),
            selection.is_some(),
            &text,
        ) else {
            self.feedback = Some(if self.connection == ConnectionState::Offline {
                "Offline: Send and Queue are unavailable; the draft is preserved.".to_owned()
            } else if text.trim().is_empty() {
                "Write a message before sending.".to_owned()
            } else {
                "Select an available Model and a valid Effort before sending.".to_owned()
            });
            cx.notify();
            return;
        };
        let (model, effort) = selection.expect("primary action requires a valid model");
        let (intent, queued_item) = match action {
            ComposerAction::Send => (
                SessionIntent::Send {
                    session: self.session.clone(),
                    text,
                    model,
                    effort,
                },
                None,
            ),
            ComposerAction::Queue => {
                let id = loop {
                    state.next_queue_id += 1;
                    let id = format!(
                        "{}:queue-intent:{}",
                        session_id(&self.session),
                        state.next_queue_id
                    );
                    if !state.queue.iter().any(|item| item.id == id) {
                        break id;
                    }
                };
                let item = QueueItemProjection { id, text };
                (
                    SessionIntent::Queue {
                        item_id: item.id.clone(),
                        session: self.session.clone(),
                        text: item.text.clone(),
                        model,
                        effort,
                    },
                    Some(item),
                )
            }
        };
        let result = (self.effects.borrow_mut())(intent);
        let recorded = match result {
            #[cfg(test)]
            EffectResult::Recorded => true,
            EffectResult::Unavailable => false,
        };
        if recorded && let Some(item) = queued_item {
            input.update(cx, |input, cx| input.set_value("", window, cx));
            let item = Self::new_queue_item(&self.session, &item, self.parent.clone(), window, cx);
            if let Some(state) = self.states.get_mut(&self.session) {
                state.queue.push(item);
            }
            self.schedule_queue_sync(None, None, window, cx);
        }
        self.feedback = Some(if recorded {
            "Intent recorded locally; no Host acceptance or execution is claimed.".to_owned()
        } else {
            "No Host connection is attached; the draft was not sent.".to_owned()
        });
        cx.notify();
    }
}

impl Render for Composer {
    #[allow(clippy::too_many_lines)] // The single native Composer row stays together.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self
            .parent
            .upgrade()
            .expect("composer owner is alive")
            .read(cx)
            .theme
            .resolved()
            .clone();
        let Some(state) = self.states.get(&self.session) else {
            return div();
        };
        let input = state.input.clone();
        let draft = input.read(cx).value().to_string();
        let selection = self.current_selection();
        let action_kind = action_kind(self.execution, self.approval.is_some());
        let action = primary_action(
            self.connection,
            self.execution,
            self.approval.is_some(),
            selection.is_some(),
            &draft,
        );
        let model_id = state.model_id.clone();
        let effort = state.effort.clone();
        let menu_focus = state.menu_focus.clone();
        let queue_focus = state.queue_focus.clone();
        let stats_focus = state.stats_focus.clone();
        let open_stats = state.open_stats.clone();
        let parent = self.parent.upgrade().expect("composer owner is alive");
        let menu_listener = window.listener_for(&parent, |this, open: &bool, window, cx| {
            this.model_menu_changed(*open, window, cx);
        });
        let stats_projection = StatsProjection::project(self.stats.clone());
        let stats_dock = render_stats(
            &stats_projection,
            &self.session,
            &stats_focus,
            &open_stats,
            &cx.weak_entity(),
        );
        let queue = state
            .queue
            .iter()
            .map(|item| (item.id.clone(), item.input.clone()))
            .collect::<Vec<_>>();
        let queue_len = queue.len();
        let feedback = self.feedback.clone();
        let approval_dock = self.approval.clone().map(|call| {
            let call_id = call.key.call_id.clone();
            let focus = state.approval_focus.clone();
            let expected_generation = self.generation;
            let can_approve = self.connection == ConnectionState::Online
                && call.key.session == self.session
                && matches!(call.approval, ApprovalState::Pending);
            let allow_call = call.key.clone();
            let reject_call = call.key.clone();
            let source_call = call.key.clone();
            let heading = if matches!(call.approval, ApprovalState::Submitting) {
                "Approval for this tool call · Submitting"
            } else {
                "Approval for this tool call"
            };
            div()
                .id(format!("approval-dock-{call_id}"))
                .test_support()
                .flex()
                .items_start()
                .gap_2()
                .py_2()
                .border_t_1()
                .border_color(tokens.metis.border_subtle)
                .track_focus(&focus)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_color(tokens.projected.muted_foreground)
                                .child(Label::new(heading)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_start()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_color(tokens.metis.foreground_secondary)
                                        .child(Label::new(format!(
                                            "{}\n{}",
                                            call.title, call.arguments
                                        ))),
                                )
                                .child(
                                    Button::new(format!("approval-source-{call_id}"))
                                        .ghost()
                                        .label("View tool call")
                                        .dropdown_caret(true)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if let Some(parent) = this.parent.upgrade() {
                                                parent.update(cx, |parent, cx| {
                                                    parent.reveal_tool_call(
                                                        &source_call,
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }
                                        })),
                                ),
                        )
                        .children(call.cwd.clone().map(|cwd| {
                            div()
                                .id(format!("approval-cwd-{call_id}"))
                                .test_support()
                                .text_color(tokens.projected.muted_foreground)
                                .child(Label::new(format!("cwd {cwd}")))
                        }))
                        .children(call.reason.clone().map(|reason| {
                            div()
                                .id(format!("approval-reason-{call_id}"))
                                .test_support()
                                .text_color(tokens.metis.foreground_secondary)
                                .child(Label::new(reason))
                        })),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new(format!("approval-reject-{call_id}"))
                                .label("Reject")
                                .disabled(!can_approve)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide(
                                        reject_call.clone(),
                                        ApprovalDecision::Reject,
                                        expected_generation,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new(format!("approval-allow-{call_id}"))
                                .primary()
                                .label("Allow once")
                                .disabled(!can_approve)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide(
                                        allow_call.clone(),
                                        ApprovalDecision::AllowOnce,
                                        expected_generation,
                                        cx,
                                    );
                                })),
                        ),
                )
        });

        div()
            .flex_col()
            .gap_2()
            .py_2()
            .text_color(tokens.projected.foreground)
            .text_size(tokens.metis.metadata_font_size)
            .on_action(cx.listener(|this, action: &SelectModel, _, cx| {
                this.select_model(&action.0, cx);
            }))
            .on_action(cx.listener(|this, action: &SelectEffort, _, cx| {
                this.select_effort(&action.0, cx);
            }))
            .children(approval_dock)
            .children((!queue.is_empty()).then(|| {
                div()
                    .id("session-queue")
                    .test_support()
                    .flex_col()
                    .gap_2()
                    .track_focus(&queue_focus)
                    .child(Label::new(format!("Queue · {queue_len}")))
                    .children(queue.into_iter().map(|(item_id, input)| {
                        let cancel_id = item_id.clone();
                        div()
                            .id(format!("queue-item:{item_id}"))
                            .test_support()
                            .flex()
                            .items_start()
                            .gap_2()
                            .p_1()
                            .rounded_md()
                            .border_1()
                            .border_color(tokens.projected.border)
                            .bg(tokens.metis.code_surface)
                            .child(
                                div()
                                    .id(format!("queue-editor:{item_id}"))
                                    .test_support()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Textarea::new(&input).w_full().bordered(false)),
                            )
                            .child(
                                Button::new(format!("queue-cancel:{item_id}"))
                                    .label("Cancel")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.cancel_queue_item(&cancel_id, window, cx);
                                    })),
                            )
                    }))
            }))
            .child(
                div()
                    .id("composer-card")
                    .test_support()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.projected.border)
                    .bg(tokens.projected.popover)
                    .child(
                        div()
                            .id("composer-editor")
                            .test_support()
                            .child(Textarea::new(&input).w_full().bordered(false)),
                    )
                    .child(
                        div()
                            .id("composer-toolbar")
                            .test_support()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_color(tokens.metis.foreground_secondary).child(
                                Label::new(match self.permission {
                                    SessionPermissionPolicy::AskEachTime => "Ask each time",
                                }),
                            ))
                            .child(div().flex_1())
                            .child(ModelEffortControl {
                                models: self.models.clone(),
                                model_id,
                                effort,
                                focus: menu_focus,
                                on_open_change: Rc::new(move |open, window, cx| {
                                    menu_listener(open, window, cx);
                                }),
                            })
                            .child(
                                Button::new("composer-submit")
                                    .ghost()
                                    .label(match action_kind {
                                        ComposerAction::Send => "Send",
                                        ComposerAction::Queue => "Queue",
                                    })
                                    .disabled(action.is_none())
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            ),
                    ),
            )
            .children(stats_dock)
            .children(feedback.map(|feedback| {
                div()
                    .text_color(tokens.metis.foreground_secondary)
                    .child(feedback)
            }))
    }
}

#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(namespace = metis_workbench, no_json)]
struct SelectModel(String);

#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(namespace = metis_workbench, no_json)]
struct SelectEffort(String);

#[derive(gpui_kit::IntoElement)]
struct ModelEffortControl {
    models: Vec<ModelSpec>,
    model_id: Option<String>,
    effort: Option<String>,
    focus: FocusHandle,
    on_open_change: OpenChangeHandler,
}

impl RenderOnce for ModelEffortControl {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let current = self
            .model_id
            .as_ref()
            .and_then(|id| self.models.iter().find(|model| &model.id == id));
        let model_label = current.map_or("Model unavailable", |model| model.label.as_str());
        let effort_label = current.and_then(|model| {
            if model.efforts.is_empty() {
                None
            } else {
                self.effort.as_deref().or(model.default_effort.as_deref())
            }
        });
        let label = effort_label.map_or_else(
            || model_label.to_owned(),
            |effort| format!("{model_label} + {effort}"),
        );
        let models = self.models.clone();
        let selected_model = self.model_id.clone();
        let selected_effort = self.effort.clone();
        let current_efforts = current.map(|model| model.efforts.clone());
        let action_context = self.focus.clone();
        let on_open_change = self.on_open_change;
        Button::new("model-effort")
            .ghost()
            .label(label)
            .dropdown_caret(true)
            .disabled(current.is_none())
            .track_focus(&self.focus)
            .dropdown_menu(move |menu, window, cx| {
                let models = models.clone();
                let selected_model = selected_model.clone();
                let selected_effort = selected_effort.clone();
                let menu = menu.action_context(action_context.clone()).submenu(
                    "Model",
                    window,
                    cx,
                    move |menu, _, _| {
                        models.iter().fold(menu, |menu, model| {
                            menu.menu_with_check(
                                model.label.clone(),
                                selected_model.as_ref() == Some(&model.id),
                                Box::new(SelectModel(model.id.clone())),
                            )
                        })
                    },
                );
                if let Some(efforts) = current_efforts
                    .clone()
                    .filter(|efforts| !efforts.is_empty())
                {
                    menu.submenu("Effort", window, cx, move |menu, _, _| {
                        efforts.iter().fold(menu, |menu, effort| {
                            menu.menu_with_check(
                                effort.clone(),
                                selected_effort.as_ref() == Some(effort),
                                Box::new(SelectEffort(effort.clone())),
                            )
                        })
                    })
                } else {
                    menu
                }
            })
            .on_open_change(move |open, window, cx| on_open_change(open, window, cx))
    }
}

fn render_stats(
    stats: &StatsProjection,
    session: &SessionKey,
    focus: &BTreeMap<StatsDomain, FocusHandle>,
    open: &BTreeSet<StatsDomain>,
    owner: &WeakEntity<Composer>,
) -> Option<gpui_kit::AnyElement> {
    if !stats.has_visible_domain() {
        return None;
    }
    let mut dock = div().id("composer-stats").flex().justify_center().gap_3();
    if let Some(activity) = &stats.activity {
        let summary = format!(
            "Activity · {} turns · {} steps",
            activity.turns, activity.steps
        );
        let details = format!(
            "LLM {} · Tools {} · TTFT {} · {}",
            activity.llm_duration(),
            activity.tool_duration(),
            activity
                .average_ttft_duration()
                .unwrap_or_else(|| "unavailable".to_owned()),
            activity
                .decode_speed()
                .unwrap_or_else(|| "speed unavailable".to_owned())
        );
        dock = dock.child(stats_disclosure(
            StatsDomain::Activity,
            summary,
            details,
            session,
            focus,
            open,
            owner,
        ));
    }
    if let Some(usage) = &stats.usage {
        let summary = format!("Usage · {} tokens", usage.compact_total);
        let details = format!(
            "Billed input {} · Total {}{}",
            usage.billed_input,
            usage.total,
            usage
                .cache_hit_percent
                .as_ref()
                .map_or(String::new(), |percent| format!(" · Cache hit {percent}"))
        );
        dock = dock.child(stats_disclosure(
            StatsDomain::Usage,
            summary,
            details,
            session,
            focus,
            open,
            owner,
        ));
    }
    if let Some(context) = &stats.context {
        let summary = format!("Context · {}%", context.percent);
        let details = context
            .segments
            .iter()
            .map(|segment| {
                let label = match segment.kind {
                    ContextSegmentKind::System => "System",
                    ContextSegmentKind::Tools => "Tools",
                    ContextSegmentKind::Messages => "Messages",
                    ContextSegmentKind::Unclassified => "Unclassified",
                };
                format!("{label} {:.1}%", segment.width_percent)
            })
            .collect::<Vec<_>>()
            .join(" · ");
        dock = dock.child(stats_disclosure(
            StatsDomain::Context,
            summary,
            if details.is_empty() {
                "No estimated context breakdown.".to_owned()
            } else {
                details
            },
            session,
            focus,
            open,
            owner,
        ));
    }
    Some(dock.into_any_element())
}

fn stats_disclosure(
    domain: StatsDomain,
    summary: String,
    details: String,
    session: &SessionKey,
    focus: &BTreeMap<StatsDomain, FocusHandle>,
    open: &BTreeSet<StatsDomain>,
    owner: &WeakEntity<Composer>,
) -> impl IntoElement {
    let id = logical_id(
        match domain {
            StatsDomain::Activity => "activity",
            StatsDomain::Usage => "usage",
            StatsDomain::Context => "context",
        },
        &[&session_id(session)],
    );
    let owner = owner.clone();
    super::super::disclosure::Disclosure::new(
        id,
        open.contains(&domain),
        focus
            .get(&domain)
            .cloned()
            .expect("stats focus is installed"),
    )
    .summary(summary)
    .body(details)
    .on_toggle(move |open, _, cx| {
        owner
            .update(cx, |composer, cx| composer.toggle_stats(domain, open, cx))
            .expect("composer owner is alive");
    })
}

pub(super) fn action_kind(execution: ExecutionState, approval_pending: bool) -> ComposerAction {
    if approval_pending || execution == ExecutionState::Running {
        ComposerAction::Queue
    } else {
        ComposerAction::Send
    }
}

pub(super) fn primary_action(
    connection: ConnectionState,
    execution: ExecutionState,
    approval_pending: bool,
    model_available: bool,
    draft: &str,
) -> Option<ComposerAction> {
    if connection == ConnectionState::Offline || !model_available || draft.trim().is_empty() {
        return None;
    }
    Some(action_kind(execution, approval_pending))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComposerAction {
    Send,
    Queue,
}
