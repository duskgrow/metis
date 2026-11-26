//! Minimal native Session slice with injected projections and honest effects.

mod composer;
mod host_bridge;
mod host_projection;
mod navigation;
mod timeline;

#[cfg(test)]
use std::cell::Cell;
use std::{cell::RefCell, fmt::Write as _, rc::Rc};

use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Render, SharedString, Subscription, Task,
    Window,
    base::TestSupportExt as _,
    component::{ThemeMode, button::Button, tree::TreeState},
    div,
    prelude::*,
    px,
};

use super::{
    focus::{FocusIntent, FocusReducer, FocusScope, FocusTarget, FocusTopology},
    history::{
        ApprovalDecision, FocusAnchor, MainView, NavigationSnapshot, NavigationState, Region,
    },
    identity::{ProjectKey, SessionKey, SubagentKey, ToolCallKey, WorktreeKey},
    stats::StatsInput,
    theme::{ResolvedTokens, ThemeController},
};
use composer::Composer;
use host_bridge::HostSessionBridge;
use metis::{
    runtime::DshSupervisor,
    session_transport::{LocalSession, LocalSessionConfig, LocalSessionError, SessionTransport},
};
use navigation::SessionNavigation;
use timeline::SessionTimeline;

const SIDEBAR_WIDTH: f32 = 268.;
const SESSION_HEADER_HEIGHT: f32 = 86.;
const SESSION_CONTENT_WIDTH: f32 = 800.;
const SESSION_HEADER_WIDTH: f32 = 820.;
const COMPOSER_REGION_HEIGHT: f32 = 192.;
const COMPOSER_APPROVAL_HEIGHT: f32 = 320.;
const COMPOSER_QUEUE_HEIGHT: f32 = 384.;
const STATUS_NOTICE_BYTES: usize = 4 * 1024;

#[cfg(test)]
thread_local! {
    static COLD_START_RESULTS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn cold_start_result_count() -> usize {
    COLD_START_RESULTS.with(Cell::get)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ConnectionState {
    Online,
    #[default]
    Offline,
}

/// Values are supplied by Host or deterministic test projections rather than
/// constructed optimistically by the renderer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ExecutionState {
    #[default]
    NotExecuted,
    Running,
    #[allow(dead_code)] // Host projection state; not synthesized by this slice.
    Succeeded,
    #[allow(dead_code)] // Host projection state; not synthesized by this slice.
    Failed,
    #[allow(dead_code)] // Host projection state; not synthesized by this slice.
    Stopped,
    #[allow(dead_code)] // Host projection state; not synthesized by this slice.
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ApprovalState {
    #[default]
    NotRequested,
    #[allow(dead_code)] // Supplied by Host projections.
    Pending,
    #[allow(dead_code)] // Supplied by Host projections.
    Submitting,
    #[allow(dead_code)] // Supplied by Host projections.
    Decided(ApprovalDecision),
    #[allow(dead_code)] // Supplied by Host projections.
    Stale,
}

pub(super) type SharedText = Rc<str>;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ToolCallProjection {
    pub(super) key: ToolCallKey,
    pub(super) title: SharedText,
    pub(super) arguments: SharedText,
    pub(super) cwd: Option<SharedText>,
    pub(super) reason: Option<SharedText>,
    pub(super) execution: ExecutionState,
    pub(super) approval: ApprovalState,
    pub(super) output: Option<SharedText>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum TimelineItem {
    #[allow(dead_code)] // Supplied by Host projections.
    User { id: String, text: SharedText },
    #[allow(dead_code)] // Supplied by Host projections.
    Model { id: String, markdown: SharedText },
    #[allow(dead_code)] // Supplied by Host projections.
    Tool(ToolCallProjection),
}

impl TimelineItem {
    pub(super) fn stable_id(&self) -> String {
        match self {
            Self::User { id, .. } => format!("user:{id}"),
            Self::Model { id, .. } => format!("model:{id}"),
            Self::Tool(call) => format!("tool:{}", call.key.call_id),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct QueueItemProjection {
    pub(super) id: String,
    pub(super) text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SubagentProjection {
    pub(super) key: SubagentKey,
    pub(super) title: String,
    pub(super) continuable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ModelSpec {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) default_effort: Option<String>,
    pub(super) efforts: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SessionPermissionPolicy {
    #[default]
    AskEachTime,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SessionProjection {
    pub(super) key: SessionKey,
    pub(super) title: String,
    pub(super) generation: u64,
    pub(super) execution: ExecutionState,
    pub(super) items: Rc<Vec<TimelineItem>>,
    pub(super) queue: Vec<QueueItemProjection>,
    pub(super) permission: SessionPermissionPolicy,
    pub(super) models: Vec<ModelSpec>,
    pub(super) initial_model: Option<String>,
    pub(super) stats: StatsInput,
    pub(super) subagents: Vec<SubagentProjection>,
}

impl SessionProjection {
    pub(super) fn pending_approval(&self) -> Option<&ToolCallProjection> {
        self.items.iter().find_map(|item| match item {
            TimelineItem::Tool(call)
                if matches!(
                    call.approval,
                    ApprovalState::Pending | ApprovalState::Submitting
                ) =>
            {
                Some(call)
            }
            _ => None,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct WorkbenchProjection {
    pub(super) connection: ConnectionState,
    pub(super) sessions: Vec<SessionProjection>,
}

impl WorkbenchProjection {
    fn session(&self, key: &SessionKey) -> Option<&SessionProjection> {
        self.sessions.iter().find(|session| &session.key == key)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SessionIntent {
    Send {
        session: SessionKey,
        text: String,
        model: String,
        effort: Option<String>,
    },
    Queue {
        session: SessionKey,
        item_id: String,
        text: String,
        model: String,
        effort: Option<String>,
    },
    Approval {
        call: ToolCallKey,
        decision: ApprovalDecision,
        generation: u64,
    },
}

/// The injected effect handler decides whether an intent was merely recorded
/// locally or could not be delivered; neither variant is Host acceptance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EffectResult {
    Recorded,
    Unavailable,
}

pub(super) type EffectHandler = Rc<RefCell<dyn FnMut(SessionIntent) -> EffectResult>>;

/// Owns only local navigation and focus. It never stores or rolls back Host
/// facts; the current projection is supplied separately to each native view.
pub(super) struct MainViewController {
    navigation: NavigationState,
    snapshot: NavigationSnapshot,
    focus: FocusReducer,
    pending_focus: Option<FocusIntent>,
}

impl MainViewController {
    fn new(session: &SessionProjection, snapshot: NavigationSnapshot) -> Self {
        let mut navigation = NavigationState::new(session.key.clone());
        let topology = Self::topology_for(navigation.current_view(), Some(session));
        let mut focus = FocusReducer::new(topology.clone());
        let initial_anchor = session.pending_approval().map_or_else(
            || navigation.focus().clone(),
            |call| {
                FocusAnchor::new(MainView::Session(session.key.clone()), Region::Approval)
                    .with_item(call.key.call_id.clone())
            },
        );
        let pending_focus = Some(focus.restore(topology, initial_anchor.clone()));
        navigation.set_focus_anchor(initial_anchor);
        Self {
            navigation,
            snapshot,
            focus,
            pending_focus,
        }
    }

    fn topology_for(view: &MainView, session: Option<&SessionProjection>) -> FocusTopology {
        let approval = session.and_then(|session| {
            session
                .pending_approval()
                .map(|call| call.key.call_id.clone())
        });
        let has_queue = session.is_some_and(|session| !session.queue.is_empty());
        FocusTopology::new(view.clone(), approval, has_queue)
    }

    fn current_session(&self) -> Option<&SessionKey> {
        match self.navigation.current_view() {
            MainView::Session(session) => Some(session),
            MainView::Diff { .. } | MainView::Subagent { .. } => None,
        }
    }

    fn sync_topology(&mut self, session: Option<&SessionProjection>) {
        let topology = Self::topology_for(self.navigation.current_view(), session);
        debug_assert_eq!(topology.view(), self.navigation.current_view());
        debug_assert_ne!(topology.regions(), &[]);
        let intent = self.focus.rebuild(topology);
        if intent != FocusIntent::Noop {
            self.pending_focus = Some(intent);
        }
    }

    fn replace_projection(
        &mut self,
        projection: &WorkbenchProjection,
        snapshot: NavigationSnapshot,
    ) {
        self.snapshot = snapshot;
        let current = self.navigation.current_view().clone();
        self.navigation.restore(current, &self.snapshot);
        self.navigation.prune(&self.snapshot);
        let session = self
            .current_session()
            .and_then(|key| projection.session(key));
        let topology = Self::topology_for(self.navigation.current_view(), session);
        let intent = if self.focus.current().scope == FocusScope::Shell {
            self.focus
                .restore(topology, self.navigation.focus().clone())
        } else {
            self.focus.rebuild(topology)
        };
        if intent != FocusIntent::Noop {
            self.pending_focus = Some(intent);
        }
    }

    fn open_session(&mut self, session: SessionKey, projection: &SessionProjection) -> bool {
        if !self.navigation.open_session(session, &self.snapshot) {
            return false;
        }
        let topology = Self::topology_for(self.navigation.current_view(), Some(projection));
        self.pending_focus = Some(
            self.focus
                .restore(topology, self.navigation.focus().clone()),
        );
        true
    }

    fn return_to_origin(&mut self, projection: &WorkbenchProjection) -> bool {
        if !self.navigation.return_to_origin(&self.snapshot) {
            return false;
        }
        let session = self
            .current_session()
            .and_then(|key| projection.session(key));
        let topology = Self::topology_for(self.navigation.current_view(), session);
        self.pending_focus = Some(
            self.focus
                .restore(topology, self.navigation.focus().clone()),
        );
        true
    }

    fn note_focus(&mut self, anchor: FocusAnchor, session: Option<&SessionProjection>) {
        if !self.navigation.set_focus_anchor(anchor.clone()) {
            return;
        }
        let topology = Self::topology_for(self.navigation.current_view(), session);
        // The native control already has focus; only synchronize the logical
        // reducer and deliberately discard its resolution intent.
        let _ = self.focus.restore(topology, anchor);
    }

    fn model_menu_changed(&mut self, open: bool, session: Option<&SessionProjection>) {
        let intent = if open {
            let trigger =
                FocusAnchor::new(self.navigation.current_view().clone(), Region::Composer)
                    .with_item("model-effort");
            self.note_focus(trigger, session);
            self.focus
                .open_overlay("model-effort", Some("model-effort".to_owned()))
        } else {
            self.focus.escape()
        };
        if intent != FocusIntent::Noop {
            self.pending_focus = Some(intent);
        }
        self.sync_topology(session);
    }

    fn host_filter_changed(&mut self, open: bool, session: Option<&SessionProjection>) {
        let intent = if open {
            let trigger =
                FocusAnchor::new(self.navigation.current_view().clone(), Region::Navigation)
                    .with_item("host-filter");
            self.note_focus(trigger, session);
            self.focus
                .open_overlay("host-filter", Some("host-filter".to_owned()))
        } else {
            self.focus.escape()
        };
        if intent != FocusIntent::Noop {
            self.pending_focus = Some(intent);
        }
        self.sync_topology(session);
    }

    fn restore_current_focus(&mut self) {
        self.pending_focus = Some(FocusIntent::Focus(self.focus.current()));
    }

    fn take_focus_intent(&mut self) -> Option<FocusIntent> {
        self.pending_focus.take()
    }
}

enum ResolvedFocus {
    Handle(FocusHandle),
    Tree(Entity<TreeState>),
}

struct ActiveHost {
    local: LocalSession,
    transport: Option<SessionTransport>,
    bridge: Rc<RefCell<HostSessionBridge>>,
    pump: Option<Task<()>>,
}

impl Drop for ActiveHost {
    fn drop(&mut self) {
        // Stop waiting before closing the transport; its close signal also
        // releases a pump that is already inside a background receive.
        self.pump.take();
        if let Some(transport) = &mut self.transport {
            transport.close_and_join();
        }
        if let Err(error) = self.local.shutdown() {
            tracing::warn!(%error, "failed to stop the local Workbench Host");
        }
    }
}

pub(crate) struct Workbench {
    theme: ThemeController,
    projection: WorkbenchProjection,
    controller: Option<MainViewController>,
    navigation: Entity<SessionNavigation>,
    timeline: Option<Entity<SessionTimeline>>,
    composer: Option<Entity<Composer>>,
    header_focus: FocusHandle,
    effects: EffectHandler,
    host: Option<ActiveHost>,
    status_notice: Option<SharedString>,
    quit_subscription: Option<Subscription>,
}

impl Workbench {
    pub(crate) fn cold_start(
        config: LocalSessionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let effects: EffectHandler = Rc::new(RefCell::new(|_| EffectResult::Unavailable));
        let mut this = Self::new(WorkbenchProjection::default(), &effects, window, cx);
        this.status_notice = Some("Starting the local Host; no Session is available yet.".into());
        this.quit_subscription = Some(cx.on_app_quit(|this, _| {
            this.host.take();
            async {}
        }));
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(
                    async move { LocalSession::start_ephemeral(&DshSupervisor::system(), &config) },
                )
                .await;
            #[cfg(test)]
            COLD_START_RESULTS.with(|count| count.set(count.get() + 1));
            let _ = this.update_in(cx, |this, window, cx| {
                this.attach_local(prepared, window, cx);
            });
        })
        .detach();
        this
    }

    pub(super) fn new(
        projection: WorkbenchProjection,
        effects: &EffectHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = ThemeController::new(ThemeMode::Light, cx);
        let parent = cx.weak_entity();
        let first = projection.sessions.first();
        let controller = first.map(|session| {
            MainViewController::new(
                session,
                navigation_snapshot(&projection, session.key.clone()),
            )
        });
        let navigation = cx.new(|cx| {
            SessionNavigation::new(
                &projection,
                first.map(|session| &session.key),
                parent,
                window,
                cx,
            )
        });
        let parent = cx.weak_entity();
        let timeline = first.map(|session| cx.new(|cx| SessionTimeline::new(session, parent, cx)));
        let parent = cx.weak_entity();
        let composer = first.map(|session| {
            cx.new(|cx| {
                Composer::new(
                    session,
                    projection.connection,
                    parent,
                    Rc::clone(effects),
                    window,
                    cx,
                )
            })
        });
        Self {
            theme,
            projection,
            controller,
            navigation,
            timeline,
            composer,
            header_focus: cx.focus_handle(),
            effects: Rc::clone(effects),
            host: None,
            status_notice: None,
            quit_subscription: None,
        }
    }

    fn attach_local(
        &mut self,
        prepared: Result<LocalSession, LocalSessionError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = match prepared {
            Ok(local) => local,
            Err(error) => {
                self.status_notice = Some(bounded_status(format!(
                    "Local Host startup failed: {error}"
                )));
                cx.notify();
                return;
            }
        };
        let project = local.project();
        let session = SessionKey::new(
            WorktreeKey::new(
                ProjectKey::new("local", project.project_id.clone()),
                project.path.clone(),
            ),
            local.session_id(),
        );
        let mut transport = local.spawn_transport();
        let events = transport.take_events().expect("new transport events");
        let wake = transport.take_wake().expect("new transport wake");
        let bridge = Rc::new(RefCell::new(HostSessionBridge::new(
            session,
            transport.commands(),
            events,
        )));
        self.effects = HostSessionBridge::effect_handler(&bridge);
        let pump = cx.spawn_in(window, async move |this, cx| {
            let mut wake = wake;
            loop {
                #[cfg(not(test))]
                let awake = wake.recv().await.is_some();
                #[cfg(test)]
                let awake = {
                    // GPUI's deterministic scheduler rejects cross-thread
                    // wakes. Poll the same channel without registering one.
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(5))
                        .await;
                    !matches!(
                        wake.try_recv(),
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
                    )
                };
                if !awake {
                    break;
                }
                if this.update_in(cx, Workbench::drain_host).is_err() {
                    break;
                }
            }
        });
        self.host = Some(ActiveHost {
            local,
            transport: Some(transport),
            bridge,
            pump: Some(pump),
        });
        self.status_notice = Some("Connecting to the local Host…".into());
        self.drain_host(window, cx);
    }

    fn drain_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = &self.host else {
            return;
        };
        let (projection, notices) = {
            let mut bridge = host.bridge.borrow_mut();
            (bridge.poll(), bridge.take_notices())
        };
        if let Some(projection) = projection {
            self.status_notice = None;
            self.replace_projection(projection, window, cx);
        }
        if notices.is_empty() {
            return;
        }
        if let Some(composer) = &self.composer {
            composer.update(cx, |composer, cx| composer.show_notices(&notices, cx));
        } else {
            self.status_notice = Some(bounded_status(notices.join("\n")));
            cx.notify();
        }
    }

    fn tokens(&self) -> ResolvedTokens {
        self.theme.resolved().clone()
    }

    fn current_projection(&self) -> Option<&SessionProjection> {
        self.controller
            .as_ref()
            .and_then(MainViewController::current_session)
            .and_then(|key| self.projection.session(key))
    }

    fn set_theme(&mut self, mode: ThemeMode, window: &mut Window, cx: &mut Context<Self>) {
        self.theme.set_mode(mode, cx);
        if let Some(timeline) = &self.timeline {
            timeline.update(cx, SessionTimeline::remeasure);
        }
        let current = self.current_projection().cloned();
        if let Some(controller) = &mut self.controller {
            controller.sync_topology(current.as_ref());
            controller.restore_current_focus();
        }
        self.apply_pending_focus(window, cx);
        cx.notify();
    }

    fn reveal_tool_call(
        &mut self,
        call: &ToolCallKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let valid = self.current_projection().is_some_and(|session| {
            session.key == call.session
                && session
                    .items
                    .iter()
                    .any(|item| matches!(item, TimelineItem::Tool(tool) if &tool.key == call))
        });
        if !valid {
            return;
        }
        let revealed = self.timeline.as_ref().is_some_and(|timeline| {
            timeline.update(cx, |timeline, cx| {
                timeline.reveal_tool(&call.call_id, window, cx)
            })
        });
        if !revealed {
            return;
        }
        let current = self.current_projection().cloned();
        if let Some(controller) = &mut self.controller {
            controller.note_focus(
                FocusAnchor::new(MainView::Session(call.session.clone()), Region::Timeline)
                    .with_item(call.call_id.clone()),
                current.as_ref(),
            );
        }
    }

    fn open_session(&mut self, session: SessionKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(projection) = self.projection.session(&session).cloned() else {
            return;
        };
        let Some(controller) = &mut self.controller else {
            return;
        };
        if !controller.open_session(session, &projection) {
            return;
        }
        self.navigation.update(cx, |navigation, cx| {
            navigation.select_session(&self.projection, &projection.key, cx);
        });
        if let Some(timeline) = &self.timeline {
            timeline.update(cx, |timeline, cx| {
                timeline.set_session(&projection, cx);
            });
        }
        if let Some(composer) = &self.composer {
            composer.update(cx, |composer, cx| {
                composer.set_session(&projection, self.projection.connection, window, cx);
            });
        }
        self.apply_pending_focus(window, cx);
        cx.notify();
    }

    fn return_to_origin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(controller) = &mut self.controller else {
            return;
        };
        if !controller.return_to_origin(&self.projection) {
            return;
        }
        if let Some(projection) = self.current_projection().cloned() {
            if let Some(timeline) = &self.timeline {
                timeline.update(cx, |timeline, cx| {
                    timeline.set_session(&projection, cx);
                });
            }
            if let Some(composer) = &self.composer {
                composer.update(cx, |composer, cx| {
                    composer.set_session(&projection, self.projection.connection, window, cx);
                });
            }
        }
        self.apply_pending_focus(window, cx);
        cx.notify();
    }

    fn model_menu_changed(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_projection().cloned();
        if let Some(controller) = &mut self.controller {
            controller.model_menu_changed(open, current.as_ref());
        }
        self.apply_pending_focus(window, cx);
    }

    fn host_filter_changed(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_projection().cloned();
        if let Some(controller) = &mut self.controller {
            controller.host_filter_changed(open, current.as_ref());
        }
        self.apply_pending_focus(window, cx);
    }

    fn host_filter_focused(&mut self, cx: &mut App) {
        let view = self
            .controller
            .as_ref()
            .map(|controller| controller.navigation.current_view().clone());
        if let Some(view) = view {
            self.note_focus(
                FocusAnchor::new(view, Region::Navigation).with_item("host-filter"),
                cx,
            );
        }
    }

    fn note_focus(&mut self, anchor: FocusAnchor, _cx: &mut App) {
        let current = self.current_projection().cloned();
        if let Some(controller) = &mut self.controller {
            controller.note_focus(anchor, current.as_ref());
        }
    }

    fn queue_item_changed(&mut self, session: &SessionKey, item_id: &str, text: String) {
        let Some(item) = self
            .projection
            .sessions
            .iter_mut()
            .find(|projection| &projection.key == session)
            .and_then(|projection| projection.queue.iter_mut().find(|item| item.id == item_id))
        else {
            return;
        };
        item.text = text;
    }

    fn replace_queue(
        &mut self,
        session: SessionKey,
        queue: Vec<QueueItemProjection>,
        removed_item: Option<String>,
        focus_item: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A removed Cancel button is not guaranteed to emit another focus event.
        // Seed its row anchor so removing the last row can fall back to Composer.
        if let Some(item_id) = removed_item {
            let current = self.current_projection().cloned();
            if let Some(controller) = &mut self.controller {
                controller.note_focus(
                    FocusAnchor::new(MainView::Session(session.clone()), Region::Queue)
                        .with_item(item_id),
                    current.as_ref(),
                );
            }
        }
        let Some(projection) = self
            .projection
            .sessions
            .iter_mut()
            .find(|projection| projection.key == session)
        else {
            return;
        };
        projection.queue = queue;
        let current = self.current_projection().cloned();
        if let Some(controller) = &mut self.controller {
            if let Some(item_id) = focus_item {
                controller.note_focus(
                    FocusAnchor::new(MainView::Session(session), Region::Queue).with_item(item_id),
                    current.as_ref(),
                );
                controller.restore_current_focus();
            } else {
                controller.sync_topology(current.as_ref());
            }
        }
        self.apply_pending_focus(window, cx);
        cx.notify();
    }

    /// Replace Host facts while preserving native per-Session UI state.
    fn replace_projection(
        &mut self,
        mut projection: WorkbenchProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use std::collections::BTreeSet;

        if let Some(composer) = &self.composer {
            composer.read_with(cx, |composer, cx| {
                composer.apply_local_queues(&mut projection, cx);
            });
        }
        let host_menu_open = self.controller.as_ref().is_some_and(|controller| {
            matches!(
                controller.focus.current().scope,
                FocusScope::Overlay(id) if id == "host-filter"
            )
        });
        let close_host_menu = host_menu_open && self.navigation.read(cx).hosts_changed(&projection);
        let focus_header_after_close = close_host_menu && projection.sessions.is_empty();
        if close_host_menu {
            // DropdownMenu builds its items once per opening. Keep the stale
            // popup focused until Cancel; an empty shell focuses Header next.
            let header_focus = self.header_focus.clone();
            window.defer(cx, move |window, cx| {
                window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
                if focus_header_after_close {
                    header_focus.focus(window, cx);
                }
            });
        }
        let previous_session = self.current_projection().map(|session| session.key.clone());
        self.projection = projection;
        let live = self
            .projection
            .sessions
            .iter()
            .map(|session| session.key.clone())
            .collect::<BTreeSet<_>>();
        if live.is_empty() {
            self.controller = None;
            self.timeline = None;
            self.composer = None;
        } else {
            let first = self.projection.sessions.first().expect("live Session");
            let snapshot = navigation_snapshot(&self.projection, first.key.clone());
            if let Some(controller) = &mut self.controller {
                controller.replace_projection(&self.projection, snapshot);
            } else {
                self.controller = Some(MainViewController::new(first, snapshot));
            }
            let selected = self.current_projection().map(|session| session.key.clone());
            let reveal_selected = selected != previous_session;
            self.navigation.update(cx, |navigation, cx| {
                navigation.replace_projection(
                    &self.projection,
                    selected.as_ref(),
                    reveal_selected,
                    cx,
                );
            });
            if let Some(projection) = self.current_projection().cloned() {
                let connection = self.projection.connection;
                if self.timeline.is_none() {
                    let parent = cx.weak_entity();
                    self.timeline =
                        Some(cx.new(|cx| SessionTimeline::new(&projection, parent, cx)));
                }
                if self.composer.is_none() {
                    let parent = cx.weak_entity();
                    let effects = Rc::clone(&self.effects);
                    self.composer = Some(cx.new(|cx| {
                        Composer::new(&projection, connection, parent, effects, window, cx)
                    }));
                }
                if let Some(timeline) = &self.timeline {
                    timeline.update(cx, |timeline, cx| {
                        timeline.set_session(&projection, cx);
                        timeline.prune(&live);
                    });
                }
                if let Some(composer) = &self.composer {
                    composer.update(cx, |composer, cx| {
                        composer.set_session(&projection, connection, window, cx);
                        composer.prune(&live);
                    });
                }
            }
        }
        self.navigation.update(cx, |navigation, cx| {
            if live.is_empty() {
                navigation.replace_projection(&self.projection, None, false, cx);
            }
        });
        if self.controller.is_some() {
            self.apply_pending_focus(window, cx);
        } else if !focus_header_after_close {
            self.header_focus.clone().focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn mount(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_pending_focus(window, cx);
    }

    fn apply_pending_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(intent) = self
            .controller
            .as_mut()
            .and_then(MainViewController::take_focus_intent)
        else {
            return;
        };
        let target = match intent {
            FocusIntent::Focus(target) => Some(target),
            FocusIntent::CloseOverlay { restore, .. } => Some(restore),
            FocusIntent::Noop => None,
        };
        let Some(target) = target else {
            return;
        };
        // Native PopupMenu owns overlay focus. The reducer records its scope;
        // only shell restoration needs a product FocusHandle here.
        if target.scope != FocusScope::Shell {
            return;
        }
        match self.resolve_focus(&target, cx) {
            Some(ResolvedFocus::Handle(handle)) => handle.focus(window, cx),
            Some(ResolvedFocus::Tree(tree)) => {
                tree.update(cx, |tree, cx| tree.focus(window, cx));
            }
            None => self.header_focus.clone().focus(window, cx),
        }
    }

    fn resolve_focus(&self, target: &FocusTarget, cx: &App) -> Option<ResolvedFocus> {
        match target.anchor.region {
            Region::Navigation if target.anchor.item_id.as_deref() == Some("host-filter") => Some(
                ResolvedFocus::Handle(self.navigation.read(cx).filter_focus()),
            ),
            Region::Navigation => {
                Some(ResolvedFocus::Tree(self.navigation.read(cx).tree_state(cx)))
            }
            Region::Header | Region::SourceHeader | Region::DiffContent | Region::Transcript => {
                Some(ResolvedFocus::Handle(self.header_focus.clone()))
            }
            Region::Timeline => self.timeline.as_ref().and_then(|timeline| {
                timeline
                    .read(cx)
                    .timeline_focus(target.anchor.item_id.as_deref(), cx)
                    .map(ResolvedFocus::Handle)
            }),
            Region::Approval => self
                .composer
                .as_ref()
                .map(|composer| ResolvedFocus::Handle(composer.read(cx).approval_focus(cx))),
            Region::Queue => self.composer.as_ref().map(|composer| {
                ResolvedFocus::Handle(
                    composer
                        .read(cx)
                        .queue_focus(target.anchor.item_id.as_deref(), cx),
                )
            }),
            Region::Composer => self.composer.as_ref().map(|composer| {
                let composer = composer.read(cx);
                let handle = if target.anchor.item_id.as_deref() == Some("model-effort") {
                    composer.menu_focus(cx)
                } else {
                    composer.input_focus(cx)
                };
                ResolvedFocus::Handle(handle)
            }),
        }
    }
}

impl Render for Workbench {
    #[allow(clippy::too_many_lines)] // Stable shell geometry stays in one place.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens();
        let mode = tokens.mode;
        let current_view = self
            .controller
            .as_ref()
            .map(|controller| controller.navigation.current_view().clone());
        let current = self.current_projection().cloned();
        let title = current
            .as_ref()
            .map_or("No Session selected", |session| session.title.as_str());
        let context = current.as_ref().map_or_else(
            || "No Host or Worktree selected".to_owned(),
            |session| {
                let key = &session.key;
                format!(
                    "{} · {} · {}",
                    key.worktree.project.host_id,
                    key.worktree.project.project_id,
                    key.worktree.worktree_id
                )
            },
        );
        let connection = match self.projection.connection {
            ConnectionState::Online => "Online · Host connected",
            ConnectionState::Offline => "Offline · Host actions unavailable",
        };
        let composer_height = current.as_ref().map_or(COMPOSER_REGION_HEIGHT, |session| {
            if !session.queue.is_empty() {
                COMPOSER_QUEUE_HEIGHT
            } else if session.pending_approval().is_some() {
                COMPOSER_APPROVAL_HEIGHT
            } else {
                COMPOSER_REGION_HEIGHT
            }
        });
        let navigation = self.navigation.clone();
        let timeline = self.timeline.clone();
        let composer = self.composer.clone();

        div()
            .flex()
            .size_full()
            .min_w_0()
            .bg(tokens.projected.background)
            .text_color(tokens.projected.foreground)
            .child(
                div()
                    .id("session-navigation")
                    .test_support()
                    .w(px(SIDEBAR_WIDTH))
                    .h_full()
                    .flex_none()
                    .bg(tokens.metis.navigation_surface)
                    .border_r_1()
                    .border_color(tokens.metis.border_subtle)
                    .child(div().size_full().child(navigation)),
            )
            .child(
                div()
                    .id("session-main")
                    .test_support()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .w_full()
                    .h_full()
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .id("session-header")
                            .test_support()
                            .h(px(SESSION_HEADER_HEIGHT))
                            .flex()
                            .justify_center()
                            .border_b_1()
                            .border_color(tokens.metis.border_subtle)
                            .track_focus(&self.header_focus)
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(SESSION_HEADER_WIDTH))
                                    .flex_col()
                                    .justify_center()
                                    .gap_1()
                                    .py_2()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_size(px(16.))
                                                    .child(title.to_owned()),
                                            )
                                            .child(
                                                Button::new("theme-toggle")
                                                    .label(if mode.is_dark() {
                                                        "Use Light"
                                                    } else {
                                                        "Use Dark"
                                                    })
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.set_theme(
                                                                if mode.is_dark() {
                                                                    ThemeMode::Light
                                                                } else {
                                                                    ThemeMode::Dark
                                                                },
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_color(tokens.projected.muted_foreground)
                                            .text_size(tokens.metis.metadata_font_size)
                                            .child(context),
                                    )
                                    .child(
                                        div()
                                            .text_color(tokens.projected.muted_foreground)
                                            .text_size(tokens.metis.metadata_font_size)
                                            .child(connection),
                                    ),
                            ),
                    )
                    .children(match current_view {
                        Some(MainView::Session(_)) => Some(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(SESSION_HEADER_HEIGHT))
                                .bottom_0()
                                .children(timeline.map(|timeline| {
                                    div()
                                        .id("session-timeline-region")
                                        .test_support()
                                        .absolute()
                                        .left_0()
                                        .right_0()
                                        .top_0()
                                        .bottom(px(composer_height))
                                        .flex()
                                        .justify_center()
                                        .child(
                                            div()
                                                .id("session-timeline-content")
                                                .test_support()
                                                .w_full()
                                                .max_w(px(SESSION_CONTENT_WIDTH))
                                                .h_full()
                                                .child(timeline),
                                        )
                                }))
                                .children(composer.map(|composer| {
                                    div()
                                        .id("session-composer-region")
                                        .test_support()
                                        .absolute()
                                        .left_0()
                                        .right_0()
                                        .bottom_0()
                                        .h(px(composer_height))
                                        .overflow_y_scroll()
                                        .flex()
                                        .justify_center()
                                        .child(
                                            div()
                                                .id("session-composer-content")
                                                .test_support()
                                                .w_full()
                                                .max_w(px(SESSION_CONTENT_WIDTH))
                                                .child(composer),
                                        )
                                })),
                        ),
                        Some(MainView::Diff { .. } | MainView::Subagent { .. }) => Some(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(SESSION_HEADER_HEIGHT))
                                .bottom_0()
                                .p_4()
                                .child("This slice keeps Diff and Subagent as pure models only.")
                                .child(Button::new("return-to-session").label("Return").on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.return_to_origin(window, cx);
                                    }),
                                )),
                        ),
                        None => Some(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(SESSION_HEADER_HEIGHT))
                                .bottom_0()
                                .items_center()
                                .justify_center()
                                .p_8()
                                .child(
                                    div()
                                        .text_color(tokens.metis.foreground_secondary)
                                        .child(self.status_notice.clone().unwrap_or_else(|| {
                                            SharedString::new_static("No Host is connected. No Session or execution is being simulated.")
                                        })),
                                ),
                        ),
                    }),
            )
    }
}

fn navigation_snapshot(
    projection: &WorkbenchProjection,
    fallback: SessionKey,
) -> NavigationSnapshot {
    let mut snapshot = NavigationSnapshot::new(fallback);
    for session in &projection.sessions {
        snapshot.add_session(session.key.clone());
        for subagent in &session.subagents {
            snapshot.add_subagent(subagent.key.clone(), subagent.continuable);
        }
    }
    snapshot
}

fn bounded_status(mut notice: String) -> SharedString {
    if notice.len() > STATUS_NOTICE_BYTES {
        let mut boundary = STATUS_NOTICE_BYTES;
        while !notice.is_char_boundary(boundary) {
            boundary -= 1;
        }
        notice.truncate(boundary);
        notice.push('…');
    }
    notice.into()
}

pub(super) fn logical_id(kind: &str, parts: &[&str]) -> String {
    let mut id = String::from(kind);
    for part in parts {
        let _ = write!(id, ":{}:{part}", part.len());
    }
    id
}

pub(super) fn session_id(session: &SessionKey) -> String {
    logical_id(
        "session",
        &[
            &session.worktree.project.host_id,
            &session.worktree.project.project_id,
            &session.worktree.worktree_id,
            &session.session_id,
        ],
    )
}

#[cfg(test)]
mod cold_start_tests;
#[cfg(test)]
mod tests;
