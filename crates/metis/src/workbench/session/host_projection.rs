//! Pure Host Session projection over the ordered durable journal.
//!
//! Transport ownership stays outside this module. The reducer never submits an
//! intent, infers success from connectivity, or revives an approval without a
//! current-generation request.

use std::{collections::BTreeMap, rc::Rc};

use metis::{
    client::{ApprovalRequest, ClientError, ClientResult, HostEvent},
    journal::{JournalUpdate, SessionJournal},
};
use serde_json::Value;

use super::{
    ApprovalState, ExecutionState, SessionKey, SessionPermissionPolicy, SessionProjection,
    SharedText, TimelineItem, ToolCallKey, ToolCallProjection,
};
use crate::workbench::{history::ApprovalDecision, stats::StatsInput};

const VISIBLE_ITEM_LIMIT: usize = 200;
const LIVE_APPROVAL_LIMIT: usize = 64;

/// One Session's bounded visible projection plus the approval requests
/// answerable by the current `$events` generation.
pub(super) struct HostSessionReducer {
    key: SessionKey,
    client_id: String,
    journal: SessionJournal,
    state: ProjectionState,
    generation: u64,
    live_approvals: BTreeMap<String, ApprovalRequest>,
    approval_overflow: bool,
}

impl HostSessionReducer {
    pub(super) fn new(key: SessionKey, generation: u64, client_id: impl Into<String>) -> Self {
        let title = key.session_id.clone();
        Self {
            journal: SessionJournal::new(key.session_id.clone()),
            state: ProjectionState::new(title, ExecutionState::NotExecuted),
            key,
            client_id: client_id.into(),
            generation,
            live_approvals: BTreeMap::new(),
            approval_overflow: false,
        }
    }

    /// Apply one follow frame atomically. Snapshots build a replacement once;
    /// live events apply incrementally without replaying the visible window.
    pub(super) fn apply(&mut self, frame: &Value) -> ClientResult<()> {
        let mut journal = self.journal.clone();
        let update = journal.apply(frame)?;
        match update {
            JournalUpdate::Replace(events) => {
                let events = parse_events(&events, &self.key)?;
                let title = snapshot_title(frame, &self.key.session_id)?;
                let mut state = ProjectionState::new(title, snapshot_execution(frame)?);
                let mut live_approvals = self.live_approvals.clone();
                let baseline_title = frame
                    .pointer("/projections/values/title")
                    .and_then(Value::as_str)
                    .is_some();
                for event in events {
                    if baseline_title && matches!(event, DomainEvent::Title(_)) {
                        continue;
                    }
                    state.apply(event, &mut live_approvals);
                }
                self.journal = journal;
                self.state = state;
                self.live_approvals = live_approvals;
            }
            JournalUpdate::Append(events) => {
                let events = parse_events(&events, &self.key)?;
                self.journal = journal;
                for event in events {
                    self.state.apply(event, &mut self.live_approvals);
                }
            }
        }
        Ok(())
    }

    /// Merge the independent `$events` approval stream. A live request alone
    /// does not create a durable pending audit card.
    pub(super) fn apply_host_event(&mut self, event: HostEvent) -> ClientResult<bool> {
        match event {
            HostEvent::Approval(request) => {
                if request.client_id != self.client_id
                    || request.agent_id != self.key.session_id
                    || self.approval_overflow
                    || request
                        .request
                        .get("callId")
                        .and_then(Value::as_str)
                        .is_none()
                {
                    return Ok(false);
                }
                let is_new = !self.live_approvals.contains_key(&request.event_id);
                if is_new && self.live_approvals.len() >= LIVE_APPROVAL_LIMIT {
                    self.live_approvals.clear();
                    self.approval_overflow = true;
                    self.recompute_approvals();
                    return Err(ClientError::Protocol(
                        "Live approval capacity exceeded; answerability disabled for this generation",
                    ));
                }
                let call_id = request
                    .request
                    .get("callId")
                    .and_then(Value::as_str)
                    .expect("checked callId")
                    .to_owned();
                let changed = self
                    .live_approvals
                    .insert(request.event_id.clone(), request)
                    .is_none();
                self.state
                    .recompute_approval(&call_id, &self.live_approvals);
                Ok(changed)
            }
            HostEvent::Cancelled { event_id } => {
                let Some(request) = self.live_approvals.remove(&event_id) else {
                    return Ok(false);
                };
                // `$events` delivers a request at most once within one client
                // generation, so cancellation needs no unbounded tombstones.
                if let Some(call_id) = request.request.get("callId").and_then(Value::as_str) {
                    self.state.recompute_approval(call_id, &self.live_approvals);
                }
                Ok(true)
            }
            HostEvent::Emit { .. } | HostEvent::Waterfall { .. } => Ok(false),
        }
    }

    fn recompute_approvals(&mut self) {
        let calls = self.state.audits.keys().cloned().collect::<Vec<_>>();
        for call_id in calls {
            self.state
                .recompute_approval(&call_id, &self.live_approvals);
        }
    }

    /// Switch the opaque client generation without treating a durable sequence
    /// number as approval relevance.
    pub(super) fn replace_generation(&mut self, generation: u64, client_id: impl Into<String>) {
        self.generation = generation;
        self.client_id = client_id.into();
        self.live_approvals.clear();
        self.approval_overflow = false;
        self.recompute_approvals();
    }

    /// Opaque id of the current `$events` generation; the bridge derives
    /// caller-owned prompt request ids from it.
    pub(super) fn client_id(&self) -> &str {
        &self.client_id
    }

    #[cfg(all(test, unix))]
    pub(super) fn cursor(&self) -> Option<i64> {
        self.journal.cursor()
    }

    pub(super) fn projection(&self) -> SessionProjection {
        SessionProjection {
            key: self.key.clone(),
            title: self.state.title.clone(),
            generation: self.generation,
            execution: self.state.execution,
            items: Rc::new(
                self.state
                    .items
                    .iter()
                    .map(|entry| entry.item.clone())
                    .collect(),
            ),
            queue: Vec::new(),
            permission: SessionPermissionPolicy::AskEachTime,
            models: Vec::new(),
            initial_model: None,
            stats: StatsInput::default(),
            subagents: Vec::new(),
        }
    }

    /// Return the original request only when the current durable card and live
    /// generation still identify exactly the same answerable `ToolCall`.
    pub(super) fn approval_request(
        &self,
        key: &ToolCallKey,
        generation: u64,
    ) -> Option<&ApprovalRequest> {
        if generation != self.generation {
            return None;
        }
        let call = self.state.tool(&key.call_id)?;
        if &call.key != key || call.approval != ApprovalState::Pending {
            return None;
        }
        live_request(call, &self.live_approvals)
    }
}

struct ProjectionState {
    title: String,
    execution: ExecutionState,
    items: Vec<SourcedItem>,
    tool_positions: BTreeMap<String, usize>,
    message_positions: BTreeMap<String, usize>,
    audits: BTreeMap<String, ApprovalAudit>,
    results: BTreeMap<String, SourcedResult>,
}

impl ProjectionState {
    fn new(title: String, execution: ExecutionState) -> Self {
        Self {
            title,
            execution,
            items: Vec::new(),
            tool_positions: BTreeMap::new(),
            message_positions: BTreeMap::new(),
            audits: BTreeMap::new(),
            results: BTreeMap::new(),
        }
    }

    fn apply(
        &mut self,
        event: DomainEvent,
        live_approvals: &mut BTreeMap<String, ApprovalRequest>,
    ) {
        match event {
            DomainEvent::Ignored => {}
            DomainEvent::Title(title) => self.title = title,
            DomainEvent::TurnStart => self.execution = ExecutionState::Running,
            DomainEvent::TurnEnd(execution) => self.execution = execution,
            DomainEvent::Message { seq, surface, item } => {
                let replacement = self.replace_surface(surface);
                self.insert_message(replacement, seq, item);
            }
            DomainEvent::ToolCall { seq, call } => {
                let call_id = call.key.call_id.clone();
                if let Some(existing) = self.tool_mut(&call_id) {
                    existing.title = call.title;
                    existing.arguments = call.arguments;
                } else {
                    self.tool_positions.insert(call_id, self.items.len());
                    self.items.push(SourcedItem {
                        seq,
                        item: TimelineItem::Tool(call),
                    });
                }
            }
            DomainEvent::ToolResult {
                seq,
                surface,
                call_id,
                execution,
                output,
            } => {
                self.replace_surface(surface);
                if self.tool_positions.contains_key(&call_id) {
                    self.results.insert(call_id.clone(), SourcedResult { seq });
                    if let Some(call) = self.tool_mut(&call_id) {
                        call.execution = execution;
                        call.output = Some(output);
                    }
                }
            }
            DomainEvent::ApprovalAsked {
                id,
                call_id,
                tool_name,
                reason,
            } => {
                if self.tool_positions.contains_key(&call_id) {
                    let ambiguous = self.audits.contains_key(&call_id);
                    self.audits.insert(
                        call_id.clone(),
                        ApprovalAudit {
                            id,
                            tool_name,
                            reason,
                            outcome: None,
                            ambiguous,
                        },
                    );
                    self.recompute_approval(&call_id, live_approvals);
                }
            }
            DomainEvent::ApprovalDecided { id, outcome } => {
                let call_id = self
                    .audits
                    .iter()
                    .find(|(_, audit)| audit.id == id)
                    .map(|(call_id, _)| call_id.clone());
                if let Some(call_id) = call_id {
                    if let Some(audit) = self.audits.get_mut(&call_id) {
                        audit.outcome = Some(outcome);
                    }
                    self.recompute_approval(&call_id, live_approvals);
                }
            }
        }
        self.evict(live_approvals);
    }

    fn replace_surface(&mut self, surface: SurfaceOp) -> Option<usize> {
        let SurfaceOp::Replace { start, end } = surface else {
            return None;
        };
        let index = self
            .items
            .iter()
            .position(|entry| {
                entry.seq >= start
                    && entry.seq <= end
                    && !matches!(entry.item, TimelineItem::Tool(_))
            })
            .or_else(|| {
                self.items
                    .iter()
                    .rposition(|entry| entry.seq < start)
                    .map(|index| index + 1)
            })
            .unwrap_or(self.items.len());
        self.items.retain(|entry| {
            entry.seq < start || entry.seq > end || matches!(entry.item, TimelineItem::Tool(_))
        });
        let replaced_results = self
            .results
            .iter()
            .filter(|(_, result)| result.seq >= start && result.seq <= end)
            .map(|(call_id, _)| call_id.clone())
            .collect::<Vec<_>>();
        self.results
            .retain(|_, result| result.seq < start || result.seq > end);
        for call_id in replaced_results {
            if let Some(call) = self.tool_mut(&call_id) {
                call.execution = ExecutionState::Unknown;
                call.output = None;
            }
        }
        self.rebuild_indexes();
        Some(index)
    }

    fn insert_message(&mut self, replacement: Option<usize>, seq: i64, item: Option<TimelineItem>) {
        let Some(item) = item else {
            return;
        };
        if let Some(index) = replacement {
            self.items.insert(index, SourcedItem { seq, item });
            self.rebuild_indexes();
        } else if let Some(index) = self.message_positions.get(&item.stable_id()).copied() {
            self.items[index].item = item;
        } else {
            self.message_positions
                .insert(item.stable_id(), self.items.len());
            self.items.push(SourcedItem { seq, item });
        }
    }

    fn recompute_approval(
        &mut self,
        call_id: &str,
        live_approvals: &BTreeMap<String, ApprovalRequest>,
    ) {
        let Some(audit) = self.audits.get(call_id).cloned() else {
            return;
        };
        let Some(call) = self.tool(call_id) else {
            return;
        };
        let approval = match audit.outcome.as_deref() {
            Some("allowed-once") => ApprovalState::Decided(ApprovalDecision::AllowOnce),
            Some("rejected") => ApprovalState::Decided(ApprovalDecision::Reject),
            None if !audit.ambiguous
                && audit.tool_name.as_str() == call.title.as_ref()
                && live_request(call, live_approvals).is_some() =>
            {
                ApprovalState::Pending
            }
            Some(_) | None => ApprovalState::Stale,
        };
        if let Some(call) = self.tool_mut(call_id) {
            call.reason = audit.reason;
            call.approval = approval;
        }
    }

    fn evict(&mut self, live_approvals: &mut BTreeMap<String, ApprovalRequest>) {
        if self.items.len() <= VISIBLE_ITEM_LIMIT {
            return;
        }
        let removed = self
            .items
            .drain(..self.items.len() - VISIBLE_ITEM_LIMIT)
            .collect::<Vec<_>>();
        for entry in removed {
            let TimelineItem::Tool(call) = entry.item else {
                continue;
            };
            let call_id = call.key.call_id;
            self.audits.remove(&call_id);
            self.results.remove(&call_id);
            live_approvals.retain(|_, request| {
                request.request.get("callId").and_then(Value::as_str) != Some(call_id.as_str())
            });
        }
        self.rebuild_indexes();
    }

    fn rebuild_indexes(&mut self) {
        self.tool_positions.clear();
        self.message_positions.clear();
        for (index, entry) in self.items.iter().enumerate() {
            match &entry.item {
                TimelineItem::Tool(call) => {
                    self.tool_positions.insert(call.key.call_id.clone(), index);
                }
                item => {
                    self.message_positions.insert(item.stable_id(), index);
                }
            }
        }
    }

    fn tool(&self, call_id: &str) -> Option<&ToolCallProjection> {
        let index = self.tool_positions.get(call_id).copied()?;
        match &self.items.get(index)?.item {
            TimelineItem::Tool(call) => Some(call),
            _ => None,
        }
    }

    fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolCallProjection> {
        let index = self.tool_positions.get(call_id).copied()?;
        match &mut self.items.get_mut(index)?.item {
            TimelineItem::Tool(call) => Some(call),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct SourcedItem {
    seq: i64,
    item: TimelineItem,
}

struct SourcedResult {
    seq: i64,
}

#[derive(Clone)]
struct ApprovalAudit {
    id: String,
    tool_name: String,
    reason: Option<SharedText>,
    outcome: Option<String>,
    ambiguous: bool,
}

#[derive(Clone, Copy)]
enum SurfaceOp {
    Append,
    Replace { start: i64, end: i64 },
}

enum DomainEvent {
    Ignored,
    Title(String),
    TurnStart,
    TurnEnd(ExecutionState),
    Message {
        seq: i64,
        surface: SurfaceOp,
        item: Option<TimelineItem>,
    },
    ToolCall {
        seq: i64,
        call: ToolCallProjection,
    },
    ToolResult {
        seq: i64,
        surface: SurfaceOp,
        call_id: String,
        execution: ExecutionState,
        output: SharedText,
    },
    ApprovalAsked {
        id: String,
        call_id: String,
        tool_name: String,
        reason: Option<SharedText>,
    },
    ApprovalDecided {
        id: String,
        outcome: String,
    },
}

fn parse_events(events: &[Value], key: &SessionKey) -> ClientResult<Vec<DomainEvent>> {
    events.iter().map(|event| parse_event(event, key)).collect()
}

fn parse_event(event: &Value, session: &SessionKey) -> ClientResult<DomainEvent> {
    match event.get("type").and_then(Value::as_str) {
        Some("session/title") => Ok(DomainEvent::Title(
            required_string(
                event
                    .get("data")
                    .ok_or(ClientError::Protocol("Session title omitted its payload"))?,
                "title",
                "Session title omitted its title",
            )?
            .to_owned(),
        )),
        Some("turn/start") => Ok(DomainEvent::TurnStart),
        Some("turn/end") => {
            let kind = event
                .pointer("/data/reason/kind")
                .and_then(Value::as_str)
                .ok_or(ClientError::Protocol("Turn end omitted its reason"))?;
            Ok(DomainEvent::TurnEnd(match kind {
                "completed" => ExecutionState::Succeeded,
                "error" => ExecutionState::Failed,
                "aborted" => ExecutionState::Stopped,
                _ => ExecutionState::Unknown,
            }))
        }
        Some("user/message") => Ok(DomainEvent::Message {
            seq: event_seq(event)?,
            surface: surface_op(event)?,
            item: human_message(
                event
                    .get("data")
                    .ok_or(ClientError::Protocol("User message omitted its payload"))?,
            )?,
        }),
        Some("assistant/message") => Ok(DomainEvent::Message {
            seq: event_seq(event)?,
            surface: surface_op(event)?,
            item: assistant_message(event.pointer("/data/message").ok_or(
                ClientError::Protocol("Assistant message omitted its message"),
            )?)?,
        }),
        Some("tool/call") => {
            let data = event
                .get("data")
                .ok_or(ClientError::Protocol("Tool call omitted its payload"))?;
            let call_id = required_string(data, "callId", "Tool call omitted callId")?;
            let call = ToolCallProjection {
                key: ToolCallKey {
                    session: session.clone(),
                    call_id: call_id.to_owned(),
                },
                title: required_string(data, "name", "Tool call omitted its name")?.into(),
                arguments: required_string(data, "arguments", "Tool call omitted its arguments")?
                    .into(),
                cwd: None,
                reason: None,
                execution: ExecutionState::Unknown,
                approval: ApprovalState::NotRequested,
                output: None,
            };
            Ok(DomainEvent::ToolCall {
                seq: event_seq(event)?,
                call,
            })
        }
        Some("tool/result") => parse_tool_result(event),
        Some("approval/asked") => {
            let data = event.get("data").ok_or(ClientError::Protocol(
                "Approval request omitted its payload",
            ))?;
            let Some(call_id) = data.get("callId").and_then(Value::as_str) else {
                return Ok(DomainEvent::Ignored);
            };
            Ok(DomainEvent::ApprovalAsked {
                id: required_string(data, "id", "Approval request omitted its id")?.to_owned(),
                call_id: call_id.to_owned(),
                tool_name: required_string(
                    data,
                    "toolName",
                    "Approval request omitted its tool name",
                )?
                .to_owned(),
                reason: optional_string(data, "reason", "Approval reason was not text")?
                    .map(Into::into),
            })
        }
        Some("approval/decided") => {
            let data = event.get("data").ok_or(ClientError::Protocol(
                "Approval decision omitted its payload",
            ))?;
            Ok(DomainEvent::ApprovalDecided {
                id: required_string(data, "id", "Approval decision omitted its id")?.to_owned(),
                outcome: required_string(data, "outcome", "Approval decision omitted its outcome")?
                    .to_owned(),
            })
        }
        _ => Ok(DomainEvent::Ignored),
    }
}

fn parse_tool_result(event: &Value) -> ClientResult<DomainEvent> {
    let data = event
        .get("data")
        .ok_or(ClientError::Protocol("Tool result omitted its payload"))?;
    let message = data
        .get("message")
        .ok_or(ClientError::Protocol("Tool result omitted its message"))?;
    let call_id = required_string(
        message,
        "toolCallId",
        "Tool result omitted its ToolCall identity",
    )?;
    if message.pointer("/source/callId").and_then(Value::as_str) != Some(call_id) {
        return Err(ClientError::Protocol(
            "Tool result source named a different ToolCall",
        ));
    }
    let is_error = message
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let code = data.pointer("/error/code").and_then(Value::as_str);
    if code.is_some() && !is_error {
        return Err(ClientError::Protocol(
            "Tool result carried an error without isError",
        ));
    }
    let execution = match code {
        Some("TOOL_NOT_STARTED") => ExecutionState::NotExecuted,
        Some("TOOL_OUTCOME_UNKNOWN") => ExecutionState::Unknown,
        _ if is_error => ExecutionState::Failed,
        _ => ExecutionState::Succeeded,
    };
    Ok(DomainEvent::ToolResult {
        seq: event_seq(event)?,
        surface: surface_op(event)?,
        call_id: call_id.to_owned(),
        execution,
        output: display_content(message)?.into(),
    })
}

fn live_request<'a>(
    call: &ToolCallProjection,
    approvals: &'a BTreeMap<String, ApprovalRequest>,
) -> Option<&'a ApprovalRequest> {
    let mut candidates = approvals.values().filter(|request| {
        request.request.get("callId").and_then(Value::as_str) == Some(call.key.call_id.as_str())
            && request.request.get("toolName").and_then(Value::as_str) == Some(call.title.as_ref())
    });
    let request = candidates.next()?;
    candidates.next().is_none().then_some(request)
}

fn surface_op(event: &Value) -> ClientResult<SurfaceOp> {
    match event.get("surfaceOp") {
        Some(Value::String(operation)) if operation == "append" => Ok(SurfaceOp::Append),
        Some(Value::Object(operation))
            if operation.get("op").and_then(Value::as_str) == Some("replace") =>
        {
            let start =
                operation
                    .get("startSeq")
                    .and_then(Value::as_i64)
                    .ok_or(ClientError::Protocol(
                        "Surface replacement omitted startSeq",
                    ))?;
            let end = operation
                .get("endSeq")
                .and_then(Value::as_i64)
                .filter(|end| *end >= start)
                .ok_or(ClientError::Protocol(
                    "Surface replacement had an invalid range",
                ))?;
            Ok(SurfaceOp::Replace { start, end })
        }
        _ => Err(ClientError::Protocol(
            "Surface event omitted a valid surfaceOp",
        )),
    }
}

fn human_message(message: &Value) -> ClientResult<Option<TimelineItem>> {
    if message.pointer("/source/kind").and_then(Value::as_str) != Some("user") {
        return Ok(None);
    }
    let id = required_string(message, "id", "User message omitted its id")?;
    let text = text_content(message)?;
    Ok((!text.is_empty()).then(|| TimelineItem::User {
        id: id.to_owned(),
        text,
    }))
}

fn assistant_message(message: &Value) -> ClientResult<Option<TimelineItem>> {
    let id = required_string(message, "id", "Assistant message omitted its id")?;
    let markdown = text_content(message)?;
    Ok((!markdown.is_empty()).then(|| TimelineItem::Model {
        id: id.to_owned(),
        markdown,
    }))
}

fn text_content(message: &Value) -> ClientResult<SharedText> {
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or(ClientError::Protocol("Message omitted its content blocks"))?;
    if let [block] = content.as_slice()
        && block.get("type").and_then(Value::as_str) == Some("text")
    {
        let part = block
            .get("text")
            .and_then(Value::as_str)
            .ok_or(ClientError::Protocol("Text content block omitted its text"))?;
        // The common cumulative-stream shape needs only the one unavoidable
        // copy into shared UI storage, not a second temporary String copy.
        return Ok(Rc::from(part));
    }
    let mut text = String::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let part = block
            .get("text")
            .and_then(Value::as_str)
            .ok_or(ClientError::Protocol("Text content block omitted its text"))?;
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(part);
    }
    Ok(text.into())
}

fn display_content(message: &Value) -> ClientResult<String> {
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or(ClientError::Protocol(
            "Tool result omitted its content blocks",
        ))?;
    let mut parts = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            parts.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(ClientError::Protocol("Text content block omitted its text"))?
                    .to_owned(),
            );
        } else {
            parts.push(
                serde_json::to_string_pretty(block).map_err(|_| {
                    ClientError::Protocol("Tool result content was not serializable")
                })?,
            );
        }
    }
    Ok(parts.join("\n"))
}

fn snapshot_title(snapshot: &Value, fallback: &str) -> ClientResult<String> {
    match snapshot.pointer("/projections/values/title") {
        Some(Value::String(title)) if !title.is_empty() => Ok(title.clone()),
        Some(Value::Null) => {
            let mut title = None;
            for record in snapshot
                .get("records")
                .and_then(Value::as_array)
                .ok_or(ClientError::Protocol("Session snapshot omitted records"))?
            {
                let event = record.get("event").ok_or(ClientError::Protocol(
                    "Session snapshot record omitted its event",
                ))?;
                if event.get("type").and_then(Value::as_str) == Some("session/title") {
                    title = Some(
                        required_string(
                            event.get("data").ok_or(ClientError::Protocol(
                                "Session title omitted its payload",
                            ))?,
                            "title",
                            "Session title omitted its title",
                        )?
                        .to_owned(),
                    );
                }
            }
            Ok(title.unwrap_or_else(|| fallback.to_owned()))
        }
        None => Ok(fallback.to_owned()),
        _ => Err(ClientError::Protocol(
            "Session title projection was invalid",
        )),
    }
}

fn snapshot_execution(snapshot: &Value) -> ClientResult<ExecutionState> {
    let blank = snapshot.pointer("/projections/values/sessionListMetadata/blank");
    if blank.is_some_and(|blank| !blank.is_boolean()) {
        return Err(ClientError::Protocol(
            "Session blank projection was invalid",
        ));
    }
    let cursor = snapshot
        .get("cursor")
        .and_then(Value::as_i64)
        .ok_or(ClientError::Protocol("Session snapshot omitted its cursor"))?;
    Ok(
        if blank.and_then(Value::as_bool) == Some(true) || cursor == -1 {
            ExecutionState::NotExecuted
        } else {
            ExecutionState::Unknown
        },
    )
}

fn event_seq(event: &Value) -> ClientResult<i64> {
    event
        .get("seq")
        .and_then(Value::as_i64)
        .ok_or(ClientError::Protocol("Session event omitted its sequence"))
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
    message: &'static str,
) -> ClientResult<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ClientError::Protocol(message))
}

fn optional_string(
    value: &Value,
    field: &str,
    message: &'static str,
) -> ClientResult<Option<String>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(ClientError::Protocol(message)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::workbench::identity::{ProjectKey, WorktreeKey};

    fn session_key() -> SessionKey {
        SessionKey::new(
            WorktreeKey::new(ProjectKey::new("host", "project"), "worktree"),
            "session-1",
        )
    }

    fn reducer() -> HostSessionReducer {
        HostSessionReducer::new(session_key(), 1, "client-a")
    }

    fn event(seq: i64, kind: &str, data: Value) -> Value {
        let mut event = json!({ "type": kind, "seq": seq, "time": 100 });
        event["data"] = data;
        event
    }

    fn surface(seq: i64, kind: &str, data: Value) -> Value {
        let mut event = event(seq, kind, data);
        event["surfaceOp"] = json!("append");
        event
    }

    fn user(seq: i64, id: &str, text: &str) -> Value {
        surface(
            seq,
            "user/message",
            json!({
                "id": id,
                "role": "user",
                "content": [{ "type": "text", "text": text }],
                "source": { "kind": "user" },
            }),
        )
    }

    fn assistant(seq: i64, id: &str, text: &str) -> Value {
        surface(
            seq,
            "assistant/message",
            json!({
                "turn": 1,
                "step": 1,
                "message": {
                    "id": id,
                    "role": "assistant",
                    "content": [{ "type": "text", "text": text }],
                    "source": { "kind": "model", "provider": "provider", "model": "model" },
                },
                "stream": [],
            }),
        )
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

    fn tool_result(seq: i64, call_id: &str, text: &str, code: Option<&str>) -> Value {
        let mut data = json!({
            "turn": 1,
            "step": 1,
            "message": {
                "id": format!("result-{call_id}-{seq}"),
                "role": "tool",
                "toolCallId": call_id,
                "source": { "kind": "tool", "callId": call_id },
                "content": [{ "type": "text", "text": text }],
                "isError": code.is_some(),
            },
        });
        if let Some(code) = code {
            data["error"] = json!({ "name": "Recovery", "code": code });
        }
        surface(seq, "tool/result", data)
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

    fn approval_decided(seq: i64, id: &str, outcome: &str) -> Value {
        event(
            seq,
            "approval/decided",
            json!({ "id": id, "outcome": outcome }),
        )
    }

    fn title(seq: i64, value: &str) -> Value {
        event(
            seq,
            "session/title",
            json!({
                "title": value,
                "messageSeqs": [],
                "source": { "kind": "user" },
            }),
        )
    }

    fn turn_start(seq: i64) -> Value {
        event(seq, "turn/start", json!({ "turn": 1 }))
    }

    fn turn_end(seq: i64, kind: &str) -> Value {
        event(
            seq,
            "turn/end",
            json!({ "turn": 1, "reason": { "kind": kind } }),
        )
    }

    fn snapshot(
        events: &[Value],
        cursor: i64,
        title_value: Option<&str>,
        blank: bool,
        has_more: bool,
    ) -> Value {
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
            "hasMore": has_more,
            "records": events.iter().map(|event| json!({ "type": "event", "event": event })).collect::<Vec<_>>(),
            "projections": {
                "asOfSeq": cursor,
                "values": {
                    "title": title_value,
                    "sessionListMetadata": { "blank": blank, "lastPromptAt": null },
                },
            },
            "assistantStream": { "revision": 0 },
        })
    }

    fn frame(event: Value) -> Value {
        let mut frame = json!({ "type": "event" });
        frame["event"] = event;
        frame
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

    fn tool<'a>(projection: &'a SessionProjection, call_id: &str) -> &'a ToolCallProjection {
        projection
            .items
            .iter()
            .find_map(|item| match item {
                TimelineItem::Tool(call) if call.key.call_id == call_id => Some(call),
                _ => None,
            })
            .expect("projected ToolCall")
    }

    fn ids(projection: &SessionProjection) -> Vec<String> {
        projection
            .items
            .iter()
            .map(TimelineItem::stable_id)
            .collect()
    }

    #[test]
    fn streaming_projections_share_unchanged_text_and_replace_changed_text() {
        let long = "x".repeat(16 * 1024);
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(
                &[
                    assistant(0, "stable", &long),
                    assistant(1, "stream", "part"),
                ],
                1,
                None,
                false,
                false,
            ))
            .expect("opening snapshot");
        let first = reducer.projection();
        let TimelineItem::Model {
            markdown: stable, ..
        } = &first.items[0]
        else {
            panic!("stable model fixture");
        };
        let stable = Rc::clone(stable);
        let TimelineItem::Model {
            markdown: changed_before,
            ..
        } = &first.items[1]
        else {
            panic!("streaming model fixture");
        };
        let changed_before = Rc::clone(changed_before);

        let mut latest = None;
        for seq in 2..20 {
            reducer
                .apply(&frame(assistant(seq, "stream", &long)))
                .expect("streaming update");
            let projection = reducer.projection();
            let TimelineItem::Model { markdown, .. } = &projection.items[0] else {
                panic!("stable model remains first");
            };
            assert!(Rc::ptr_eq(&stable, markdown));
            latest = Some(projection);
        }
        let latest = latest.expect("streamed projection");
        let TimelineItem::Model { markdown, .. } = &latest.items[1] else {
            panic!("changed model remains second");
        };
        assert!(!Rc::ptr_eq(&changed_before, markdown));
        assert_eq!(markdown.as_ref(), long);
    }

    #[test]
    fn durable_frames_drive_messages_approvals_tools_and_execution() {
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(&[], -1, None, true, false))
            .unwrap();
        assert_eq!(reducer.projection().title, "session-1");
        assert_eq!(reducer.projection().execution, ExecutionState::NotExecuted);

        for event in [
            turn_start(0),
            user(1, "user-1", "请检查当前目录"),
            assistant(2, "assistant-1", "我会先确认目录。"),
            tool_call(3, "call-1"),
        ] {
            reducer.apply(&frame(event)).unwrap();
        }
        reducer
            .apply_host_event(live_approval("client-a", "event-1", "call-1"))
            .unwrap();
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::NotRequested
        );

        reducer
            .apply(&frame(approval_asked(4, "approval-1", "call-1")))
            .unwrap();
        let pending = reducer.projection();
        assert_eq!(pending.execution, ExecutionState::Running);
        assert_eq!(tool(&pending, "call-1").approval, ApprovalState::Pending);
        assert_eq!(
            tool(&pending, "call-1").reason.as_deref(),
            Some("Verify the working directory")
        );
        assert_eq!(
            reducer
                .approval_request(&tool(&pending, "call-1").key, 1)
                .unwrap()
                .event_id,
            "event-1"
        );

        reducer
            .apply(&frame(approval_decided(5, "approval-1", "allowed-once")))
            .unwrap();
        reducer
            .apply(&frame(tool_result(6, "call-1", "/workspace", None)))
            .unwrap();
        reducer.apply(&frame(turn_end(7, "completed"))).unwrap();

        let projection = reducer.projection();
        assert_eq!(projection.execution, ExecutionState::Succeeded);
        assert_eq!(
            ids(&projection),
            ["user:user-1", "model:assistant-1", "tool:call-1"]
        );
        let call = tool(&projection, "call-1");
        assert_eq!(call.key.session, session_key());
        assert_eq!(
            call.approval,
            ApprovalState::Decided(ApprovalDecision::AllowOnce)
        );
        assert_eq!(call.execution, ExecutionState::Succeeded);
        assert_eq!(call.output.as_deref(), Some("/workspace"));
        assert_eq!(call.cwd, None);
        assert!(reducer.approval_request(&call.key, 1).is_none());
        assert_eq!(projection.models, Vec::new());
        assert_eq!(projection.queue, Vec::new());
        assert_eq!(projection.stats, StatsInput::default());
    }

    #[test]
    fn reconnect_replaces_the_window_and_malformed_append_is_atomic() {
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(
                &[
                    user(0, "old-user", "old"),
                    assistant(1, "old-model", "answer"),
                ],
                1,
                Some("Old"),
                false,
                false,
            ))
            .unwrap();
        reducer.apply(&frame(title(2, "Live title"))).unwrap();
        reducer.apply(&frame(title(2, "Live title"))).unwrap();
        let before_gap = reducer.projection();
        assert!(reducer.apply(&frame(user(4, "gap", "gap"))).is_err());
        assert_eq!(reducer.projection(), before_gap);
        let mut malformed = user(3, "malformed", "malformed");
        malformed["data"]["content"][0]["text"] = json!(42);
        assert!(reducer.apply(&frame(malformed)).is_err());
        assert_eq!(reducer.projection(), before_gap);
        reducer.apply(&frame(user(3, "new-user", "new"))).unwrap();
        assert_eq!(reducer.projection().items.len(), 3);

        reducer
            .apply(&snapshot(
                &[user(3, "new-user", "new"), title(4, "Stale event title")],
                4,
                Some("Replacement"),
                false,
                true,
            ))
            .unwrap();
        assert_eq!(reducer.projection().title, "Replacement");
        assert_eq!(ids(&reducer.projection()), ["user:new-user"]);
    }

    #[test]
    fn a_new_events_generation_invalidates_old_approval_closures() {
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(
                &[
                    tool_call(0, "call-1"),
                    approval_asked(1, "approval-1", "call-1"),
                ],
                1,
                None,
                false,
                false,
            ))
            .unwrap();
        reducer
            .apply_host_event(live_approval("client-a", "event-1", "call-1"))
            .unwrap();
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::Pending
        );

        reducer.replace_generation(2, "client-b");
        assert_eq!(reducer.projection().generation, 2);
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::Stale
        );
        assert!(
            !reducer
                .apply_host_event(live_approval("client-a", "event-1", "call-1"))
                .unwrap()
        );
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::Stale
        );

        reducer
            .apply_host_event(live_approval("client-b", "event-1", "call-1"))
            .unwrap();
        let pending = reducer.projection();
        let key = &tool(&pending, "call-1").key;
        assert!(reducer.approval_request(key, 1).is_none());
        assert_eq!(
            reducer.approval_request(key, 2).unwrap().client_id,
            "client-b"
        );
        reducer.apply(&frame(title(2, "Still current"))).unwrap();
        assert_eq!(reducer.projection().generation, 2);
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::Pending
        );

        reducer
            .apply_host_event(HostEvent::Cancelled {
                event_id: "event-1".to_owned(),
            })
            .unwrap();
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::Stale
        );
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::Stale
        );
    }

    #[test]
    fn multiple_durable_approval_generations_fail_closed_without_durable_ids() {
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(
                &[
                    tool_call(0, "call-1"),
                    approval_asked(1, "approval-a", "call-1"),
                    approval_asked(2, "approval-b", "call-1"),
                ],
                2,
                None,
                false,
                false,
            ))
            .unwrap();
        reducer
            .apply_host_event(live_approval("client-a", "event-b", "call-1"))
            .unwrap();
        reducer
            .apply(&frame(approval_decided(3, "approval-a", "rejected")))
            .unwrap();
        let ambiguous = reducer.projection();
        assert_eq!(tool(&ambiguous, "call-1").approval, ApprovalState::Stale);
        assert!(
            reducer
                .approval_request(&tool(&ambiguous, "call-1").key, 1)
                .is_none()
        );
        reducer
            .apply(&frame(approval_decided(4, "approval-b", "rejected")))
            .unwrap();
        assert_eq!(
            tool(&reducer.projection(), "call-1").approval,
            ApprovalState::Decided(ApprovalDecision::Reject)
        );
    }

    #[test]
    fn ambiguous_live_requests_fail_closed_until_one_is_cancelled() {
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(
                &[
                    tool_call(0, "call-1"),
                    approval_asked(1, "approval-1", "call-1"),
                ],
                1,
                None,
                false,
                false,
            ))
            .unwrap();
        reducer
            .apply_host_event(live_approval("client-a", "event-1", "call-1"))
            .unwrap();
        reducer
            .apply_host_event(live_approval("client-a", "event-2", "call-1"))
            .unwrap();
        let ambiguous = reducer.projection();
        let key = &tool(&ambiguous, "call-1").key;
        assert_eq!(tool(&ambiguous, "call-1").approval, ApprovalState::Stale);
        assert!(reducer.approval_request(key, 1).is_none());

        reducer
            .apply_host_event(HostEvent::Cancelled {
                event_id: "event-1".to_owned(),
            })
            .unwrap();
        let pending = reducer.projection();
        assert_eq!(tool(&pending, "call-1").approval, ApprovalState::Pending);
        assert_eq!(
            reducer
                .approval_request(&tool(&pending, "call-1").key, 1)
                .unwrap()
                .event_id,
            "event-2"
        );
    }

    #[test]
    fn approvals_without_a_tool_call_do_not_fabricate_a_card() {
        let asked = event(
            1,
            "approval/asked",
            json!({
                "id": "approval-1",
                "toolName": "bash",
                "reason": "A Host-level question",
            }),
        );
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(
                &[user(0, "user-1", "prompt"), asked],
                1,
                None,
                false,
                false,
            ))
            .unwrap();
        reducer
            .apply_host_event(HostEvent::Approval(ApprovalRequest {
                client_id: "client-a".to_owned(),
                event_id: "event-1".to_owned(),
                agent_id: "session-1".to_owned(),
                request: json!({ "toolName": "bash", "reason": "A Host-level question" }),
            }))
            .unwrap();
        assert_eq!(ids(&reducer.projection()), ["user:user-1"]);
    }

    #[test]
    fn recovery_outcomes_do_not_turn_unknown_or_unstarted_tools_into_failures() {
        let events = [
            tool_call(0, "failed"),
            tool_result(1, "failed", "failed", Some("COMMAND_FAILED")),
            tool_call(2, "not-started"),
            tool_result(3, "not-started", "not started", Some("TOOL_NOT_STARTED")),
            tool_call(4, "unknown"),
            tool_result(5, "unknown", "unknown", Some("TOOL_OUTCOME_UNKNOWN")),
            tool_call(6, "succeeded"),
            tool_result(7, "succeeded", "done", None),
            turn_end(8, "aborted"),
        ];
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(&events, 8, None, false, false))
            .unwrap();
        let projection = reducer.projection();
        assert_eq!(projection.execution, ExecutionState::Stopped);
        assert_eq!(
            tool(&projection, "failed").execution,
            ExecutionState::Failed
        );
        assert_eq!(
            tool(&projection, "not-started").execution,
            ExecutionState::NotExecuted
        );
        assert_eq!(
            tool(&projection, "unknown").execution,
            ExecutionState::Unknown
        );
        assert_eq!(
            tool(&projection, "succeeded").execution,
            ExecutionState::Succeeded
        );
    }

    #[test]
    fn unavailable_approval_is_stale_and_surface_replacement_drops_stale_output() {
        let mut replacement = assistant(4, "replacement", "replacement");
        replacement["surfaceOp"] = json!({ "op": "replace", "startSeq": 0, "endSeq": 1 });
        let mut result_replacement = assistant(5, "result-replacement", "result replaced");
        result_replacement["surfaceOp"] = json!({ "op": "replace", "startSeq": 3, "endSeq": 3 });
        let events = [
            user(0, "old-user", "old"),
            assistant(1, "old-model", "old"),
            tool_call(2, "call-1"),
            tool_result(3, "call-1", "old output", None),
            replacement,
            result_replacement,
            approval_asked(6, "approval-1", "call-1"),
            approval_decided(7, "approval-1", "unavailable"),
            turn_end(8, "interrupted"),
        ];
        let mut reducer = reducer();
        reducer
            .apply(&snapshot(&events, 8, None, false, false))
            .unwrap();
        reducer
            .apply_host_event(live_approval("client-a", "event-1", "call-1"))
            .unwrap();
        let projection = reducer.projection();
        assert_eq!(projection.execution, ExecutionState::Unknown);
        assert_eq!(
            ids(&projection),
            [
                "model:replacement",
                "tool:call-1",
                "model:result-replacement"
            ]
        );
        let call = tool(&projection, "call-1");
        assert_eq!(call.approval, ApprovalState::Stale);
        assert_eq!(call.execution, ExecutionState::Unknown);
        assert_eq!(call.output, None);
        assert!(reducer.approval_request(&call.key, 1).is_none());
        assert!(projection.items.iter().any(|item| matches!(
            item,
            TimelineItem::Model { id, markdown } if id == "replacement" && markdown.as_ref() == "replacement"
        )));
        assert!(projection.items.iter().any(|item| matches!(
            item,
            TimelineItem::Model { id, markdown } if id == "result-replacement" && markdown.as_ref() == "result replaced"
        )));
    }

    #[test]
    fn approval_state_stays_bounded_and_capacity_overflow_fails_closed() {
        let mut audits = reducer();
        audits
            .apply(&snapshot(&[tool_call(0, "call-1")], 0, None, false, false))
            .unwrap();
        for seq in 1..=250 {
            audits
                .apply(&frame(approval_asked(
                    seq,
                    &format!("approval-{seq}"),
                    "call-1",
                )))
                .unwrap();
        }
        assert_eq!(audits.state.audits.len(), 1);
        assert_eq!(
            tool(&audits.projection(), "call-1").approval,
            ApprovalState::Stale
        );

        let mut live = reducer();
        live.apply(&snapshot(
            &[
                tool_call(0, "call-1"),
                approval_asked(1, "approval-1", "call-1"),
            ],
            1,
            None,
            false,
            false,
        ))
        .unwrap();
        live.apply_host_event(live_approval("client-a", "current", "call-1"))
            .unwrap();
        assert_eq!(
            tool(&live.projection(), "call-1").approval,
            ApprovalState::Pending
        );
        for index in 0..(LIVE_APPROVAL_LIMIT - 1) {
            live.apply_host_event(live_approval(
                "client-a",
                &format!("event-{index}"),
                &format!("call-{index}"),
            ))
            .unwrap();
        }
        assert_eq!(live.live_approvals.len(), LIVE_APPROVAL_LIMIT);
        assert!(
            live.apply_host_event(live_approval("client-a", "overflow", "overflow-call"))
                .is_err()
        );
        assert!(live.live_approvals.is_empty());
        assert!(live.approval_overflow);
        let overflowed = live.projection();
        let overflowed_call = tool(&overflowed, "call-1");
        assert_eq!(overflowed_call.approval, ApprovalState::Stale);
        assert!(live.approval_request(&overflowed_call.key, 1).is_none());
        assert!(
            !live
                .apply_host_event(live_approval("client-a", "later", "later-call"))
                .unwrap()
        );
        live.replace_generation(2, "client-b");
        assert!(!live.approval_overflow);
        live.apply_host_event(live_approval("client-b", "current-new", "call-1"))
            .unwrap();
        let recovered = live.projection();
        let recovered_call = tool(&recovered, "call-1");
        assert_eq!(recovered_call.approval, ApprovalState::Pending);
        assert_eq!(
            live.approval_request(&recovered_call.key, 2)
                .unwrap()
                .event_id,
            "current-new"
        );
    }

    #[test]
    fn visible_items_stay_bounded_for_message_and_tool_only_streams() {
        let mut messages = reducer();
        messages
            .apply(&snapshot(&[], -1, None, true, false))
            .unwrap();
        for seq in 0..250 {
            messages
                .apply(&frame(user(seq, &format!("user-{seq}"), "text")))
                .unwrap();
        }
        assert_eq!(messages.state.items.len(), VISIBLE_ITEM_LIMIT);
        assert_eq!(ids(&messages.projection()).first().unwrap(), "user:user-50");
        assert_eq!(ids(&messages.projection()).last().unwrap(), "user:user-249");

        let mut tools = reducer();
        tools.apply(&snapshot(&[], -1, None, true, false)).unwrap();
        for seq in 0..250 {
            tools
                .apply(&frame(tool_call(seq, &format!("call-{seq}"))))
                .unwrap();
        }
        assert_eq!(tools.state.items.len(), VISIBLE_ITEM_LIMIT);
        assert_eq!(ids(&tools.projection()).first().unwrap(), "tool:call-50");
        assert_eq!(ids(&tools.projection()).last().unwrap(), "tool:call-249");
    }
}
