//! UI-thread Host Session bridge between the async transport and the bounded
//! projection reducer. No GPUI types appear here: the product entry drains
//! this bridge and hands the resulting projection to the native Workbench.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use metis::{
    client::{ApprovalOutcome, ClientResult},
    session_transport::{TransportCommand, TransportEvent},
};
use tokio::sync::mpsc;

use super::{
    ConnectionState, EffectHandler, EffectResult, SessionIntent, SessionKey, WorkbenchProjection,
    host_projection::HostSessionReducer,
};
use crate::workbench::history::ApprovalDecision;

/// Notice delivery is best-effort; the oldest entries drop first so a broken
/// Host cannot grow the queue without bound.
const NOTICE_LIMIT: usize = 8;
const NOTICE_TEXT_BYTES: usize = 2 * 1024;
const NOTICE_TOTAL_BYTES: usize = 4 * 1024;

pub(super) struct GitDiffResult {
    pub(super) request_id: u64,
    pub(super) result: ClientResult<String>,
}

pub(super) struct HostSessionBridge {
    session: SessionKey,
    reducer: HostSessionReducer,
    commands: mpsc::Sender<TransportCommand>,
    events: mpsc::Receiver<TransportEvent>,
    connection: ConnectionState,
    generation: u64,
    prompt_counter: u64,
    /// A rejected frame invalidates stream order: fail closed and ignore
    /// further frames instead of projecting a partially trusted history.
    failed: bool,
    dirty: bool,
    notices: VecDeque<String>,
    pending_git_diff: Option<u64>,
    git_diff_result: Option<GitDiffResult>,
}

impl HostSessionBridge {
    pub(super) fn new(
        session: SessionKey,
        commands: mpsc::Sender<TransportCommand>,
        events: mpsc::Receiver<TransportEvent>,
    ) -> Self {
        Self {
            reducer: HostSessionReducer::new(session.clone(), 0, ""),
            session,
            commands,
            events,
            connection: ConnectionState::Offline,
            generation: 0,
            prompt_counter: 0,
            failed: false,
            dirty: false,
            notices: VecDeque::new(),
            pending_git_diff: None,
            git_diff_result: None,
        }
    }

    /// Drain every pending transport event. Returns a fresh projection when
    /// visible state changed; notices are taken separately.
    pub(super) fn poll(&mut self) -> Option<WorkbenchProjection> {
        while let Ok(event) = self.events.try_recv() {
            self.apply(event);
        }
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(WorkbenchProjection {
            connection: self.connection,
            sessions: vec![self.reducer.projection()],
        })
    }

    pub(super) fn take_notices(&mut self) -> Vec<String> {
        self.notices.drain(..).collect()
    }

    pub(super) fn take_git_diff_result(&mut self) -> Option<GitDiffResult> {
        self.git_diff_result.take()
    }

    #[cfg(test)]
    pub(super) fn cursor(&self) -> Option<i64> {
        self.reducer.cursor()
    }

    /// Production effect handler: `Recorded` means the intent reached the
    /// bounded transport queue, never that the Host accepted it.
    pub(super) fn effect_handler(bridge: &Rc<RefCell<Self>>) -> EffectHandler {
        let bridge = Rc::clone(bridge);
        Rc::new(RefCell::new(move |intent| {
            bridge.borrow_mut().record(intent)
        }))
    }

    fn apply(&mut self, event: TransportEvent) {
        match event {
            TransportEvent::Connected { client_id } => {
                self.pending_git_diff = None;
                self.git_diff_result = None;
                self.generation += 1;
                self.reducer.replace_generation(self.generation, client_id);
                self.connection = ConnectionState::Online;
                self.failed = false;
                self.dirty = true;
            }
            TransportEvent::FollowFrame(frame) => {
                if self.failed {
                    return;
                }
                match self.reducer.apply(&frame) {
                    Ok(()) => self.dirty = true,
                    Err(error) => {
                        self.failed = true;
                        self.pending_git_diff = None;
                        self.connection = ConnectionState::Offline;
                        self.dirty = true;
                        self.push_notice(format!(
                            "Host Session history was rejected; the projection is frozen: {error}"
                        ));
                    }
                }
            }
            TransportEvent::Host(event) => {
                if self.failed {
                    return;
                }
                match self.reducer.apply_host_event(event) {
                    Ok(changed) => self.dirty |= changed,
                    Err(error) => {
                        // The reducer already failed closed on answerability.
                        self.dirty = true;
                        self.push_notice(format!("Host approval tracking was disabled: {error}"));
                    }
                }
            }

            TransportEvent::CommandFailed {
                operation,
                error,
                request_id,
            } => {
                let request = request_id.map_or_else(String::new, |id| format!(" (request {id})"));
                self.push_notice(format!(
                    "{operation} was not completed by the Host{request}: {error}"
                ));
            }
            TransportEvent::GitDiff { request_id, result } => {
                if self.pending_git_diff == Some(request_id) {
                    self.pending_git_diff = None;
                    self.git_diff_result = Some(GitDiffResult { request_id, result });
                }
            }
            TransportEvent::Closed { reason } => {
                self.pending_git_diff = None;
                self.connection = ConnectionState::Offline;
                self.dirty = true;
                if let Some(error) = reason {
                    self.push_notice(format!("Host connection closed: {error}"));
                }
            }
        }
    }

    fn record(&mut self, intent: SessionIntent) -> EffectResult {
        if self.connection != ConnectionState::Online || self.failed {
            return EffectResult::Unavailable;
        }
        let command = match intent {
            // Model and effort stay local for now: the Host client has no
            // selection RPC yet, so only the prompt text is submitted. The
            // request id is caller-owned so a later retry can reuse it.
            SessionIntent::Send { session, text, .. }
            | SessionIntent::Queue { session, text, .. } => {
                if session != self.session {
                    return EffectResult::Unavailable;
                }
                self.prompt_counter += 1;
                TransportCommand::Prompt {
                    request_id: format!(
                        "metis-prompt-{}-{}",
                        self.reducer.client_id(),
                        self.prompt_counter
                    ),
                    text,
                }
            }
            SessionIntent::GitDiff {
                request_id,
                worktree,
            } => {
                if worktree != self.session.worktree {
                    return EffectResult::Unavailable;
                }
                TransportCommand::GitDiff {
                    request_id,
                    project_id: self.session.worktree.project.project_id.clone(),
                }
            }
            SessionIntent::Approval {
                call,
                decision,
                generation,
            } => {
                let Some(request) = self.reducer.approval_request(&call, generation).cloned()
                else {
                    return EffectResult::Unavailable;
                };
                TransportCommand::Answer {
                    request,
                    outcome: match decision {
                        ApprovalDecision::AllowOnce => ApprovalOutcome::AllowedOnce,
                        ApprovalDecision::Reject => ApprovalOutcome::Rejected,
                    },
                }
            }
        };
        let diff_request = match &command {
            TransportCommand::GitDiff { request_id, .. } => Some(*request_id),
            _ => None,
        };
        if self.commands.try_send(command).is_err() {
            return EffectResult::Unavailable;
        }
        if let Some(request_id) = diff_request {
            self.pending_git_diff = Some(request_id);
            self.git_diff_result = None;
        }
        EffectResult::Recorded
    }

    fn push_notice(&mut self, mut notice: String) {
        if notice.len() > NOTICE_TEXT_BYTES {
            let mut boundary = NOTICE_TEXT_BYTES;
            while !notice.is_char_boundary(boundary) {
                boundary -= 1;
            }
            notice.truncate(boundary);
            notice.push('…');
        }
        while self.notices.len() >= NOTICE_LIMIT
            || self.notices.iter().map(String::len).sum::<usize>() + notice.len()
                > NOTICE_TOTAL_BYTES
        {
            self.notices.pop_front();
        }
        self.notices.push_back(notice);
    }
}

#[cfg(test)]
mod tests {
    use metis::client::{ApprovalRequest, ClientError, HostEvent};
    use serde_json::{Value, json};

    use super::*;
    use crate::workbench::{
        identity::{ProjectKey, WorktreeKey},
        session::{ApprovalState, TimelineItem},
    };

    fn session_key() -> SessionKey {
        SessionKey::new(
            WorktreeKey::new(ProjectKey::new("host", "project"), "worktree"),
            "session-1",
        )
    }

    struct Fixture {
        bridge: Rc<RefCell<HostSessionBridge>>,
        events: mpsc::Sender<TransportEvent>,
        commands: mpsc::Receiver<TransportCommand>,
    }

    fn fixture() -> Fixture {
        let (command_sender, command_receiver) = mpsc::channel(16);
        let (event_sender, event_receiver) = mpsc::channel(16);
        Fixture {
            bridge: Rc::new(RefCell::new(HostSessionBridge::new(
                session_key(),
                command_sender,
                event_receiver,
            ))),
            events: event_sender,
            commands: command_receiver,
        }
    }

    fn event(seq: i64, kind: &str, data: Value) -> Value {
        let mut event = json!({ "type": kind, "seq": seq, "time": 100 });
        event["data"] = data;
        event
    }

    fn tool_call(seq: i64, call_id: &str) -> Value {
        event(
            seq,
            "tool/call",
            json!({
                "turn": 1,
                "step": 1,
                "callId": call_id,
                "name": "bash",
                "arguments": "{\"command\":\"pwd\"}",
            }),
        )
    }

    fn approval_asked(seq: i64, id: &str, call_id: &str) -> Value {
        event(
            seq,
            "approval/asked",
            json!({
                "id": id,
                "toolName": "bash",
                "callId": call_id,
                "reason": "Verify the working directory",
            }),
        )
    }

    fn snapshot(events: &[Value], cursor: i64) -> Value {
        json!({
            "type": "snapshot",
            "header": {
                "version": 4,
                "id": "session-1",
                "createdAt": 0,
                "cwd": "/workspace",
                "isSeeded": false,
            },
            "cursor": cursor,
            "hasMore": false,
            "records": events.iter().map(|event| json!({ "type": "event", "event": event })).collect::<Vec<_>>(),
            "projections": {
                "asOfSeq": cursor,
                "values": {
                    "title": "Host session",
                    "sessionListMetadata": { "blank": false, "lastPromptAt": null },
                },
            },
            "assistantStream": { "revision": 0 },
        })
    }

    fn live_approval(client_id: &str, event_id: &str, call_id: &str) -> HostEvent {
        HostEvent::Approval(ApprovalRequest {
            client_id: client_id.to_owned(),
            event_id: event_id.to_owned(),
            agent_id: "session-1".to_owned(),
            request: json!({
                "toolName": "bash",
                "callId": call_id,
                "reason": "Verify the working directory",
            }),
        })
    }

    fn send(fixture: &Fixture, event: TransportEvent) {
        fixture
            .events
            .try_send(event)
            .expect("queue transport event");
    }

    fn connect(fixture: &Fixture) {
        send(
            fixture,
            TransportEvent::Connected {
                client_id: "client-a".to_owned(),
            },
        );
    }

    fn send_intent(fixture: &Fixture, intent: SessionIntent) -> EffectResult {
        let handler = HostSessionBridge::effect_handler(&fixture.bridge);
        (handler.borrow_mut())(intent)
    }

    #[test]
    fn notices_have_utf8_safe_entry_and_total_byte_budgets() {
        let fixture = fixture();
        fixture
            .bridge
            .borrow_mut()
            .push_notice("界".repeat(NOTICE_TEXT_BYTES));
        let notice = fixture.bridge.borrow_mut().take_notices().remove(0);
        assert!(notice.len() <= NOTICE_TEXT_BYTES + '…'.len_utf8());
        assert!(notice.ends_with('…'));

        for _ in 0..NOTICE_LIMIT * 2 {
            fixture
                .bridge
                .borrow_mut()
                .push_notice("x".repeat(NOTICE_TEXT_BYTES));
        }
        let notices = fixture.bridge.borrow_mut().take_notices();
        assert!(notices.len() <= NOTICE_LIMIT);
        assert!(notices.iter().map(String::len).sum::<usize>() <= NOTICE_TOTAL_BYTES);
    }

    #[test]
    fn connected_snapshots_build_an_online_projection() {
        let fixture = fixture();
        assert!(fixture.bridge.borrow_mut().poll().is_none());
        assert_eq!(
            send_intent(
                &fixture,
                SessionIntent::Send {
                    session: session_key(),
                    text: "offline".to_owned(),
                    model: String::new(),
                    effort: None,
                }
            ),
            EffectResult::Unavailable
        );

        connect(&fixture);
        send(&fixture, TransportEvent::FollowFrame(snapshot(&[], -1)));
        let projection = fixture
            .bridge
            .borrow_mut()
            .poll()
            .expect("connected projection");
        assert_eq!(projection.connection, ConnectionState::Online);
        let session = projection.sessions.first().expect("one bound session");
        assert_eq!(session.title, "Host session");
        assert_eq!(session.generation, 1);
        assert!(fixture.bridge.borrow_mut().poll().is_none());
    }

    #[test]
    fn git_diff_is_worktree_scoped_and_ignores_stale_results() {
        let mut fixture = fixture();
        connect(&fixture);
        fixture.bridge.borrow_mut().poll();

        let worktree = session_key().worktree;
        assert_eq!(
            send_intent(
                &fixture,
                SessionIntent::GitDiff {
                    request_id: 7,
                    worktree: WorktreeKey::new(
                        ProjectKey::new("host", "other-project"),
                        "worktree",
                    ),
                }
            ),
            EffectResult::Unavailable
        );
        assert_eq!(
            send_intent(
                &fixture,
                SessionIntent::GitDiff {
                    request_id: 7,
                    worktree,
                }
            ),
            EffectResult::Recorded
        );
        assert!(matches!(
            fixture.commands.try_recv(),
            Ok(TransportCommand::GitDiff { request_id: 7, project_id })
                if project_id == "project"
        ));

        send(
            &fixture,
            TransportEvent::GitDiff {
                request_id: 6,
                result: Ok("stale patch".to_owned()),
            },
        );
        fixture.bridge.borrow_mut().poll();
        assert!(fixture.bridge.borrow_mut().take_git_diff_result().is_none());

        send(
            &fixture,
            TransportEvent::GitDiff {
                request_id: 7,
                result: Ok("current patch".to_owned()),
            },
        );
        fixture.bridge.borrow_mut().poll();
        let result = fixture
            .bridge
            .borrow_mut()
            .take_git_diff_result()
            .expect("matching Git Diff result");
        assert_eq!(result.request_id, 7);
        assert_eq!(result.result.expect("successful patch"), "current patch");
    }

    #[test]
    fn send_and_queue_intents_forward_prompts_only_for_the_bound_session() {
        let mut fixture = fixture();
        connect(&fixture);
        fixture.bridge.borrow_mut().poll();

        for intent in [
            SessionIntent::Send {
                session: session_key(),
                text: "send text".to_owned(),
                model: "model".to_owned(),
                effort: Some("high".to_owned()),
            },
            SessionIntent::Queue {
                session: session_key(),
                item_id: "queue-1".to_owned(),
                text: "queued text".to_owned(),
                model: "model".to_owned(),
                effort: None,
            },
        ] {
            assert_eq!(send_intent(&fixture, intent), EffectResult::Recorded);
        }
        assert_eq!(
            send_intent(
                &fixture,
                SessionIntent::Send {
                    session: SessionKey::new(
                        WorktreeKey::new(ProjectKey::new("host", "project"), "worktree"),
                        "other-session",
                    ),
                    text: "misrouted".to_owned(),
                    model: String::new(),
                    effort: None,
                }
            ),
            EffectResult::Unavailable
        );

        for (text, suffix) in [("send text", "1"), ("queued text", "2")] {
            match fixture.commands.try_recv() {
                Ok(TransportCommand::Prompt {
                    request_id,
                    text: actual,
                }) => {
                    assert_eq!(actual, text);
                    assert_eq!(request_id, format!("metis-prompt-client-a-{suffix}"));
                }
                other => panic!("expected prompt command, got {other:?}"),
            }
        }
        assert!(fixture.commands.try_recv().is_err());
    }

    #[test]
    fn approval_effects_require_a_live_request_in_the_current_generation() {
        let mut fixture = fixture();
        connect(&fixture);
        send(
            &fixture,
            TransportEvent::FollowFrame(snapshot(
                &[
                    tool_call(0, "call-1"),
                    approval_asked(1, "approval-1", "call-1"),
                ],
                1,
            )),
        );
        send(
            &fixture,
            TransportEvent::Host(live_approval("client-a", "event-1", "call-1")),
        );
        let projection = fixture
            .bridge
            .borrow_mut()
            .poll()
            .expect("pending approval");
        let session = projection.sessions.first().expect("bound session");
        let call = session
            .items
            .iter()
            .find_map(|item| match item {
                TimelineItem::Tool(call) => Some(call),
                _ => None,
            })
            .expect("tool card");
        assert_eq!(call.approval, ApprovalState::Pending);

        let stale = send_intent(
            &fixture,
            SessionIntent::Approval {
                call: call.key.clone(),
                decision: ApprovalDecision::AllowOnce,
                generation: 99,
            },
        );
        assert_eq!(stale, EffectResult::Unavailable);
        assert!(fixture.commands.try_recv().is_err());

        let recorded = send_intent(
            &fixture,
            SessionIntent::Approval {
                call: call.key.clone(),
                decision: ApprovalDecision::AllowOnce,
                generation: session.generation,
            },
        );
        assert_eq!(recorded, EffectResult::Recorded);
        match fixture.commands.try_recv() {
            Ok(TransportCommand::Answer { request, outcome }) => {
                assert_eq!(request.event_id, "event-1");
                assert_eq!(request.client_id, "client-a");
                assert_eq!(outcome, ApprovalOutcome::AllowedOnce);
            }
            other => panic!("expected answer command, got {other:?}"),
        }
    }

    #[test]
    fn failures_and_closure_are_honest_and_bounded() {
        let fixture = fixture();
        connect(&fixture);
        fixture.bridge.borrow_mut().poll();

        send(
            &fixture,
            TransportEvent::CommandFailed {
                operation: "session/prompt",
                error: ClientError::ConnectionClosed,
                request_id: Some("metis-prompt-client-a-1".to_owned()),
            },
        );
        send(&fixture, TransportEvent::Closed { reason: None });
        let projection = fixture
            .bridge
            .borrow_mut()
            .poll()
            .expect("offline projection");
        assert_eq!(projection.connection, ConnectionState::Offline);
        let notices = fixture.bridge.borrow_mut().take_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("session/prompt"));
        assert!(notices[0].contains("metis-prompt-client-a-1"));
        assert_eq!(
            fixture.bridge.borrow_mut().take_notices(),
            Vec::<String>::new()
        );
        assert_eq!(
            send_intent(
                &fixture,
                SessionIntent::Send {
                    session: session_key(),
                    text: "offline".to_owned(),
                    model: String::new(),
                    effort: None,
                }
            ),
            EffectResult::Unavailable
        );

        for index in 0..20 {
            fixture
                .bridge
                .borrow_mut()
                .push_notice(format!("notice {index}"));
        }
        let notices = fixture.bridge.borrow_mut().take_notices();
        assert_eq!(notices.len(), NOTICE_LIMIT);
        assert_eq!(notices.last().map(String::as_str), Some("notice 19"));
    }

    #[test]
    fn rejected_frames_freeze_the_projection_until_a_new_generation() {
        let fixture = fixture();
        connect(&fixture);
        send(
            &fixture,
            TransportEvent::FollowFrame(json!({ "type": "unknown" })),
        );
        let projection = fixture
            .bridge
            .borrow_mut()
            .poll()
            .expect("failed projection");
        assert_eq!(projection.connection, ConnectionState::Offline);
        assert_eq!(fixture.bridge.borrow_mut().take_notices().len(), 1);

        send(&fixture, TransportEvent::FollowFrame(snapshot(&[], -1)));
        assert!(fixture.bridge.borrow_mut().poll().is_none());
        assert_eq!(
            send_intent(
                &fixture,
                SessionIntent::Send {
                    session: session_key(),
                    text: "frozen".to_owned(),
                    model: String::new(),
                    effort: None,
                }
            ),
            EffectResult::Unavailable
        );

        connect(&fixture);
        send(&fixture, TransportEvent::FollowFrame(snapshot(&[], -1)));
        let projection = fixture.bridge.borrow_mut().poll().expect("new generation");
        assert_eq!(projection.connection, ConnectionState::Online);
        assert_eq!(projection.sessions[0].generation, 2);
        assert_eq!(projection.sessions[0].title, "Host session");
    }
}
