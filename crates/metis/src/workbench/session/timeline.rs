use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::{Arc, Mutex},
};

use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Render, RenderOnce, Subscription,
    WeakEntity, Window,
    base::Disableable as _,
    component::{
        button::Button,
        label::Label,
        message_scroller::{MessageScroller, MessageScrollerState},
        text::TextViewState,
    },
    div,
    prelude::*,
    px,
};

use super::{
    ApprovalDecision, ApprovalState, ConnectionState, EffectHandler, EffectResult, FocusAnchor,
    MainView, Region, ResolvedTokens, SessionIntent, SessionKey, SessionProjection, TimelineItem,
    ToolCallKey, ToolCallProjection, Workbench, session_id,
};
use crate::workbench::markdown::{AgentMarkdown, LinkConfirmation};

struct TimelineSession {
    items: Rc<Vec<TimelineItem>>,
    scroller: Entity<MessageScrollerState>,
    markdown: BTreeMap<String, Entity<TextViewState>>,
    item_ids: Vec<String>,
    tool_focus: BTreeMap<String, FocusHandle>,
    approval_focus: BTreeMap<String, FocusHandle>,
    open_tools: BTreeSet<String>,
    _scroll_subscription: Subscription,
}

pub(super) struct SessionTimeline {
    session: SessionKey,
    connection: ConnectionState,
    generation: u64,
    items: Rc<Vec<TimelineItem>>,
    states: BTreeMap<SessionKey, TimelineSession>,
    focus: FocusHandle,
    pending_link: Arc<Mutex<Option<String>>>,
    notice: Option<String>,
    parent: WeakEntity<Workbench>,
    effects: EffectHandler,
    #[cfg(test)]
    markdown_set_text_calls: usize,
}

impl SessionTimeline {
    pub(super) fn new(
        projection: &SessionProjection,
        connection: ConnectionState,
        parent: WeakEntity<Workbench>,
        effects: EffectHandler,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session: projection.key.clone(),
            connection,
            generation: projection.generation,
            items: Rc::new(Vec::new()),
            states: BTreeMap::new(),
            focus: cx.focus_handle(),
            pending_link: Arc::new(Mutex::new(None)),
            notice: None,
            parent,
            effects,
            #[cfg(test)]
            markdown_set_text_calls: 0,
        };
        this.set_session(projection, connection, cx);
        this
    }

    pub(super) fn set_session(
        &mut self,
        projection: &SessionProjection,
        connection: ConnectionState,
        cx: &mut Context<Self>,
    ) {
        self.session = projection.key.clone();
        self.connection = connection;
        self.generation = projection.generation;
        self.items = Rc::clone(&projection.items);
        self.pending_link
            .lock()
            .expect("lock pending Markdown link")
            .take();
        self.ensure_state(projection, cx);
        let Some(state) = self.states.get_mut(&self.session) else {
            return;
        };
        let previous_items = std::mem::replace(&mut state.items, Rc::clone(&projection.items));
        let previous_markdown = previous_items
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Model { id, markdown } => Some((id.as_str(), markdown.as_str())),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let item_ids = self
            .items
            .iter()
            .map(TimelineItem::stable_id)
            .collect::<Vec<_>>();
        let old_count = state.scroller.read(cx).item_count();
        state.item_ids = item_ids;
        for item in self.items.iter() {
            match item {
                TimelineItem::Model { id, markdown } => {
                    let key = format!("model:{id}");
                    if let Some(text) = state.markdown.get(&key) {
                        if previous_markdown.get(id.as_str()).copied() != Some(markdown.as_str()) {
                            #[cfg(test)]
                            {
                                self.markdown_set_text_calls += 1;
                            }
                            text.update(cx, |text, cx| text.set_text(markdown, cx));
                        }
                    } else {
                        let text = cx.new(|cx| TextViewState::markdown(markdown, cx));
                        state.markdown.insert(key, text);
                    }
                }
                TimelineItem::Tool(call) => {
                    state
                        .tool_focus
                        .entry(call.key.call_id.clone())
                        .or_insert_with(|| cx.focus_handle());
                    state
                        .approval_focus
                        .entry(call.key.call_id.clone())
                        .or_insert_with(|| cx.focus_handle());
                }
                TimelineItem::User { .. } => {}
            }
        }
        if old_count == self.items.len() {
            for index in previous_items
                .iter()
                .zip(self.items.iter())
                .enumerate()
                .filter(|(_, (previous, current))| previous != current)
                .map(|(index, _)| index)
            {
                state.scroller.update(cx, |scroller, cx| {
                    scroller.remeasure_items(index..index + 1, cx);
                });
            }
        } else {
            let new_count = self.items.len();
            state.scroller.update(cx, |scroller, cx| {
                scroller.splice(0..old_count, new_count, cx);
            });
        }
        cx.notify();
    }

    fn ensure_state(&mut self, projection: &SessionProjection, cx: &mut Context<Self>) {
        if self.states.contains_key(&projection.key) {
            return;
        }
        let key = projection.key.clone();
        let scroller = cx.new(|cx| MessageScrollerState::new(projection.items.len(), cx));
        let scroll_subscription = cx.observe(&scroller, |_, _, cx| cx.notify());
        self.states.insert(
            key,
            TimelineSession {
                items: Rc::new(Vec::new()),
                scroller,
                markdown: BTreeMap::new(),
                item_ids: Vec::new(),
                tool_focus: BTreeMap::new(),
                approval_focus: BTreeMap::new(),
                open_tools: BTreeSet::new(),
                _scroll_subscription: scroll_subscription,
            },
        );
    }

    #[cfg(test)]
    pub(super) fn scroller_state(&self, _: &App) -> Entity<MessageScrollerState> {
        self.states
            .get(&self.session)
            .expect("current Timeline state")
            .scroller
            .clone()
    }

    #[cfg(test)]
    pub(super) fn prune(&mut self, live: &BTreeSet<SessionKey>) {
        self.states.retain(|key, _| live.contains(key));
        self.notice = None;
        self.pending_link
            .lock()
            .expect("lock pending Markdown link")
            .take();
    }

    #[cfg(test)]
    pub(super) fn retained_session_count(&self) -> usize {
        self.states.len()
    }

    #[cfg(test)]
    pub(super) fn markdown_update_count(&self) -> usize {
        self.markdown_set_text_calls
    }

    #[cfg(test)]
    pub(super) fn markdown_source(&self, id: &str, cx: &App) -> String {
        self.states
            .get(&self.session)
            .and_then(|state| state.markdown.get(&format!("model:{id}")))
            .expect("current Markdown state")
            .read(cx)
            .rendered_text()
            .source()
            .to_string()
    }

    pub(super) fn remeasure(&mut self, cx: &mut Context<Self>) {
        for state in self.states.values() {
            state.scroller.update(cx, MessageScrollerState::remeasure);
        }
    }

    pub(super) fn timeline_focus(&self, item_id: Option<&str>, _: &App) -> Option<FocusHandle> {
        let state = self.states.get(&self.session)?;
        item_id
            .and_then(|id| {
                id.strip_prefix("tool:")
                    .or(Some(id))
                    .and_then(|id| state.tool_focus.get(id))
            })
            .cloned()
            .or_else(|| Some(self.focus.clone()))
    }

    pub(super) fn approval_focus(&self, item_id: Option<&str>, _: &App) -> Option<FocusHandle> {
        let state = self.states.get(&self.session)?;
        item_id
            .and_then(|id| state.approval_focus.get(id))
            .cloned()
            .or_else(|| Some(self.focus.clone()))
    }

    fn jump_to_latest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.states.get(&self.session) else {
            return;
        };
        let offset = state.scroller.read(cx).logical_scroll_top();
        let item_id = state.item_ids.get(offset.item_ix).cloned();
        let tool_id = item_id
            .as_deref()
            .and_then(|id| id.strip_prefix("tool:"))
            .map(str::to_owned);
        let focus = tool_id
            .as_ref()
            .and_then(|id| state.tool_focus.get(id))
            .cloned()
            .unwrap_or_else(|| self.focus.clone());
        state.scroller.update(cx, |scroller, cx| {
            scroller.scroll_to_end(cx);
        });
        // The jump button may disappear after this update; move focus before
        // notifying the view so it never remains on a hidden control.
        focus.focus(window, cx);
        let mut anchor =
            FocusAnchor::new(MainView::Session(self.session.clone()), Region::Timeline);
        if let Some(tool_id) = tool_id {
            anchor = anchor.with_item(tool_id);
        }
        if let Some(parent) = self.parent.upgrade() {
            parent.update(cx, |parent, cx| parent.note_focus(anchor, cx));
        }
        cx.notify();
    }

    fn toggle_tool(&mut self, call_id: &str, open: bool, cx: &mut Context<Self>) {
        let anchor = FocusAnchor::new(MainView::Session(self.session.clone()), Region::Timeline)
            .with_item(call_id.to_owned());
        if let Some(parent) = self.parent.upgrade() {
            parent.update(cx, |parent, cx| parent.note_focus(anchor, cx));
        }
        if let Some(state) = self.states.get_mut(&self.session) {
            if open {
                state.open_tools.insert(call_id.to_owned());
            } else {
                state.open_tools.remove(call_id);
            }
            cx.notify();
        }
    }

    pub(super) fn decide(
        &mut self,
        call: ToolCallKey,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let is_current_pending = call.session == self.session
            && self.items.iter().any(|item| match item {
                TimelineItem::Tool(current) => {
                    current.key == call && matches!(current.approval, ApprovalState::Pending)
                }
                _ => false,
            });
        if self.connection != ConnectionState::Online || !is_current_pending {
            self.notice = Some("Approval is stale or Offline; no decision was sent.".to_owned());
            cx.notify();
            return;
        }
        let result = (self.effects.borrow_mut())(SessionIntent::Approval {
            call,
            decision,
            generation: self.generation,
        });
        self.notice = Some(match result {
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
}

impl Render for SessionTimeline {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self
            .parent
            .upgrade()
            .expect("timeline owner is alive")
            .read(cx)
            .theme
            .resolved()
            .clone();
        let Some(state) = self.states.get(&self.session) else {
            return div();
        };
        let scroller = state.scroller.clone();
        let notice_color = tokens.metis.foreground_secondary;
        let items = Rc::clone(&self.items);
        let markdown = state.markdown.clone();
        let tool_focus = state.tool_focus.clone();
        let approval_focus = state.approval_focus.clone();
        let open_tools = state.open_tools.clone();
        let entity = cx.entity().clone();
        let pending_link = Arc::clone(&self.pending_link);
        let show_jump = scroller.read(cx).is_scrolled_up();
        let timeline = MessageScroller::new(
            format!("session-timeline:{}", session_id(&self.session)),
            scroller,
            move |index, window, cx| {
                let item = items[index].clone();
                render_item(
                    item,
                    &entity,
                    &markdown,
                    &tool_focus,
                    &approval_focus,
                    &open_tools,
                    &pending_link,
                    &tokens,
                    window,
                    cx,
                )
            },
        )
        .jump_button(false)
        .size_full();
        let pending = self
            .pending_link
            .lock()
            .expect("lock pending Markdown link")
            .clone();
        let close_link = Arc::clone(&self.pending_link);
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .child(timeline)
            .when(show_jump, |this| {
                this.child(
                    div().absolute().bottom(px(12.)).right(px(12.)).child(
                        Button::new("jump-to-latest")
                            .label("Jump to latest")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.jump_to_latest(window, cx);
                            })),
                    ),
                )
            })
            .children(
                self.notice
                    .as_ref()
                    .map(|notice| div().text_color(notice_color).child(notice.clone())),
            )
            .children(pending.map(|target| {
                LinkConfirmation::new(target).on_close(move |window, _| {
                    close_link
                        .lock()
                        .expect("lock pending Markdown link")
                        .take();
                    window.refresh();
                })
            }))
    }
}

#[allow(clippy::too_many_arguments)]
fn render_item(
    item: TimelineItem,
    entity: &Entity<SessionTimeline>,
    markdown: &BTreeMap<String, Entity<TextViewState>>,
    tool_focus: &BTreeMap<String, FocusHandle>,
    approval_focus: &BTreeMap<String, FocusHandle>,
    open_tools: &BTreeSet<String>,
    pending_link: &Arc<Mutex<Option<String>>>,
    tokens: &ResolvedTokens,
    _window: &mut Window,
    cx: &mut App,
) -> gpui_kit::AnyElement {
    match item {
        TimelineItem::User { text, .. } => div()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_color(tokens.metis.foreground_secondary)
                    .text_size(tokens.metis.metadata_font_size)
                    .child("You"),
            )
            .child(text)
            .into_any_element(),
        TimelineItem::Model { id, .. } => {
            let key = format!("model:{id}");
            let Some(state) = markdown.get(&key) else {
                return div().into_any_element();
            };
            let pending_link = Arc::clone(pending_link);
            AgentMarkdown::new(state)
                .on_confirmation(move |target, window, _| {
                    *pending_link.lock().expect("lock pending Markdown link") = Some(target);
                    window.refresh();
                })
                .into_any_element()
        }
        TimelineItem::Tool(call) => {
            let call_id = call.key.call_id.clone();
            let open = open_tools.contains(&call_id);
            let focus = tool_focus
                .get(&call_id)
                .cloned()
                .expect("ToolCall focus is installed with its projection");
            let approval_focus = approval_focus
                .get(&call_id)
                .cloned()
                .expect("approval focus is installed with its projection");
            let toggle_entity = entity.clone();
            let toggle_id = call_id.clone();
            let on_toggle = move |open: bool, _: &mut Window, cx: &mut App| {
                toggle_entity.update(cx, |timeline, cx| {
                    timeline.toggle_tool(&toggle_id, open, cx);
                });
            };
            let decide_entity = entity.clone();
            let decide_call = call.key.clone();
            let on_decide = move |decision: ApprovalDecision, _: &mut Window, cx: &mut App| {
                decide_entity.update(cx, |timeline, cx| {
                    timeline.decide(decide_call.clone(), decision, cx);
                });
            };
            let can_approve = entity.read(cx).connection == ConnectionState::Online
                && call.key.session == entity.read(cx).session
                && matches!(call.approval, ApprovalState::Pending);
            ToolCallView {
                call,
                open,
                focus,
                approval_focus,
                can_approve,
                tokens: tokens.clone(),
                on_toggle: Rc::new(on_toggle),
                on_decide: Rc::new(on_decide),
            }
            .into_any_element()
        }
    }
}

type ToolToggleHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;
type ApprovalHandler = Rc<dyn Fn(ApprovalDecision, &mut Window, &mut App)>;

#[derive(gpui_kit::IntoElement)]
struct ToolCallView {
    call: ToolCallProjection,
    open: bool,
    focus: FocusHandle,
    approval_focus: FocusHandle,
    can_approve: bool,
    tokens: ResolvedTokens,
    on_toggle: ToolToggleHandler,
    on_decide: ApprovalHandler,
}

impl RenderOnce for ToolCallView {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let call = self.call;
        let call_id = call.key.call_id.clone();
        let approval_label = match call.approval {
            ApprovalState::NotRequested => None,
            ApprovalState::Pending => Some("Approval pending"),
            ApprovalState::Submitting => Some("Approval submitting"),
            ApprovalState::Decided(ApprovalDecision::AllowOnce) => Some("Allowed once"),
            ApprovalState::Decided(ApprovalDecision::Reject) => Some("Rejected"),
            ApprovalState::Stale => Some("Approval stale"),
        };
        let summary = format!(
            "{} · {}{}",
            call.title,
            call.execution.label(),
            approval_label.map_or(String::new(), |label| format!(" · {label}"))
        );
        let body = div()
            .flex_col()
            .gap_2()
            .p_2()
            .bg(self.tokens.metis.code_surface)
            .child(format!("Arguments\n{}", call.arguments))
            .child(Label::new(
                call.output
                    .map_or("No output reported.".to_owned(), |output| {
                        format!("Output\n{output}")
                    }),
            ));
        let disclosure = super::super::disclosure::Disclosure::new(
            format!("tool-{call_id}"),
            self.open,
            self.focus,
        )
        .summary(summary)
        .body(body)
        .on_toggle(move |open, window, cx| (self.on_toggle)(open, window, cx));
        let on_allow = self.on_decide.clone();
        let on_reject = self.on_decide;
        div()
            .id(format!("tool-call-{call_id}"))
            .flex_col()
            .border_1()
            .border_color(self.tokens.projected.border)
            .child(disclosure)
            .when(
                matches!(
                    call.approval,
                    ApprovalState::Pending | ApprovalState::Submitting
                ),
                |this| {
                    this.child(
                        div()
                            .flex()
                            .gap_2()
                            .p_2()
                            .track_focus(&self.approval_focus)
                            .child(
                                Button::new(format!("approval-allow-{call_id}"))
                                    .label("Allow once")
                                    .disabled(!self.can_approve)
                                    .on_click(move |_, window, cx| {
                                        on_allow(ApprovalDecision::AllowOnce, window, cx);
                                    }),
                            )
                            .child(
                                Button::new(format!("approval-reject-{call_id}"))
                                    .label("Reject")
                                    .disabled(!self.can_approve)
                                    .on_click(move |_, window, cx| {
                                        on_reject(ApprovalDecision::Reject, window, cx);
                                    }),
                            ),
                    )
                },
            )
            .into_any_element()
    }
}
