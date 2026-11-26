//! Minimal asynchronous client for the public dsh Web protocol.
//!
//! Authentication material is deliberately absent from [`ClientError`]. HTTP
//! and WebSocket implementation errors are reduced to stable operations because
//! their displays can contain request URLs.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::{
    StatusCode, Url,
    header::{CONTENT_TYPE, COOKIE, SET_COOKIE},
    redirect::Policy,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{
        Error as WebSocketError, Message, client::IntoClientRequest as _, http::HeaderValue,
        protocol::WebSocketConfig,
    },
};

const MAX_RPC_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MUX_COMMAND_CAPACITY: usize = 64;

/// Deadlines used by one client. Long-lived stream reads have no implicit deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DshClientConfig {
    /// Authentication and unary RPC deadline. For `wait`, the server wait duration is added.
    pub request_timeout: Duration,
    /// WebSocket connection and logical-stream opening deadline.
    pub stream_open_timeout: Duration,
}

impl Default for DshClientConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(15),
            stream_open_timeout: Duration::from_secs(10),
        }
    }
}

/// Branchable dsh client failures, with authentication material excluded.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ClientError {
    /// The supplied launch URL is not a loopback HTTP `/connect` URL.
    /// Allowed hosts are `127.0.0.1`, `::1`, and `localhost`.
    #[error("invalid dsh authentication URL")]
    InvalidAuthUrl,
    /// The launch token was rejected or the Host returned another non-302 response.
    #[error("dsh authentication failed with HTTP status {status}")]
    Authentication { status: StatusCode },
    /// The authentication response had no usable opaque cookie.
    #[error("dsh authentication response omitted a valid Set-Cookie header")]
    MissingCookie,
    /// An operation exceeded a caller-provided deadline.
    #[error("dsh {operation} timed out after {timeout:?}")]
    Timeout {
        operation: &'static str,
        timeout: Duration,
    },
    /// A transport operation failed. The source is intentionally not exposed.
    #[error("dsh {operation} failed")]
    Transport { operation: &'static str },
    /// An RPC endpoint returned a non-success HTTP status.
    #[error("dsh RPC request returned HTTP status {status}")]
    HttpStatus { status: StatusCode },
    /// The Host violated the public envelope or stream protocol.
    #[error("dsh protocol violation: {0}")]
    Protocol(&'static str),
    /// A unary method or logical stream returned `ok: false` / `error`.
    #[error("dsh Remote error {code}: {message}")]
    Remote {
        code: String,
        message: String,
        details: Value,
    },
    /// A unary response exceeded the client's byte limit.
    #[error("dsh {operation} exceeded the {limit}-byte response limit")]
    ResponseTooLarge {
        operation: &'static str,
        limit: usize,
    },
    /// A logical stream exceeded its bounded downstream queue.
    #[error("dsh logical stream queue overflowed")]
    StreamOverflow,
    /// The bounded mux command queue cannot accept another control operation.
    #[error("dsh WebSocket command queue is full")]
    CommandQueueFull,
    /// The physical connection closed while an operation was pending.
    #[error("dsh connection is closed")]
    ConnectionClosed,
}

/// Convenience alias for dsh client operations.
pub type ClientResult<T> = std::result::Result<T, ClientError>;

/// A project known to the Host, including a bounded recent Session summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    /// Host-assigned project identity.
    pub project_id: String,
    /// Project path on the Host.
    pub path: String,
    /// Human-readable project title.
    pub title: String,
    /// Most recent Sessions associated with the project.
    pub recent_session_ids: Vec<String>,
    /// Whether older associated Sessions exist beyond this summary.
    pub has_more_sessions: bool,
}

/// One approval delivered by the answering `$events` generation.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    pub client_id: String,
    /// Opaque correlation ID delivered at most once within this client generation.
    pub event_id: String,
    pub agent_id: String,
    /// Original Host approval request after transport-only Agent/signal removal.
    pub request: Value,
}

/// Decisions supported by the minimal client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
}

impl ApprovalOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::AllowedOnce => "allowed-once",
            Self::Rejected => "rejected",
        }
    }
}

/// One item from the automatically opened `$events` stream.
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    Emit {
        event: String,
        args: Value,
    },
    Approval(ApprovalRequest),
    /// A non-approval waterfall, retained as raw JSON for the caller.
    Waterfall {
        event: String,
        event_id: String,
        agent_id: String,
        request: Value,
    },
    Cancelled {
        event_id: String,
    },
}

/// One logical Remote stream. Items are endpoint-owned JSON values, without mux framing.
/// Each downstream queue is bounded; overflow fails only this logical stream.
pub struct DshStream {
    id: String,
    opening: Option<Value>,
    receiver: mpsc::Receiver<StreamMessage>,
    commands: mpsc::Sender<MuxCommand>,
    shared: Arc<MuxShared>,
    overflowed: Arc<AtomicBool>,
    active: bool,
}

impl DshStream {
    /// Read the next raw item. `Ok(None)` is a Host `end` frame.
    pub async fn next(&mut self) -> ClientResult<Option<Value>> {
        if let Some(opening) = self.opening.take() {
            return Ok(Some(opening));
        }
        match self.receiver.recv().await {
            Some(StreamMessage::Item(value)) => Ok(Some(value)),
            Some(StreamMessage::End) => {
                self.active = false;
                Ok(None)
            }
            Some(StreamMessage::Failed(error)) => {
                self.active = false;
                Err(error)
            }
            None => {
                self.active = false;
                if self.overflowed.load(Ordering::Acquire) {
                    return Err(ClientError::StreamOverflow);
                }
                Err(self
                    .shared
                    .terminal_failure()
                    .unwrap_or(ClientError::ConnectionClosed))
            }
        }
    }

    /// Cancel this logical stream. Dropping the handle has the same effect.
    pub fn cancel(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // Wake a mux send blocked on a full queue; cancellation must not require more reads.
        self.receiver.close();
        lock_streams(&self.shared).remove(&self.id);
        cancel_or_stop(&self.commands, &self.shared, self.id.clone());
    }
}

impl Drop for DshStream {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Authenticated public dsh client sharing one WebSocket across logical streams.
pub struct DshClient {
    http: reqwest::Client,
    base_url: Url,
    cookie: Cookie,
    sensitive: Arc<Sensitive>,
    config: DshClientConfig,
    next_id: AtomicU64,
    commands: mpsc::Sender<MuxCommand>,
    shared: Arc<MuxShared>,
    events: DshStream,
    client_id: String,
    task: Option<JoinHandle<()>>,
}

impl DshClient {
    /// Exchange `runtime.auth_url()` for an API cookie and wait for `$events` ready.
    /// Redirects are never followed and `interactions=answer` is always explicit.
    pub async fn connect(auth_url: &str, config: DshClientConfig) -> ClientResult<Self> {
        let (http, base_url, cookie, sensitive) = authenticate(auth_url, config).await?;
        let mut ws_url = base_url
            .join("/api/remote.mux")
            .map_err(|_| ClientError::InvalidAuthUrl)?;
        ws_url
            .set_scheme("ws")
            .map_err(|()| ClientError::InvalidAuthUrl)?;
        let mut request =
            ws_url
                .as_str()
                .into_client_request()
                .map_err(|_| ClientError::Transport {
                    operation: "WebSocket request construction",
                })?;
        let cookie_header = HeaderValue::from_str(&cookie.0)
            .map_err(|_| ClientError::Protocol("authentication cookie is not a valid header"))?;
        request.headers_mut().insert(COOKIE, cookie_header);
        let socket_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WEBSOCKET_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WEBSOCKET_MESSAGE_BYTES));
        let (socket, _) = timeout(
            config.stream_open_timeout,
            connect_async_with_config(request, Some(socket_config), false),
        )
        .await
        .map_err(|_| ClientError::Timeout {
            operation: "WebSocket opening",
            timeout: config.stream_open_timeout,
        })?
        .map_err(|_| ClientError::Transport {
            operation: "WebSocket opening",
        })?;

        let (command_sender, command_receiver) = mpsc::channel(MUX_COMMAND_CAPACITY);
        let shared = Arc::new(MuxShared::new());
        let task = tokio::spawn(run_mux(
            socket,
            command_receiver,
            config.stream_open_timeout,
            Arc::clone(&shared),
            Arc::clone(&sensitive),
        ));
        let next_id = AtomicU64::new(0);
        let mut events = match open_mux_stream(
            &command_sender,
            &shared,
            &next_id,
            config,
            "$events",
            json!({}),
        )
        .await
        {
            Ok(stream) => stream,
            Err(error) => {
                task.abort();
                return Err(error);
            }
        };
        let ready = events.opening.take().ok_or(ClientError::Protocol(
            "event stream opening item is missing",
        ))?;
        let client_id = match parse_ready_client_id(&ready) {
            Ok(id) => id,
            Err(error) => {
                task.abort();
                return Err(error);
            }
        };

        Ok(Self {
            http,
            base_url,
            cookie,
            sensitive,
            config,
            next_id,
            commands: command_sender,
            shared,
            events,
            client_id,
            task: Some(task),
        })
    }

    /// Opaque identity of this `$events` generation.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Create or reuse a Session attached to `project_id`.
    pub async fn create_session(&self, project_id: &str) -> ClientResult<String> {
        let value = self
            .rpc(
                "session/create",
                json!({ "request": { "workspaceId": project_id } }),
            )
            .await?;
        parse_session_id(&value)
    }

    /// Open the project at `path` and return its current Host metadata.
    pub async fn open_project(&self, path: &str) -> ClientResult<Project> {
        let value = self
            .rpc("project/open", json!({ "request": { "path": path } }))
            .await?;
        parse_project(&value)
    }

    /// Read one project's current metadata and recent Session summary.
    pub async fn get_project(&self, project_id: &str) -> ClientResult<Project> {
        let value = self
            .rpc(
                "project/get",
                json!({ "request": { "projectId": project_id } }),
            )
            .await?;
        parse_project(&value)
    }

    /// Read one UTF-8 file from a Host project and return its content.
    pub async fn read_project_file(&self, project_id: &str, path: &str) -> ClientResult<String> {
        let value = self
            .rpc(
                "project/read",
                json!({ "request": { "projectId": project_id, "path": path } }),
            )
            .await?;
        required_text(&value, "content", "project/read response omitted content")
    }

    /// Replace one existing UTF-8 file in a Host project.
    pub async fn write_project_file(
        &self,
        project_id: &str,
        path: &str,
        content: &str,
    ) -> ClientResult<()> {
        self.rpc(
            "project/write",
            json!({ "request": { "projectId": project_id, "path": path, "content": content } }),
        )
        .await?;
        Ok(())
    }

    /// Return the project's UTF-8 porcelain-v1, NUL-delimited Git status.
    pub async fn git_status(&self, project_id: &str) -> ClientResult<String> {
        let value = self
            .rpc(
                "git/status",
                json!({ "request": { "projectId": project_id } }),
            )
            .await?;
        required_text(&value, "status", "git/status response omitted status")
    }

    /// Return the project's UTF-8 Git diff patch.
    pub async fn git_diff(&self, project_id: &str) -> ClientResult<String> {
        let value = self
            .rpc(
                "git/diff",
                json!({ "request": { "projectId": project_id } }),
            )
            .await?;
        required_text(&value, "patch", "git/diff response omitted patch")
    }

    /// Queue a text prompt. The caller owns `request_id`; retries can reuse it unchanged.
    /// Every invocation still receives a fresh outer RPC correlation id.
    ///
    /// Success means only that the Host admitted the prompt to the Agent inbox, not that
    /// model execution, tools, or persistence completed.
    pub async fn prompt(&self, session_id: &str, request_id: &str, text: &str) -> ClientResult<()> {
        let value = self
            .rpc("session/prompt", prompt_args(session_id, request_id, text))
            .await?;
        expect_accepted(&value, "session/prompt response was not accepted")
    }

    /// Rename a Session and wait for the Host to commit the title event.
    pub async fn rename_session(&self, session_id: &str, title: &str) -> ClientResult<()> {
        self.rpc(
            "session/rename",
            json!({ "request": { "sessionId": session_id, "title": title } }),
        )
        .await?;
        Ok(())
    }

    /// Request cancellation of the active turn.
    ///
    /// Success means cancellation was accepted by the live Agent, not that the turn has
    /// already stopped or that its final durable state has been written.
    pub async fn cancel(&self, session_id: &str) -> ClientResult<()> {
        let value = self
            .rpc(
                "session/cancel",
                json!({ "request": { "sessionId": session_id } }),
            )
            .await?;
        expect_accepted(&value, "session/cancel response was not accepted")
    }

    /// Read the current passive Session state as raw JSON.
    pub async fn state(&self, session_id: &str) -> ClientResult<Value> {
        self.rpc(
            "session/state",
            json!({ "request": { "sessionId": session_id } }),
        )
        .await
    }

    /// Read an older durable Session page ending at inclusive `through_seq`.
    /// `before_seq` excludes records at or above that sequence from the page.
    pub async fn session_page(
        &self,
        session_id: &str,
        through_seq: i64,
        before_seq: Option<i64>,
    ) -> ClientResult<Value> {
        let mut request = json!({
            "address": { "kind": "session", "sessionId": session_id },
            "throughSeq": through_seq,
        });
        if let Some(before_seq) = before_seq {
            request
                .as_object_mut()
                .ok_or(ClientError::Protocol(
                    "failed to build Session page request",
                ))?
                .insert("beforeSeq".to_owned(), json!(before_seq));
        }
        self.rpc("session/page", json!({ "request": request }))
            .await
    }

    /// Wait for idle, required input, or the server-side deadline and return raw JSON.
    pub async fn wait(&self, session_id: &str, wait_timeout: Duration) -> ClientResult<Value> {
        let timeout_ms = u64::try_from(wait_timeout.as_millis()).unwrap_or(u64::MAX);
        self.rpc_with_timeout(
            "session/wait",
            json!({ "request": { "sessionId": session_id, "timeoutMs": timeout_ms } }),
            self.config.request_timeout.saturating_add(wait_timeout),
        )
        .await
    }

    /// Open `session/follow` with activation and assistant streaming enabled.
    /// The first item is guaranteed to be a `snapshot`; later items retain `event` and
    /// `assistant-stream` JSON unchanged.
    pub async fn follow(&self, session_id: &str) -> ClientResult<DshStream> {
        self.follow_with_options(session_id, None).await
    }

    /// Open a follow whose snapshot budgets at most `max_messages` messages.
    /// Non-message records ride along with that window and are not separately capped.
    pub async fn follow_with_limit(
        &self,
        session_id: &str,
        max_messages: u64,
    ) -> ClientResult<DshStream> {
        if max_messages == 0 {
            return Err(ClientError::Protocol(
                "Session follow message limit must be positive",
            ));
        }
        self.follow_with_options(session_id, Some(max_messages))
            .await
    }

    async fn follow_with_options(
        &self,
        session_id: &str,
        max_messages: Option<u64>,
    ) -> ClientResult<DshStream> {
        let mut request = json!({
            "address": { "kind": "session", "sessionId": session_id },
            "activate": true,
            "assistantStream": true,
        });
        if let Some(max_messages) = max_messages {
            request
                .as_object_mut()
                .ok_or(ClientError::Protocol(
                    "failed to build Session follow request",
                ))?
                .insert("maxMessages".to_owned(), json!(max_messages));
        }
        let stream = self
            .open_stream("session/follow", json!({ "request": request }))
            .await?;
        if stream
            .opening
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            != Some("snapshot")
        {
            return Err(ClientError::Protocol(
                "session/follow did not open with a snapshot",
            ));
        }
        Ok(stream)
    }

    /// Open a Remote stream with `payload.args` and wait for its first item.
    async fn open_stream(&self, endpoint: &str, args: Value) -> ClientResult<DshStream> {
        if endpoint.is_empty() {
            return Err(ClientError::Protocol("stream endpoint is empty"));
        }
        open_mux_stream(
            &self.commands,
            &self.shared,
            &self.next_id,
            self.config,
            endpoint,
            args,
        )
        .await
    }

    /// Read and parse one forwarded Host event.
    pub async fn next_event(&mut self) -> ClientResult<HostEvent> {
        let value = self
            .events
            .next()
            .await?
            .ok_or(ClientError::ConnectionClosed)?;
        parse_host_event(&value, &self.client_id)
    }

    /// Answer one approval through the `$events/result` unary endpoint.
    pub async fn answer(
        &self,
        approval: &ApprovalRequest,
        outcome: ApprovalOutcome,
    ) -> ClientResult<()> {
        if approval.client_id != self.client_id {
            return Err(ClientError::Protocol(
                "approval belongs to a different event generation",
            ));
        }
        self.rpc("$events/result", approval_args(approval, outcome))
            .await?;
        Ok(())
    }

    /// Close the physical socket and wait for its task and logical streams to finish.
    pub async fn close(mut self) -> ClientResult<()> {
        self.events.cancel();
        if timeout(
            self.config.request_timeout,
            self.commands.send(MuxCommand::Close),
        )
        .await
        .is_err()
        {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            return Err(ClientError::Timeout {
                operation: "WebSocket shutdown",
                timeout: self.config.request_timeout,
            });
        }
        let mut task = self.task.take().ok_or(ClientError::ConnectionClosed)?;
        match timeout(self.config.request_timeout, &mut task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(ClientError::Transport {
                operation: "WebSocket shutdown",
            }),
            Err(_) => {
                task.abort();
                Err(ClientError::Timeout {
                    operation: "WebSocket shutdown",
                    timeout: self.config.request_timeout,
                })
            }
        }
    }

    async fn rpc(&self, method: &str, args: Value) -> ClientResult<Value> {
        self.rpc_with_timeout(method, args, self.config.request_timeout)
            .await
    }

    async fn rpc_with_timeout(
        &self,
        method: &str,
        args: Value,
        deadline: Duration,
    ) -> ClientResult<Value> {
        let rpc_id = next_wire_id(&self.next_id, "rpc");
        let url = self
            .base_url
            .join(&format!("/api/{method}"))
            .map_err(|_| ClientError::Protocol("invalid RPC method"))?;
        let body = serde_json::to_vec(&rpc_envelope(&rpc_id, method, &args)).map_err(|_| {
            ClientError::Transport {
                operation: "RPC request serialization",
            }
        })?;
        let request = self
            .http
            .post(url)
            .header(COOKIE, self.cookie.0.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        timeout(deadline, async {
            let response = request.send().await.map_err(|_| ClientError::Transport {
                operation: "RPC request",
            })?;
            if response.status() != StatusCode::OK {
                return Err(ClientError::HttpStatus {
                    status: response.status(),
                });
            }
            let body = read_rpc_body(response).await?;
            parse_rpc_response(&body, &rpc_id, &self.sensitive)
        })
        .await
        .map_err(|_| ClientError::Timeout {
            operation: "RPC request",
            timeout: deadline,
        })?
    }
}

async fn read_rpc_body(response: reqwest::Response) -> ClientResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RPC_RESPONSE_BYTES as u64)
    {
        return Err(ClientError::ResponseTooLarge {
            operation: "RPC response",
            limit: MAX_RPC_RESPONSE_BYTES,
        });
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| ClientError::Transport {
            operation: "RPC response read",
        })?;
        if chunk.len() > MAX_RPC_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(ClientError::ResponseTooLarge {
                operation: "RPC response",
                limit: MAX_RPC_RESPONSE_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl Drop for DshClient {
    fn drop(&mut self) {
        self.shared.terminate(ClientError::ConnectionClosed);
        let _ = self.commands.try_send(MuxCommand::Close);
        // Drop cannot await a graceful close; aborting still drops the socket and all senders.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn authenticate(
    auth_url: &str,
    config: DshClientConfig,
) -> ClientResult<(reqwest::Client, Url, Cookie, Arc<Sensitive>)> {
    let mut connect_url = Url::parse(auth_url).map_err(|_| ClientError::InvalidAuthUrl)?;
    if connect_url.scheme() != "http"
        || connect_url.path() != "/connect"
        || !connect_url.username().is_empty()
        || connect_url.password().is_some()
        || !connect_url.host_str().is_some_and(|host| {
            host == "127.0.0.1"
                || host == "::1"
                || host == "[::1]"
                || host.eq_ignore_ascii_case("localhost")
        })
    {
        return Err(ClientError::InvalidAuthUrl);
    }
    let token = connect_url
        .query_pairs()
        .find(|(name, _)| name == "token")
        .map(|(_, value)| value.into_owned());
    connect_url
        .query_pairs_mut()
        .append_pair("interactions", "answer");
    let mut base_url = connect_url.clone();
    base_url.set_query(None);
    base_url.set_fragment(None);
    base_url.set_path("/");
    let http = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|_| ClientError::Transport {
            operation: "HTTP client construction",
        })?;
    let response = timeout(config.request_timeout, http.get(connect_url.clone()).send())
        .await
        .map_err(|_| ClientError::Timeout {
            operation: "authentication",
            timeout: config.request_timeout,
        })?
        .map_err(|_| ClientError::Transport {
            operation: "authentication",
        })?;
    if response.status() != StatusCode::FOUND {
        return Err(ClientError::Authentication {
            status: response.status(),
        });
    }
    let raw_cookie = response
        .headers()
        .get(SET_COOKIE)
        .ok_or(ClientError::MissingCookie)?
        .to_str()
        .map_err(|_| ClientError::MissingCookie)?;
    let pair = raw_cookie
        .split(';')
        .next()
        .map(str::trim)
        .filter(|pair| {
            pair.split_once('=')
                .is_some_and(|(name, value)| !name.is_empty() && !value.is_empty())
        })
        .ok_or(ClientError::MissingCookie)?;
    HeaderValue::from_str(pair).map_err(|_| ClientError::MissingCookie)?;
    let cookie = Cookie(pair.to_owned());
    let cookie_value = pair.split_once('=').map_or("", |(_, value)| value);
    let sensitive = Arc::new(Sensitive::new(&[
        auth_url,
        connect_url.as_str(),
        token.as_deref().unwrap_or(""),
        raw_cookie,
        pair,
        cookie_value,
    ]));
    Ok((http, base_url, cookie, sensitive))
}

struct Cookie(String);

impl fmt::Debug for Cookie {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Cookie([redacted])")
    }
}

struct Sensitive {
    values: Vec<String>,
}

impl Sensitive {
    fn new(values: &[&str]) -> Self {
        Self {
            values: values
                .iter()
                .filter(|value| !value.is_empty())
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    fn redact(&self, text: &str) -> String {
        self.values.iter().fold(text.to_owned(), |text, secret| {
            text.replace(secret, "[redacted]")
        })
    }

    fn redact_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.redact_value(item)).collect())
            }
            Value::Object(items) => Value::Object(
                items
                    .iter()
                    .map(|(key, item)| (self.redact(key), self.redact_value(item)))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }
}

fn next_wire_id(next: &AtomicU64, kind: &str) -> String {
    format!("metis-{kind}-{}", next.fetch_add(1, Ordering::Relaxed))
}

fn rpc_envelope(rpc_id: &str, method: &str, args: &Value) -> Value {
    json!({
        "type": "client-request",
        "rpcId": rpc_id,
        "method": method,
        "payload": { "args": args },
    })
}

fn parse_rpc_response(
    body: &[u8],
    expected_rpc_id: &str,
    sensitive: &Sensitive,
) -> ClientResult<Value> {
    let envelope: Value = serde_json::from_slice(body)
        .map_err(|_| ClientError::Protocol("RPC response is not valid JSON"))?;
    if envelope.get("type").and_then(Value::as_str) != Some("server-response") {
        return Err(ClientError::Protocol(
            "RPC response is not a server-response envelope",
        ));
    }
    if envelope.get("rpcId").and_then(Value::as_str) != Some(expected_rpc_id) {
        return Err(ClientError::Protocol("RPC response correlation mismatch"));
    }
    let result = envelope
        .get("result")
        .and_then(Value::as_object)
        .ok_or(ClientError::Protocol("RPC response omitted result"))?;
    match result.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(result.get("value").cloned().unwrap_or(Value::Null)),
        Some(false) => Err(remote_failure(
            result
                .get("error")
                .ok_or(ClientError::Protocol("failed RPC response omitted error"))?,
            sensitive,
        )?),
        None => Err(ClientError::Protocol("RPC response omitted result.ok")),
    }
}

fn remote_failure(value: &Value, sensitive: &Sensitive) -> ClientResult<ClientError> {
    let error = value
        .as_object()
        .ok_or(ClientError::Protocol("Remote error is not an object"))?;
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .ok_or(ClientError::Protocol("Remote error omitted code"))?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .ok_or(ClientError::Protocol("Remote error omitted message"))?;
    let details = error
        .get("details")
        .filter(|details| details.is_object())
        .ok_or(ClientError::Protocol("Remote error omitted details"))?;
    Ok(ClientError::Remote {
        code: sensitive.redact(code),
        message: sensitive.redact(message),
        details: sensitive.redact_value(details),
    })
}

fn prompt_args(session_id: &str, request_id: &str, text: &str) -> Value {
    json!({ "request": {
        "requestId": request_id,
        "sessionId": session_id,
        "mode": "queue",
        "content": [{ "type": "text", "text": text }],
    }})
}

fn approval_args(approval: &ApprovalRequest, outcome: ApprovalOutcome) -> Value {
    json!({
        "clientId": approval.client_id,
        "eventId": approval.event_id,
        "outcome": { "kind": "result", "value": outcome.as_str() },
    })
}

fn expect_accepted(value: &Value, message: &'static str) -> ClientResult<()> {
    if value.get("accepted").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(ClientError::Protocol(message))
    }
}

fn parse_session_id(value: &Value) -> ClientResult<String> {
    required_string_with_message(
        value,
        "sessionId",
        "session/create response omitted sessionId",
    )
}

fn parse_project(value: &Value) -> ClientResult<Project> {
    let recent_session_ids = value
        .get("recentSessionIds")
        .and_then(Value::as_array)
        .ok_or(ClientError::Protocol(
            "project response omitted recentSessionIds",
        ))?
        .iter()
        .map(|id| {
            id.as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or(ClientError::Protocol(
                    "project response contained an invalid sessionId",
                ))
        })
        .collect::<ClientResult<Vec<_>>>()?;
    Ok(Project {
        project_id: required_string_with_message(
            value,
            "projectId",
            "project response omitted projectId",
        )?,
        path: required_string_with_message(value, "path", "project response omitted path")?,
        title: required_string_with_message(value, "title", "project response omitted title")?,
        recent_session_ids,
        has_more_sessions: value
            .get("hasMoreSessions")
            .and_then(Value::as_bool)
            .ok_or(ClientError::Protocol(
                "project response omitted hasMoreSessions",
            ))?,
    })
}

fn parse_ready_client_id(value: &Value) -> ClientResult<String> {
    if value.get("type").and_then(Value::as_str) != Some("ready") {
        return Err(ClientError::Protocol("$events did not open with ready"));
    }
    value
        .get("clientId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(ClientError::Protocol("$events ready omitted clientId"))
}

fn parse_host_event(value: &Value, client_id: &str) -> ClientResult<HostEvent> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ClientError::Protocol("event frame omitted type"))?;
    match kind {
        "emit" => Ok(HostEvent::Emit {
            event: required_string(value, "event")?,
            args: value.get("args").cloned().unwrap_or(Value::Null),
        }),
        "waterfall" => {
            let event = required_string(value, "event")?;
            let event_id = required_string(value, "eventId")?;
            let agent_id = required_string(value, "agentId")?;
            let request = value
                .get("request")
                .filter(|request| request.is_object())
                .cloned()
                .ok_or(ClientError::Protocol("waterfall omitted request"))?;
            if event == "approval/request" {
                Ok(HostEvent::Approval(ApprovalRequest {
                    client_id: client_id.to_owned(),
                    event_id,
                    agent_id,
                    request,
                }))
            } else {
                Ok(HostEvent::Waterfall {
                    event,
                    event_id,
                    agent_id,
                    request,
                })
            }
        }
        "cancel" => Ok(HostEvent::Cancelled {
            event_id: required_string(value, "eventId")?,
        }),
        _ => Err(ClientError::Protocol("unknown event frame type")),
    }
}

fn required_text(value: &Value, key: &str, message: &'static str) -> ClientResult<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ClientError::Protocol(message))
}

fn required_string(value: &Value, key: &str) -> ClientResult<String> {
    required_string_with_message(value, key, "event frame omitted a required string")
}

fn required_string_with_message(
    value: &Value,
    key: &str,
    message: &'static str,
) -> ClientResult<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(ClientError::Protocol(message))
}

const STREAM_CAPACITY: usize = 64;

#[derive(Clone)]
enum StreamMessage {
    Item(Value),
    End,
    Failed(ClientError),
}

enum MuxCommand {
    Open {
        stream_id: String,
        endpoint: String,
        payload: Value,
    },
    Cancel(String),
    Close,
}

#[derive(Clone)]
struct StreamRoute {
    sender: mpsc::Sender<StreamMessage>,
    overflowed: Arc<AtomicBool>,
}

impl StreamRoute {
    fn new(sender: mpsc::Sender<StreamMessage>) -> Self {
        Self {
            sender,
            overflowed: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct MuxShared {
    streams: Mutex<HashMap<String, StreamRoute>>,
    failure: Mutex<Option<ClientError>>,
    shutdown: watch::Sender<ClientError>,
}

impl MuxShared {
    fn new() -> Self {
        let (shutdown, _) = watch::channel(ClientError::ConnectionClosed);
        Self {
            streams: Mutex::new(HashMap::new()),
            failure: Mutex::new(None),
            shutdown,
        }
    }

    fn terminal_failure(&self) -> Option<ClientError> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn terminate(&self, error: ClientError) {
        {
            let mut failure = self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if failure.is_none() {
                *failure = Some(error);
            }
        }
        // Dropping senders preserves queued items; next() reports this failure after the drain.
        lock_streams(self).clear();
    }

    fn terminate_and_stop(&self, error: ClientError) {
        self.terminate(error.clone());
        self.shutdown.send_replace(error);
    }
}

fn cancel_or_stop(commands: &mpsc::Sender<MuxCommand>, shared: &MuxShared, stream_id: String) {
    match commands.try_send(MuxCommand::Cancel(stream_id)) {
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            shared.terminate_and_stop(ClientError::CommandQueueFull);
        }
    }
}

fn lock_streams(shared: &MuxShared) -> std::sync::MutexGuard<'_, HashMap<String, StreamRoute>> {
    shared
        .streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn open_mux_stream(
    commands: &mpsc::Sender<MuxCommand>,
    shared: &Arc<MuxShared>,
    next_id: &AtomicU64,
    config: DshClientConfig,
    endpoint: &str,
    args: Value,
) -> ClientResult<DshStream> {
    let stream_id = next_wire_id(next_id, "stream");
    let (sender, mut receiver) = mpsc::channel(STREAM_CAPACITY);
    let route = StreamRoute::new(sender);
    let overflowed = Arc::clone(&route.overflowed);
    lock_streams(shared).insert(stream_id.clone(), route);
    // Cover a terminate racing between registration and this check.
    if let Some(error) = shared.terminal_failure() {
        lock_streams(shared).remove(&stream_id);
        return Err(error);
    }
    let command = MuxCommand::Open {
        stream_id: stream_id.clone(),
        endpoint: endpoint.to_owned(),
        payload: json!({ "args": args }),
    };
    let opened = timeout(config.stream_open_timeout, async {
        if commands.send(command).await.is_err() {
            return Err(shared
                .terminal_failure()
                .unwrap_or(ClientError::ConnectionClosed));
        }
        match receiver.recv().await {
            Some(StreamMessage::Item(value)) => Ok(value),
            Some(StreamMessage::Failed(error)) => Err(error),
            Some(StreamMessage::End) => Err(ClientError::Protocol(
                "stream ended before its opening item",
            )),
            None if overflowed.load(Ordering::Acquire) => Err(ClientError::StreamOverflow),
            None => Err(shared
                .terminal_failure()
                .unwrap_or(ClientError::ConnectionClosed)),
        }
    })
    .await;
    let opening = match opened {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            lock_streams(shared).remove(&stream_id);
            return Err(error);
        }
        Err(_) => {
            lock_streams(shared).remove(&stream_id);
            cancel_or_stop(commands, shared, stream_id);
            return Err(ClientError::Timeout {
                operation: "stream opening",
                timeout: config.stream_open_timeout,
            });
        }
    };
    Ok(DshStream {
        id: stream_id,
        opening: Some(opening),
        receiver,
        commands: commands.clone(),
        shared: Arc::clone(shared),
        overflowed,
        active: true,
    })
}

async fn run_mux(
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut commands: mpsc::Receiver<MuxCommand>,
    write_timeout: Duration,
    shared: Arc<MuxShared>,
    sensitive: Arc<Sensitive>,
) {
    let (mut writer, mut reader) = socket.split();
    let mut shutdown = shared.shutdown.subscribe();
    if let Some(error) = shared.terminal_failure() {
        commands.close();
        shared.terminate(error);
        return;
    }
    let failure = loop {
        tokio::select! {
            change = shutdown.changed() => {
                if change.is_ok() {
                    break Some(shutdown.borrow_and_update().clone());
                }
                break Some(ClientError::ConnectionClosed);
            }
            command = commands.recv() => {
                match command {
                    Some(MuxCommand::Open { stream_id, endpoint, payload }) => {
                        let text = json!({
                            "type": "open",
                            "streamId": stream_id,
                            "endpoint": endpoint,
                            "payload": payload,
                        }).to_string();
                        if let Err(error) = websocket_write(
                            writer.send(Message::Text(text.into())),
                            write_timeout,
                            "WebSocket write",
                        ).await {
                            break Some(error);
                        }
                    }
                    Some(MuxCommand::Cancel(stream_id)) => {
                        let text = json!({ "type": "cancel", "streamId": stream_id }).to_string();
                        if let Err(error) = websocket_write(
                            writer.send(Message::Text(text.into())),
                            write_timeout,
                            "WebSocket write",
                        ).await {
                            break Some(error);
                        }
                    }
                    Some(MuxCommand::Close) | None => {
                        let error = websocket_write(
                            writer.close(),
                            write_timeout,
                            "WebSocket close",
                        ).await.err();
                        break error;
                    }
                }
            }
            message = reader.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match route_server_frame(&text, &shared, &sensitive) {
                            Ok(Some(stream_id)) => {
                                let text = json!({ "type": "cancel", "streamId": stream_id }).to_string();
                                if let Err(error) = websocket_write(
                                    writer.send(Message::Text(text.into())),
                                    write_timeout,
                                    "WebSocket write",
                                ).await {
                                    break Some(error);
                                }
                            }
                            Ok(None) => {}
                            Err(error) => break Some(error),
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if let Err(error) = websocket_write(
                            writer.send(Message::Pong(payload)),
                            write_timeout,
                            "WebSocket write",
                        ).await {
                            break Some(error);
                        }
                    }

                    Some(Ok(Message::Close(_))) | None => break Some(ClientError::ConnectionClosed),
                    Some(Ok(Message::Binary(_))) => {
                        break Some(ClientError::Protocol("WebSocket received a binary frame"));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {
                        break Some(ClientError::Transport { operation: "WebSocket read" });
                    }
                }
            }
        }
    };
    commands.close();
    shared.terminate(failure.unwrap_or(ClientError::ConnectionClosed));
}

async fn websocket_write(
    write: impl std::future::Future<Output = Result<(), WebSocketError>>,
    write_timeout: Duration,
    operation: &'static str,
) -> ClientResult<()> {
    timeout(write_timeout, write)
        .await
        .map_err(|_| ClientError::Timeout {
            operation,
            timeout: write_timeout,
        })?
        .map_err(|_| ClientError::Transport { operation })
}

/// Route one frame. An overflow returns the affected stream id for a targeted cancel.
fn route_server_frame(
    text: &str,
    shared: &MuxShared,
    sensitive: &Sensitive,
) -> ClientResult<Option<String>> {
    let frame: Value = serde_json::from_str(text)
        .map_err(|_| ClientError::Protocol("WebSocket message is not valid JSON"))?;
    let stream_id = frame
        .get("streamId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(ClientError::Protocol("stream frame omitted streamId"))?;
    match frame.get("type").and_then(Value::as_str) {
        Some("item") => {
            let route = lock_streams(shared).get(stream_id).cloned();
            if let Some(route) = route {
                let message =
                    StreamMessage::Item(frame.get("value").cloned().unwrap_or(Value::Null));
                if send_bounded(&route.sender, message).is_err() {
                    lock_streams(shared).remove(stream_id);
                    route.overflowed.store(true, Ordering::Release);
                    return Ok(Some(stream_id.to_owned()));
                }
            }
            Ok(None)
        }
        Some("end") => {
            let route = lock_streams(shared).remove(stream_id);
            if let Some(route) = route
                && send_bounded(&route.sender, StreamMessage::End).is_err()
            {
                route.overflowed.store(true, Ordering::Release);
            }
            Ok(None)
        }
        Some("error") => {
            let error = remote_failure(
                frame
                    .get("error")
                    .ok_or(ClientError::Protocol("stream error omitted error"))?,
                sensitive,
            )?;
            let route = lock_streams(shared).remove(stream_id);
            if let Some(route) = route
                && send_bounded(&route.sender, StreamMessage::Failed(error)).is_err()
            {
                route.overflowed.store(true, Ordering::Release);
            }
            Ok(None)
        }
        _ => Err(ClientError::Protocol("unknown stream frame type")),
    }
}

fn send_bounded(sender: &mpsc::Sender<StreamMessage>, message: StreamMessage) -> ClientResult<()> {
    match sender.try_send(message) {
        Err(mpsc::error::TrySendError::Full(_)) => Err(ClientError::StreamOverflow),
        // Cancellation/drop can race routing; closed streams need no further delivery.
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::connect_async;

    fn sensitive() -> Sensitive {
        Sensitive::new(&["secret-token", "dsh-auth=secret-cookie", "secret-cookie"])
    }

    async fn websocket_pair() -> (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        WebSocketStream<TcpStream>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test WebSocket listener");
        let address = listener.local_addr().expect("read test listener address");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept test WebSocket");
            tokio_tungstenite::accept_async(socket)
                .await
                .expect("complete test WebSocket handshake")
        });
        let (client, _) = connect_async(format!("ws://{address}"))
            .await
            .expect("connect test WebSocket client");
        let server = server.await.expect("join test WebSocket server");
        (client, server)
    }

    #[test]
    fn request_envelope_and_prompt_retry_identity_are_stable() {
        let ids = AtomicU64::new(7);
        let first = next_wire_id(&ids, "rpc");
        let second = next_wire_id(&ids, "rpc");
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("metis-rpc-7", "metis-rpc-8")
        );
        let args = prompt_args("session-1", "caller-request", "hello");
        assert_eq!(
            rpc_envelope(&first, "session/prompt", &args),
            json!({
                "type": "client-request",
                "rpcId": "metis-rpc-7",
                "method": "session/prompt",
                "payload": { "args": { "request": {
                    "requestId": "caller-request",
                    "sessionId": "session-1",
                    "mode": "queue",
                    "content": [{ "type": "text", "text": "hello" }],
                }}},
            })
        );
        assert_eq!(args["request"]["requestId"], "caller-request");
        assert_eq!(second, "metis-rpc-8");
    }

    #[test]
    fn response_envelope_checks_correlation_and_remote_failure() {
        let secrets = sensitive();
        let value = parse_rpc_response(
            br#"{"type":"server-response","rpcId":"rpc-1","result":{"ok":true,"value":{"accepted":true}}}"#,
            "rpc-1",
            &secrets,
        )
        .expect("valid response");
        assert_eq!(value, json!({ "accepted": true }));

        let mismatch = parse_rpc_response(
            br#"{"type":"server-response","rpcId":"other","result":{"ok":true,"value":null}}"#,
            "rpc-1",
            &secrets,
        );
        assert_eq!(
            mismatch,
            Err(ClientError::Protocol("RPC response correlation mismatch"))
        );

        let failure = parse_rpc_response(
            br#"{"type":"server-response","rpcId":"rpc-1","result":{"ok":false,"error":{"code":"test/failure","message":"token secret-token","details":{"cookie":"secret-cookie"}}}}"#,
            "rpc-1",
            &secrets,
        );
        let failure = failure.unwrap_err();
        let rendered = format!("{failure:?} {failure}");
        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("secret-cookie"));
        assert!(rendered.contains("test/failure"));
    }

    #[tokio::test]
    async fn rpc_response_body_enforces_streamed_byte_limit() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test HTTP listener");
        let address = listener.local_addr().expect("read test HTTP address");
        let server = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            let (mut socket, _) = listener.accept().expect("accept test HTTP request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let chunk = vec![b'x'; 8192];
            for _ in 0..=MAX_RPC_RESPONSE_BYTES / chunk.len() {
                if socket.write_all(&chunk).is_err() {
                    break;
                }
            }
        });
        let response = reqwest::get(format!("http://{address}/"))
            .await
            .expect("read oversized test response");
        let error = read_rpc_body(response).await.unwrap_err();
        assert_eq!(
            error,
            ClientError::ResponseTooLarge {
                operation: "RPC response",
                limit: MAX_RPC_RESPONSE_BYTES,
            }
        );
        server.join().expect("test HTTP server must finish");
    }

    #[test]
    fn project_response_parses_metadata_and_sessions() {
        let project = parse_project(&json!({
            "projectId": "project-1",
            "path": "/workspace/project",
            "title": "Project",
            "recentSessionIds": ["session-1", "session-2"],
            "hasMoreSessions": false,
        }))
        .expect("valid project response");
        assert_eq!(
            project,
            Project {
                project_id: "project-1".into(),
                path: "/workspace/project".into(),
                title: "Project".into(),
                recent_session_ids: vec!["session-1".into(), "session-2".into()],
                has_more_sessions: false,
            }
        );
    }

    #[test]
    fn project_response_rejects_empty_strings_and_invalid_session_ids() {
        let empty_id = json!({
            "projectId": "",
            "path": "/workspace/project",
            "title": "Project",
            "recentSessionIds": [],
            "hasMoreSessions": false,
        });
        assert_eq!(
            parse_project(&empty_id),
            Err(ClientError::Protocol("project response omitted projectId"))
        );

        let invalid_session = json!({
            "projectId": "project-1",
            "path": "/workspace/project",
            "title": "Project",
            "recentSessionIds": ["session-1", 7],
            "hasMoreSessions": false,
        });
        assert_eq!(
            parse_project(&invalid_session),
            Err(ClientError::Protocol(
                "project response contained an invalid sessionId"
            ))
        );
    }

    #[test]
    fn approval_result_uses_host_correlation_and_supported_outcomes() {
        let approval = ApprovalRequest {
            client_id: "client-1".into(),
            event_id: "event-1".into(),
            agent_id: "agent-1".into(),
            request: json!({ "toolName": "shell" }),
        };
        assert_eq!(
            approval_args(&approval, ApprovalOutcome::AllowedOnce),
            json!({
                "clientId": "client-1",
                "eventId": "event-1",
                "outcome": { "kind": "result", "value": "allowed-once" },
            })
        );
        assert_eq!(
            approval_args(&approval, ApprovalOutcome::Rejected)["outcome"]["value"],
            "rejected"
        );
    }

    #[test]
    fn event_ready_approval_and_cancel_are_parsed() {
        assert_eq!(
            parse_ready_client_id(&json!({ "type": "ready", "clientId": "client-1" })).unwrap(),
            "client-1"
        );
        let event = parse_host_event(
            &json!({
                "type": "waterfall",
                "event": "approval/request",
                "eventId": "event-1",
                "agentId": "agent-1",
                "request": { "toolName": "shell", "reason": "test" },
            }),
            "client-1",
        )
        .unwrap();
        assert_eq!(
            event,
            HostEvent::Approval(ApprovalRequest {
                client_id: "client-1".into(),
                event_id: "event-1".into(),
                agent_id: "agent-1".into(),
                request: json!({ "toolName": "shell", "reason": "test" }),
            })
        );
        let waterfall = parse_host_event(
            &json!({
                "type": "waterfall",
                "event": "user-questions/request",
                "eventId": "event-2",
                "agentId": "agent-2",
                "request": { "question": "continue?" },
            }),
            "client-1",
        )
        .unwrap();
        assert!(matches!(
            waterfall,
            HostEvent::Waterfall { agent_id, .. } if agent_id == "agent-2"
        ));
        let empty_agent = parse_host_event(
            &json!({
                "type": "waterfall",
                "event": "approval/request",
                "eventId": "event-3",
                "agentId": "",
                "request": {},
            }),
            "client-1",
        );
        assert_eq!(
            empty_agent,
            Err(ClientError::Protocol(
                "event frame omitted a required string"
            ))
        );
        assert_eq!(
            parse_host_event(
                &json!({ "type": "cancel", "eventId": "event-1" }),
                "client-1"
            )
            .unwrap(),
            HostEvent::Cancelled {
                event_id: "event-1".into()
            }
        );
    }

    #[test]
    fn mux_frames_route_items_terminal_errors_and_late_frames() {
        let shared = MuxShared::new();
        let (sender, mut receiver) = mpsc::channel(STREAM_CAPACITY);
        lock_streams(&shared).insert("stream-1".into(), StreamRoute::new(sender));
        route_server_frame(
            r#"{"type":"item","streamId":"stream-1","value":{"type":"snapshot"}}"#,
            &shared,
            &sensitive(),
        )
        .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(StreamMessage::Item(value)) if value["type"] == "snapshot"
        ));
        route_server_frame(
            r#"{"type":"error","streamId":"stream-1","error":{"code":"test/error","message":"failed","details":{}}}"#,
            &shared,
            &sensitive(),
        )
        .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(StreamMessage::Failed(ClientError::Remote { code, .. })) if code == "test/error"
        ));
        assert!(!lock_streams(&shared).contains_key("stream-1"));
        route_server_frame(
            r#"{"type":"end","streamId":"stream-1"}"#,
            &shared,
            &sensitive(),
        )
        .expect("late frames are harmless");
    }

    #[test]
    fn stream_cancel_unregisters_sends_cancel_and_ignores_late_frames() {
        let shared = Arc::new(MuxShared::new());
        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let route = StreamRoute::new(sender);
        let overflowed = Arc::clone(&route.overflowed);
        lock_streams(&shared).insert("stream-1".into(), route);
        let (commands, mut command_receiver) = mpsc::channel(MUX_COMMAND_CAPACITY);
        let mut stream = DshStream {
            id: "stream-1".into(),
            opening: None,
            receiver,
            commands,
            shared: Arc::clone(&shared),
            overflowed,
            active: true,
        };

        stream.cancel();
        assert!(!lock_streams(&shared).contains_key("stream-1"));
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(MuxCommand::Cancel(stream_id)) if stream_id == "stream-1"
        ));
        route_server_frame(
            r#"{"type":"item","streamId":"stream-1","value":"late"}"#,
            &shared,
            &sensitive(),
        )
        .unwrap();
        route_server_frame(
            r#"{"type":"end","streamId":"stream-1"}"#,
            &shared,
            &sensitive(),
        )
        .unwrap();
        assert!(matches!(
            stream.receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn stream_overflow_isolated_and_survives_queued_items() {
        let shared = Arc::new(MuxShared::new());
        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let route = StreamRoute::new(sender);
        let overflowed = Arc::clone(&route.overflowed);
        lock_streams(&shared).insert("stream-1".into(), route);
        let (other_sender, mut other_receiver) = mpsc::channel(STREAM_CAPACITY);
        lock_streams(&shared).insert("stream-2".into(), StreamRoute::new(other_sender));
        let (commands, _command_receiver) = mpsc::channel(MUX_COMMAND_CAPACITY);
        let mut stream = DshStream {
            id: "stream-1".into(),
            opening: None,
            receiver,
            commands,
            shared: Arc::clone(&shared),
            overflowed,
            active: true,
        };
        for _ in 0..STREAM_CAPACITY {
            route_server_frame(
                r#"{"type":"item","streamId":"stream-1","value":0}"#,
                &shared,
                &sensitive(),
            )
            .unwrap();
        }
        let cancel = route_server_frame(
            r#"{"type":"item","streamId":"stream-1","value":1}"#,
            &shared,
            &sensitive(),
        )
        .unwrap();
        assert_eq!(cancel, Some("stream-1".to_owned()));
        assert_eq!(shared.terminal_failure(), None);
        assert!(!lock_streams(&shared).contains_key("stream-1"));

        route_server_frame(
            r#"{"type":"item","streamId":"stream-2","value":2}"#,
            &shared,
            &sensitive(),
        )
        .unwrap();
        assert!(matches!(
            other_receiver.try_recv(),
            Ok(StreamMessage::Item(value)) if value == 2
        ));

        let terminal = timeout(Duration::from_secs(1), async {
            for _ in 0..STREAM_CAPACITY {
                assert!(matches!(stream.next().await, Ok(Some(_))));
            }
            stream.next().await
        })
        .await
        .expect("overflow delivery must not wait for queue capacity");
        assert!(matches!(terminal, Err(ClientError::StreamOverflow)));
    }

    #[tokio::test]
    async fn mux_eof_terminates_registered_streams() {
        let (socket, server) = websocket_pair().await;
        let shared = Arc::new(MuxShared::new());
        let (commands, command_receiver) = mpsc::channel(MUX_COMMAND_CAPACITY);
        let task = tokio::spawn(run_mux(
            socket,
            command_receiver,
            Duration::from_secs(1),
            Arc::clone(&shared),
            Arc::new(sensitive()),
        ));
        let (sender, mut receiver) = mpsc::channel(STREAM_CAPACITY);
        lock_streams(&shared).insert("stream-1".into(), StreamRoute::new(sender));

        drop(server);
        let closed = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("EOF must close downstream receivers");
        assert!(closed.is_none());
        assert!(shared.terminal_failure().is_some());
        drop(commands);
        task.await.expect("mux task must finish after EOF");
    }

    #[tokio::test]
    async fn stream_open_timeout_sends_targeted_cancel() {
        let (socket, mut server) = websocket_pair().await;
        let shared = Arc::new(MuxShared::new());
        let (commands, command_receiver) = mpsc::channel(MUX_COMMAND_CAPACITY);
        let task = tokio::spawn(run_mux(
            socket,
            command_receiver,
            Duration::from_secs(1),
            Arc::clone(&shared),
            Arc::new(sensitive()),
        ));
        let config = DshClientConfig {
            request_timeout: Duration::from_secs(1),
            stream_open_timeout: Duration::from_millis(20),
        };
        let opened = open_mux_stream(
            &commands,
            &shared,
            &AtomicU64::new(0),
            config,
            "session/follow",
            json!({}),
        )
        .await;
        assert!(matches!(
            opened,
            Err(ClientError::Timeout {
                operation: "stream opening",
                ..
            })
        ));

        let open = timeout(Duration::from_secs(1), server.next())
            .await
            .expect("read open frame")
            .expect("open frame precedes stream end")
            .expect("valid open frame");
        let Message::Text(open) = open else {
            panic!("open must be a text frame");
        };
        let open: Value = serde_json::from_str(&open).expect("open frame is JSON");
        let cancel = timeout(Duration::from_secs(1), server.next())
            .await
            .expect("opening timeout must send cancel")
            .expect("cancel frame precedes stream end")
            .expect("valid cancel frame");
        let Message::Text(cancel) = cancel else {
            panic!("cancel must be a text frame");
        };
        let cancel: Value = serde_json::from_str(&cancel).expect("cancel frame is JSON");
        assert_eq!(open["type"], "open");
        assert_eq!(cancel["type"], "cancel");
        assert_eq!(cancel["streamId"], open["streamId"]);

        drop(commands);
        timeout(Duration::from_secs(1), task)
            .await
            .expect("mux shutdown")
            .expect("mux task must finish");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_command_queue_stops_mux_instead_of_dropping_cancel() {
        let (socket, server) = websocket_pair().await;
        let (commands, command_receiver) = mpsc::channel(MUX_COMMAND_CAPACITY);
        for _ in 0..MUX_COMMAND_CAPACITY {
            commands
                .try_send(MuxCommand::Cancel("occupied".into()))
                .expect("fill test command queue");
        }
        let shared = Arc::new(MuxShared::new());
        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let route = StreamRoute::new(sender);
        let overflowed = Arc::clone(&route.overflowed);
        lock_streams(&shared).insert("stream-1".into(), route);
        let mut stream = DshStream {
            id: "stream-1".into(),
            opening: None,
            receiver,
            commands,
            shared: Arc::clone(&shared),
            overflowed,
            active: true,
        };
        let task = tokio::spawn(run_mux(
            socket,
            command_receiver,
            Duration::from_secs(1),
            Arc::clone(&shared),
            Arc::new(sensitive()),
        ));

        stream.cancel();
        assert_eq!(
            shared.terminal_failure(),
            Some(ClientError::CommandQueueFull)
        );
        timeout(Duration::from_secs(1), task)
            .await
            .expect("saturated mux must stop")
            .expect("mux task must finish");
        drop(server);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_running_mux_stops_on_cancel() {
        let (socket, mut server) = websocket_pair().await;
        let (commands, command_receiver) = mpsc::channel(MUX_COMMAND_CAPACITY);
        let shared = Arc::new(MuxShared::new());
        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let route = StreamRoute::new(sender);
        let overflowed = Arc::clone(&route.overflowed);
        lock_streams(&shared).insert("stream-1".into(), route);
        let mut stream = DshStream {
            id: "stream-1".into(),
            opening: None,
            receiver,
            commands: commands.clone(),
            shared: Arc::clone(&shared),
            overflowed,
            active: true,
        };
        let task = tokio::spawn(run_mux(
            socket,
            command_receiver,
            Duration::from_secs(1),
            Arc::clone(&shared),
            Arc::new(sensitive()),
        ));
        commands
            .send(MuxCommand::Cancel("warmup".into()))
            .await
            .expect("send warmup cancel");
        timeout(Duration::from_secs(1), server.next())
            .await
            .expect("mux must enter its event loop")
            .expect("warmup cancel frame")
            .expect("valid warmup frame");
        for _ in 0..MUX_COMMAND_CAPACITY {
            commands
                .try_send(MuxCommand::Cancel("occupied".into()))
                .expect("fill running mux command queue");
        }

        stream.cancel();
        assert_eq!(
            shared.terminal_failure(),
            Some(ClientError::CommandQueueFull)
        );
        timeout(Duration::from_secs(1), task)
            .await
            .expect("watch must stop a running saturated mux")
            .expect("mux task must finish");
        drop(server);
    }

    #[tokio::test]
    async fn websocket_write_uses_bounded_timeout() {
        let error = websocket_write(
            std::future::pending::<Result<(), WebSocketError>>(),
            Duration::from_millis(1),
            "WebSocket write",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Timeout {
                operation: "WebSocket write",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn authentication_rejects_non_loopback_urls_before_network() {
        for url in [
            "http://example.com/connect?token=secret",
            "http://0.0.0.0/connect?token=secret",
        ] {
            let error = authenticate(url, DshClientConfig::default())
                .await
                .err()
                .expect("non-loopback URL must be rejected");
            assert_eq!(error, ClientError::InvalidAuthUrl);
        }
    }

    #[test]
    fn cookie_debug_is_redacted() {
        let rendered = format!("{:?}", Cookie("name=secret".into()));
        assert!(!rendered.contains("secret"));
    }
}
