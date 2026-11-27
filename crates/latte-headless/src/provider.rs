use latte_core::valid_openai_chat_tool_call_id;
use latte_engine::ToolDescriptor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingBlock {
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        thinking_blocks: Vec<ThinkingBlock>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        content: String,
    },
}
impl Message {
    #[must_use]
    pub fn is_role(&self, role: &str) -> bool {
        matches!(
            (self, role),
            (Self::System { .. }, "system")
                | (Self::User { .. }, "user")
                | (Self::Assistant { .. }, "assistant")
                | (Self::Tool { .. }, "tool")
        )
    }
    #[must_use]
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::System { content } | Self::User { content } | Self::Tool { content, .. } => {
                Some(content)
            }
            Self::Assistant { content, .. } => content.as_deref(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}
pub(crate) fn valid_tool_call_id(id: &str) -> bool {
    valid_openai_chat_tool_call_id(id)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderResponse {
    pub message: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub input_request: Option<InputRequest>,
    #[serde(default)]
    pub usage: ProviderUsage,
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<Value>,
}

impl ProviderResponse {
    #[must_use]
    pub fn reasoning_content(&self) -> Option<&str> {
        self.provider_state
            .as_ref()
            .and_then(|state| state.get("reasoning_content"))
            .and_then(Value::as_str)
    }

    #[must_use]
    pub fn thinking_blocks(&self) -> Vec<ThinkingBlock> {
        self.provider_state
            .as_ref()
            .and_then(|state| state.get("thinking_blocks"))
            .and_then(|blocks| serde_json::from_value(blocks.clone()).ok())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    /// Opaque per-Session identifier for Providers that route or group by
    /// conversation. It is derived from the Session id rather than being it:
    /// a Provider is an external party and receives no internal identifier.
    /// Stable for the whole Session, so follow-up turns keep the same value.
    pub session_ref: String,
}

/// Headers the provider derives itself. Configuration cannot set them: doing so
/// would let a config file replace resolved credentials or the wire format.
pub(crate) const RESERVED_HEADERS: [&str; 4] =
    ["authorization", "content-type", "host", "content-length"];

/// Headers the Anthropic Messages provider derives itself.
pub(crate) const ANTHROPIC_RESERVED_HEADERS: [&str; 6] = [
    "authorization",
    "x-api-key",
    "anthropic-version",
    "content-type",
    "host",
    "content-length",
];

/// The only placeholder a configured header value may use.
pub(crate) const SESSION_ID_PLACEHOLDER: &str = "${session_id}";

/// Derives the value a Provider sees for one Session.
///
/// The raw `SessionId` is never sent to a Provider. The ref is a deterministic,
/// unkeyed SHA-256 prefix, so two claims it must NOT make: it is not
/// uncorrelatable across Providers — every Provider receives the same stable
/// value for one Session, which is deliberate so a Provider can group that
/// Session's turns; and it is not unguessable by anyone who already knows the
/// UUID. Its only privacy property is that the wire value reveals no internal
/// identifier to a party that does not.
#[must_use]
pub fn session_ref_for(session_id: latte_core::SessionId) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(session_id.as_uuid().as_bytes());
    // 128 bits: collision-free at any session count a workspace will reach, and
    // short enough that Providers with header length limits accept it.
    format!("{digest:x}")[..32].to_owned()
}

pub trait ProviderEventSink: Send + Sync {
    fn observe(&self, event: ProviderEvent);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    Attempt {
        number: u32,
    },
    /// A real provider-stream text delta. Non-streaming paths never emit this.
    AssistantDelta {
        text: String,
    },
}

#[derive(Clone)]
pub struct ProviderContext {
    pub deadline: Instant,
    pub cancellation: latte_engine::CancellationToken,
    pub events: Option<Arc<dyn ProviderEventSink>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub tools: bool,
    pub parallel_tool_calls: bool,
    pub input_request: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other(String),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputRequest {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub secret: bool,
}
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider request cancelled")]
    Cancelled,
    #[error("provider capability unavailable: {0}")]
    Capability(String),
    #[error("provider rejected request: http {status}{request_id}")]
    Http {
        status: u16,
        request_id: String,
        retryable: bool,
    },
    /// The provider rejected the request because its context window was
    /// exceeded. Classified from the HTTP error body (400/413 plus a
    /// context-length marker), which the byte budget cannot foresee: the
    /// local budget is an estimate and providers tokenize differently. The
    /// session loop reacts with one deterministic-elision/compaction
    /// recovery attempt rather than failing the turn.
    #[error("provider rejected request: context length exceeded{request_id}")]
    ContextOverflow { request_id: String },
    #[error("provider timeout")]
    Timeout,
    #[error("provider transport: {0}")]
    Transport(String),
    #[error("malformed provider response: {0}")]
    Malformed(String),
}

pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>>;
pub trait Provider: Send + Sync + 'static {
    fn complete(&self, request: ProviderRequest, context: ProviderContext) -> ProviderFuture<'_>;
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            parallel_tool_calls: true,
            input_request: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct FakeProvider {
    responses: Mutex<VecDeque<Result<ProviderResponse, String>>>,
}
impl FakeProvider {
    #[must_use]
    pub fn scripted(values: impl IntoIterator<Item = ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(values.into_iter().map(Ok).collect()),
        }
    }
    pub fn push_error(&self, error: impl Into<String>) {
        self.responses.lock().unwrap().push_back(Err(error.into()));
    }
}
impl Provider for FakeProvider {
    fn complete(&self, _: ProviderRequest, _: ProviderContext) -> ProviderFuture<'_> {
        let result = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Malformed("fake provider exhausted".into()))
            .and_then(|value| value.map_err(ProviderError::Transport));
        Box::pin(async move { result })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            parallel_tool_calls: true,
            input_request: true,
        }
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    timeout: Duration,
    compatibility_input_request: bool,
    max_attempts: u32,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    streaming: bool,
    headers: std::collections::BTreeMap<String, String>,
}
fn same_origin_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        let Some(first) = attempt.previous().first() else {
            return attempt.stop();
        };
        let next = attempt.url();
        let same_origin = next.scheme() == first.scheme()
            && next.host_str() == first.host_str()
            && next.port_or_known_default() == first.port_or_known_default();
        if same_origin {
            attempt.follow()
        } else {
            attempt.error("refusing cross-origin redirect on credentialed provider request")
        }
    })
}

impl OpenAiProvider {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeout.min(Duration::from_secs(30)))
            .redirect(same_origin_redirect_policy())
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            endpoint: endpoint.into(),
            model: model.into(),
            api_key: api_key.into(),
            timeout,
            compatibility_input_request: false,
            max_attempts: 1,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            streaming: true,
            headers: std::collections::BTreeMap::new(),
        })
    }
    /// Sets extra request headers this Provider requires beyond the protocol.
    /// `${session_id}` in a value expands per request. The caller must have
    /// validated names and placeholders; reserved headers are dropped here as
    /// a second line of defence so a bypassed caller cannot replace the
    /// resolved credential or the wire format.
    #[must_use]
    pub fn with_headers(mut self, headers: std::collections::BTreeMap<String, String>) -> Self {
        self.headers = headers
            .into_iter()
            .filter(|(name, _)| !RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
            .collect();
        self
    }
    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }
    #[must_use]
    pub fn with_compatibility_input_request(mut self, enabled: bool) -> Self {
        self.compatibility_input_request = enabled;
        self
    }
    #[must_use]
    pub fn with_sampling_options(
        mut self,
        temperature: Option<f64>,
        max_tokens: Option<u32>,
    ) -> Self {
        self.temperature = temperature;
        self.max_tokens = max_tokens;
        self
    }
    #[must_use]
    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<String>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }
    /// Enables `OpenAI` Chat Completions SSE. The provider still accepts a valid
    /// inline JSON response without inventing deltas.
    #[must_use]
    pub fn with_streaming(mut self, enabled: bool) -> Self {
        self.streaming = enabled;
        self
    }
}
#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<WireRequestMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}
#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireRequestMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: &'a str,
    },
    Assistant {
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<&'a str>,
        /// Omitted when the assistant turn made no calls. An empty array is not
        /// the same as absence on the wire: some Providers reject `tool_calls:
        /// []`, which would fail every follow-up turn of a Session whose
        /// history contains a plain text answer.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireRequestCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
        content: &'a str,
    },
}
#[derive(Serialize)]
struct WireRequestCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireRequestFunction<'a>,
}
#[derive(Serialize)]
struct WireRequestFunction<'a> {
    name: &'a str,
    arguments: String,
}
#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction,
}
#[derive(Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: Value,
}
#[derive(Deserialize)]
struct Wire {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    provider_state: Option<Value>,
}
#[derive(Deserialize)]
struct Choice {
    message: WireMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}
#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptTokenDetails>,
}
#[derive(Debug, Deserialize)]
struct WirePromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}
#[derive(Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireCall>,
    #[serde(default)]
    input_request: Option<InputRequest>,
}
#[derive(Deserialize)]
struct WireCall {
    id: String,
    function: WireFunction,
}
#[derive(Deserialize)]
struct WireFunction {
    name: String,
    arguments: String,
}
fn wire_message(message: &Message) -> Result<WireRequestMessage<'_>, ProviderError> {
    Ok(match message {
        Message::System { content } => WireRequestMessage::System { content },
        Message::User { content } => WireRequestMessage::User { content },
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => WireRequestMessage::Assistant {
            content: content.as_deref(),
            reasoning_content: reasoning_content.as_deref(),
            tool_calls: tool_calls
                .iter()
                .map(|call| {
                    Ok(WireRequestCall {
                        id: &call.id,
                        kind: "function",
                        function: WireRequestFunction {
                            name: &call.name,
                            arguments: serde_json::to_string(&call.input)
                                .map_err(|error| ProviderError::Malformed(error.to_string()))?,
                        },
                    })
                })
                .collect::<Result<_, ProviderError>>()?,
        },
        Message::Tool {
            tool_call_id,
            name,
            content,
        } => WireRequestMessage::Tool {
            tool_call_id,
            name: name.as_deref(),
            content,
        },
    })
}
impl Provider for OpenAiProvider {
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_lines)]
    fn complete(&self, request: ProviderRequest, context: ProviderContext) -> ProviderFuture<'_> {
        let this = self.clone();
        Box::pin(async move {
            let messages = &request.messages;
            let tools = &request.tools;
            if context.cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let body = Request {
                model: &this.model,
                messages: messages
                    .iter()
                    .map(wire_message)
                    .collect::<Result<_, _>>()?,
                tools: tools
                    .iter()
                    .map(|tool| WireTool {
                        kind: "function",
                        function: WireToolFunction {
                            name: tool.name.clone(),
                            description: tool.description.clone(),
                            parameters: tool.input_schema.clone(),
                        },
                    })
                    .collect(),
                tool_choice: (!tools.is_empty()).then_some("auto"),
                temperature: this.temperature,
                max_tokens: this.max_tokens,
                reasoning_effort: this.reasoning_effort.as_deref(),
                stream: this.streaming.then_some(true),
            };
            let response =
                send_json_with_retries(&body, this.timeout, this.max_attempts, &context, || {
                    apply_headers(
                        this.client.post(&this.endpoint).bearer_auth(&this.api_key),
                        &this.headers,
                        &request.session_ref,
                    )
                })
                .await?;
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let request_id = extract_request_id(response.headers());
                // Buffer the (bounded) error body once: the empty-body case
                // may still fall back to an inline request, while every other
                // rejection is classified from its body (context-overflow in
                // particular drives deterministic recovery in the session).
                let error_body = tokio::time::timeout(this.timeout, response.bytes())
                    .await
                    .map_err(|_| ProviderError::Timeout)?
                    .map_err(|error| ProviderError::Transport(error.to_string()))?;
                let error_body = &error_body;
                if this.streaming
                    && matches!(status, 400 | 404 | 415 | 422)
                    && error_body.is_empty()
                {
                    // A fallback is allowed only when the stream request was
                    // rejected before it produced *any* response body bytes.
                    let mut inline_body = serde_json::to_value(&body)
                        .map_err(|error| ProviderError::Malformed(error.to_string()))?;
                    inline_body
                        .as_object_mut()
                        .expect("request serializes to object")
                        .remove("stream");
                    return complete_inline_once(
                        &this,
                        inline_body,
                        &request.session_ref,
                        &context,
                    )
                    .await;
                }
                return Err(classify_http_error(status, request_id, error_body));
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if this.streaming && content_type.contains("text/event-stream") {
                return parse_openai_sse(response, this.timeout, &context).await;
            }
            let wire: Wire = tokio::time::timeout(this.timeout, response.json())
                .await
                .map_err(|_| ProviderError::Timeout)?
                .map_err(|e| ProviderError::Malformed(e.to_string()))?;
            decode_wire(wire, this.compatibility_input_request)
        })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            parallel_tool_calls: true,
            input_request: self.compatibility_input_request,
        }
    }
}

/// Applies the provider's configured headers to one outgoing request,
/// substituting the placeholder with this Session's reference.
///
/// Reserved headers were already dropped at construction, so this cannot
/// overwrite the credential or the wire format. A value that does not survive
/// `HeaderValue` parsing is skipped rather than failing the turn: the header is
/// an addition for the Provider's benefit, and losing the whole conversation
/// over one malformed one would be worse than sending the request without it.
fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &std::collections::BTreeMap<String, String>,
    session_ref: &str,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        let expanded = value.replace(SESSION_ID_PLACEHOLDER, session_ref);
        let Ok(name) = reqwest::header::HeaderName::try_from(name) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::try_from(expanded) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    builder
}

/// The one source of truth for "an HTTP status is transient" at the
/// transport layer: the retry loop and both `ProviderError::Http`
/// construction sites consume this verdict. The session's failure
/// classification deliberately does NOT read it — that classification
/// answers "can the user make progress in this conversation", not "would an
/// identical request succeed" (a 400 for an unavailable model carries
/// `retryable: false`, yet an in-session model switch makes a retry
/// meaningful; only 401/403 terminalize — see the `Err` arm in
/// `session.rs`).
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 502 | 503 | 504 | 529)
}

fn extract_request_id(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get("x-request-id")
        .or_else(|| headers.get("request-id"))
        .and_then(|v| v.to_str().ok())
        .map(|v| format!(" (request {v})"))
        .unwrap_or_default()
}

/// Maps one non-success HTTP response to the session-facing error. A
/// context-window rejection gets its own variant so the session loop can
/// react with one deterministic elision/compaction recovery attempt; the
/// local byte budget is only an estimate, so this provider-side signal is
/// the authoritative "the request really did not fit".
fn classify_http_error(status: u16, request_id: String, body: &[u8]) -> ProviderError {
    if matches!(status, 400 | 413) && body_signals_context_overflow(body) {
        return ProviderError::ContextOverflow { request_id };
    }
    ProviderError::Http {
        status,
        request_id,
        retryable: is_retryable_status(status),
    }
}

/// Exact provider error codes that unambiguously mean "input exceeded the
/// context window". OpenAI-family and several OpenAI-compatible servers
/// (`Azure`, `DeepSeek`, `Together`) emit one of these on
/// `error.code`/`error.type`.
const CONTEXT_OVERFLOW_CODES: &[&str] = &[
    "context_length_exceeded",
    "string_above_max_length",
    "invalid_request_error.context_length_exceeded",
];

/// Message substrings used only when no structured code is present
/// (proxies, Anthropic `/v1/messages`, and non-OpenAI-compatible error envelopes).
const CONTEXT_OVERFLOW_MESSAGE_MARKERS: &[&str] = &[
    "maximum context length",
    "context length",
    "context window",
    "too many tokens",
    "prompt is too long",
    "exceed context limit",
];

fn body_signals_context_overflow(body: &[u8]) -> bool {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        let error = value.get("error").unwrap_or(&value);
        let code = error
            .get("code")
            .or_else(|| error.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if CONTEXT_OVERFLOW_CODES.contains(&code.as_str()) {
            return true;
        }
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        return CONTEXT_OVERFLOW_MESSAGE_MARKERS
            .iter()
            .any(|marker| message.contains(marker));
    }
    // Plain-text/HTML error pages: match the same markers lossily, capped to
    // the buffered prefix.
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    CONTEXT_OVERFLOW_MESSAGE_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
}

fn emit_provider_event(context: &ProviderContext, event: ProviderEvent) {
    // Deliver deltas synchronously in exact FIFO chunk order. Sinks are
    // non-blocking channel senders, never blocking I/O.
    if let Some(sink) = &context.events {
        sink.observe(event);
    }
}

async fn send_json_with_retries<B, F>(
    body: &B,
    timeout: Duration,
    max_attempts: u32,
    context: &ProviderContext,
    make_builder: F,
) -> Result<reqwest::Response, ProviderError>
where
    B: Serialize + ?Sized,
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        if context.cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        emit_provider_event(context, ProviderEvent::Attempt { number: attempt });
        let remaining = context
            .deadline
            .saturating_duration_since(Instant::now())
            .min(timeout);
        if remaining.is_zero() {
            return Err(ProviderError::Timeout);
        }
        let sent = tokio::time::timeout(remaining, make_builder().json(body).send()).await;
        let response = match sent {
            Err(_) => return Err(ProviderError::Timeout),
            Ok(Err(error)) if error.is_connect() && attempt < max_attempts => {
                retry_pause(context, attempt, None).await?;
                continue;
            }
            Ok(Err(error)) => {
                return Err(if error.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Transport(error.to_string())
                });
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status().as_u16();
        if !response.status().is_success() && is_retryable_status(status) && attempt < max_attempts
        {
            let retry_after = parse_retry_after(response.headers());
            retry_pause(context, attempt, retry_after).await?;
            continue;
        }
        return Ok(response);
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_millis(ms));
    }
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|&secs| secs.is_finite() && secs >= 0.0)
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
}

async fn send_json_once(
    body: &Value,
    timeout: Duration,
    context: &ProviderContext,
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    if context.cancellation.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    let remaining = context
        .deadline
        .saturating_duration_since(Instant::now())
        .min(timeout);
    if remaining.is_zero() {
        return Err(ProviderError::Timeout);
    }
    let response = tokio::time::timeout(remaining, builder.json(body).send())
        .await
        .map_err(|_| ProviderError::Timeout)?
        .map_err(|error| ProviderError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let request_id = extract_request_id(response.headers());
        let error_body = tokio::time::timeout(timeout, response.bytes())
            .await
            .map_err(|_| ProviderError::Timeout)?
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        return Err(classify_http_error(status, request_id, &error_body));
    }
    Ok(response)
}

async fn complete_inline_once(
    provider: &OpenAiProvider,
    body: Value,
    session_ref: &str,
    context: &ProviderContext,
) -> Result<ProviderResponse, ProviderError> {
    let response = send_json_once(
        &body,
        provider.timeout,
        context,
        apply_headers(
            provider
                .client
                .post(&provider.endpoint)
                .bearer_auth(&provider.api_key),
            &provider.headers,
            session_ref,
        ),
    )
    .await?;
    let wire = tokio::time::timeout(provider.timeout, response.json())
        .await
        .map_err(|_| ProviderError::Timeout)?
        .map_err(|error| ProviderError::Malformed(error.to_string()))?;
    decode_wire(wire, provider.compatibility_input_request)
}

fn decode_wire(
    wire: Wire,
    compatibility_input_request: bool,
) -> Result<ProviderResponse, ProviderError> {
    if wire.provider_state.is_some() {
        return Err(ProviderError::Capability(
            "openai-chat does not support provider state".into(),
        ));
    }
    let usage = usage_from_wire(wire.usage);
    let choice = wire
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::Malformed("missing choices".into()))?;
    let message = choice.message;
    if message.input_request.is_some() && !compatibility_input_request {
        return Err(ProviderError::Malformed(
            "nonstandard message.input_request requires compatibility_input_request".into(),
        ));
    }
    let tool_calls = decode_tool_calls(message.tool_calls)?;
    let provider_state = message
        .reasoning_content
        .filter(|value| !value.is_empty())
        .map(|reasoning| serde_json::json!({ "reasoning_content": reasoning }));
    Ok(ProviderResponse {
        message: message.content,
        tool_calls,
        input_request: message.input_request,
        usage,
        finish_reason: choice.finish_reason.map(finish_reason),
        provider_state,
    })
}

fn usage_from_wire(usage: Option<WireUsage>) -> ProviderUsage {
    usage.map_or_else(ProviderUsage::default, |usage| ProviderUsage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        cached_tokens: usage
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens),
    })
}

fn finish_reason(reason: String) -> FinishReason {
    match reason.as_str() {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Other(reason),
    }
}

fn decode_tool_calls(values: Vec<WireCall>) -> Result<Vec<ToolCall>, ProviderError> {
    let calls = values
        .into_iter()
        .map(|call| {
            Ok(ToolCall {
                id: call.id,
                name: call.function.name,
                input: serde_json::from_str(&call.function.arguments)
                    .map_err(|error| ProviderError::Malformed(error.to_string()))?,
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    validate_tool_calls(&calls)?;
    Ok(calls)
}

fn validate_tool_calls(calls: &[ToolCall]) -> Result<(), ProviderError> {
    let mut ids = std::collections::BTreeSet::new();
    if calls
        .iter()
        .any(|call| !valid_tool_call_id(&call.id) || !ids.insert(call.id.clone()))
    {
        return Err(ProviderError::Malformed(
            "tool call ids must match [A-Za-z0-9_-]{1,256} and be unique".into(),
        ));
    }
    Ok(())
}

const MAX_SSE_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 256 * 1024;
const MAX_SSE_TOOL_CALLS: usize = 128;

#[derive(Debug, Deserialize)]
struct StreamWire {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}
#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}
#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}
#[derive(Debug, Deserialize)]
struct StreamToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: StreamFunction,
}
#[derive(Debug, Default, Deserialize)]
struct StreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}
#[derive(Default)]
struct StreamAssembly {
    content: String,
    reasoning_content: String,
    tools: Vec<StreamToolAssembly>,
    finish_reason: Option<String>,
    usage: Option<WireUsage>,
}
#[derive(Default)]
struct StreamToolAssembly {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

async fn stream_sse_events<F>(
    mut response: reqwest::Response,
    idle_timeout: Duration,
    context: &ProviderContext,
    mut on_event: F,
) -> Result<bool, ProviderError>
where
    F: FnMut(&str, &ProviderContext) -> Result<bool, ProviderError>,
{
    let mut buffer = Vec::<u8>::new();
    let mut data_lines = Vec::<String>::new();
    let mut total = 0_usize;
    let mut done = false;
    loop {
        let next = tokio::select! {
            chunk = response.chunk() => chunk.map_err(|error| ProviderError::Transport(error.to_string()))?,
            () = tokio::time::sleep(idle_timeout) => return Err(ProviderError::Timeout),
            () = context.cancellation.cancelled() => return Err(ProviderError::Cancelled),
        };
        let Some(chunk) = next else {
            break;
        };
        total = total.saturating_add(chunk.len());
        if total > MAX_SSE_BODY_BYTES {
            return Err(ProviderError::Malformed("SSE body exceeds limit".into()));
        }
        buffer.extend_from_slice(&chunk);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = buffer.drain(..=position).collect();
            let _ = line.pop();
            if line.last() == Some(&b'\r') {
                let _ = line.pop();
            }
            if line.is_empty() {
                if !data_lines.is_empty() {
                    done = on_event(&data_lines.join("\n"), context)? || done;
                    data_lines.clear();
                    if done {
                        break;
                    }
                }
                continue;
            }
            if line.starts_with(b":") {
                continue;
            }
            let Some(colon) = line.iter().position(|byte| *byte == b':') else {
                continue;
            };
            let field = &line[..colon];
            let value = &line[colon + 1..];
            if field == b"data" {
                let value = value.strip_prefix(b" ").unwrap_or(value);
                if value.len() > MAX_SSE_EVENT_BYTES {
                    return Err(ProviderError::Malformed("SSE event exceeds limit".into()));
                }
                data_lines.push(
                    std::str::from_utf8(value)
                        .map_err(|_| ProviderError::Malformed("SSE data is not UTF-8".into()))?
                        .to_owned(),
                );
            }
        }
        if done {
            break;
        }
    }
    if !data_lines.is_empty() && !done {
        done = on_event(&data_lines.join("\n"), context)?;
    }
    Ok(done)
}

async fn parse_openai_sse(
    response: reqwest::Response,
    idle_timeout: Duration,
    context: &ProviderContext,
) -> Result<ProviderResponse, ProviderError> {
    let mut assembly = StreamAssembly::default();
    let done = stream_sse_events(response, idle_timeout, context, |data, ctx| {
        consume_sse_event(data, &mut assembly, ctx)
    })
    .await?;
    if !done {
        return Err(ProviderError::Malformed(
            "SSE stream ended without [DONE]".into(),
        ));
    }
    let calls = assembly
        .tools
        .into_iter()
        .map(|tool| {
            let id = tool
                .id
                .ok_or_else(|| ProviderError::Malformed("stream tool call missing id".into()))?;
            let name = tool
                .name
                .ok_or_else(|| ProviderError::Malformed("stream tool call missing name".into()))?;
            Ok(ToolCall {
                id,
                name,
                input: serde_json::from_str(&tool.arguments)
                    .map_err(|error| ProviderError::Malformed(error.to_string()))?,
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    validate_tool_calls(&calls)?;
    let provider_state = (!assembly.reasoning_content.is_empty())
        .then(|| serde_json::json!({ "reasoning_content": assembly.reasoning_content }));
    Ok(ProviderResponse {
        message: (!assembly.content.is_empty()).then_some(assembly.content),
        tool_calls: calls,
        input_request: None,
        usage: usage_from_wire(assembly.usage),
        finish_reason: assembly.finish_reason.map(finish_reason),
        provider_state,
    })
}

fn consume_sse_event(
    data: &str,
    assembly: &mut StreamAssembly,
    context: &ProviderContext,
) -> Result<bool, ProviderError> {
    if data == "[DONE]" {
        return Ok(true);
    }
    let wire: StreamWire = serde_json::from_str(data)
        .map_err(|error| ProviderError::Malformed(format!("invalid SSE JSON: {error}")))?;
    if let Some(usage) = wire.usage {
        assembly.usage = Some(usage);
    }
    let Some(choice) = wire.choices.into_iter().next() else {
        return Ok(false);
    };
    if let Some(reason) = choice.finish_reason {
        assembly.finish_reason = Some(reason);
    }
    if let Some(reasoning) = choice.delta.reasoning_content
        && !reasoning.is_empty()
    {
        assembly.reasoning_content.push_str(&reasoning);
        if assembly.reasoning_content.len() > MAX_SSE_BODY_BYTES {
            return Err(ProviderError::Malformed(
                "stream reasoning content exceeds limit".into(),
            ));
        }
    }
    if let Some(delta) = choice.delta.content
        && !delta.is_empty()
    {
        assembly.content.push_str(&delta);
        if assembly.content.len() > MAX_SSE_BODY_BYTES {
            return Err(ProviderError::Malformed(
                "stream assistant content exceeds limit".into(),
            ));
        }
        emit_provider_event(context, ProviderEvent::AssistantDelta { text: delta });
    }
    for delta in choice.delta.tool_calls {
        if delta.index >= MAX_SSE_TOOL_CALLS {
            return Err(ProviderError::Malformed(
                "too many stream tool calls".into(),
            ));
        }
        while assembly.tools.len() <= delta.index {
            assembly.tools.push(StreamToolAssembly::default());
        }
        let target = &mut assembly.tools[delta.index];
        if let Some(id) = delta.id {
            if target.id.as_ref().is_some_and(|old| old != &id) {
                return Err(ProviderError::Malformed("stream tool id changed".into()));
            }
            target.id = Some(id);
        }
        if let Some(name) = delta.function.name {
            if target.name.as_ref().is_some_and(|old| old != &name) {
                return Err(ProviderError::Malformed("stream tool name changed".into()));
            }
            target.name = Some(name);
        }
        if let Some(arguments) = delta.function.arguments {
            target.arguments.push_str(&arguments);
            if target.arguments.len() > MAX_SSE_EVENT_BYTES {
                return Err(ProviderError::Malformed(
                    "stream tool arguments exceed limit".into(),
                ));
            }
        }
    }
    Ok(false)
}
#[allow(clippy::duration_suboptimal_units)]
async fn retry_pause(
    context: &ProviderContext,
    attempt: u32,
    retry_after: Option<Duration>,
) -> Result<(), ProviderError> {
    let delay = if let Some(server_delay) = retry_after {
        server_delay.min(Duration::from_secs(60))
    } else {
        let base_ms = 100_u64
            .saturating_mul(1_u64 << attempt.saturating_sub(1).min(4))
            .min(2_000);
        let jitter_cap = (base_ms / 4).max(1);
        let entropy = uuid::Uuid::now_v7().into_bytes();
        let rand_u16 = u64::from(u16::from_le_bytes([entropy[14], entropy[15]]));
        let jitter_ms = rand_u16 % (jitter_cap + 1);
        Duration::from_millis(base_ms.saturating_add(jitter_ms))
    };
    if Instant::now()
        .checked_add(delay)
        .is_none_or(|wake| wake >= context.deadline)
    {
        return Err(ProviderError::Timeout);
    }
    tokio::select! {
        () = tokio::time::sleep(delay) => Ok(()),
        () = context.cancellation.cancelled() => Err(ProviderError::Cancelled),
    }
}
impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &"[REDACTED]")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 16_384;
const MAX_SSE_CONTENT_BLOCKS: usize = 256;

#[derive(Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    timeout: Duration,
    max_attempts: u32,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    streaming: bool,
    prompt_caching: bool,
    bearer_auth: bool,
    headers: std::collections::BTreeMap<String, String>,
}

impl AnthropicProvider {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeout.min(Duration::from_secs(30)))
            .redirect(same_origin_redirect_policy())
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            endpoint: endpoint.into(),
            model: model.into(),
            api_key: api_key.into(),
            timeout,
            max_attempts: 1,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            streaming: true,
            prompt_caching: true,
            bearer_auth: false,
            headers: std::collections::BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn with_headers(mut self, headers: std::collections::BTreeMap<String, String>) -> Self {
        self.headers = headers
            .into_iter()
            .filter(|(name, _)| {
                !ANTHROPIC_RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
            })
            .collect();
        self
    }

    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    #[must_use]
    pub fn with_sampling_options(
        mut self,
        temperature: Option<f64>,
        max_tokens: Option<u32>,
    ) -> Self {
        self.temperature = temperature;
        self.max_tokens = max_tokens;
        self
    }

    #[must_use]
    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<String>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }

    #[must_use]
    pub fn with_streaming(mut self, enabled: bool) -> Self {
        self.streaming = enabled;
        self
    }

    #[must_use]
    pub fn with_prompt_caching(mut self, enabled: bool) -> Self {
        self.prompt_caching = enabled;
        self
    }

    #[must_use]
    pub fn with_bearer_auth(mut self, enabled: bool) -> Self {
        self.bearer_auth = enabled;
        self
    }

    fn request_builder(
        &self,
        session_ref: &str,
        thinking_enabled: bool,
    ) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("anthropic-version", ANTHROPIC_API_VERSION);
        builder = if self.bearer_auth {
            builder.bearer_auth(&self.api_key)
        } else {
            builder.header("x-api-key", &self.api_key)
        };
        if thinking_enabled
            && !self
                .headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("anthropic-beta"))
        {
            builder = builder.header("anthropic-beta", ANTHROPIC_INTERLEAVED_THINKING_BETA);
        }
        apply_headers(builder, &self.headers, session_ref)
    }
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &"[REDACTED]")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

fn sanitize_anthropic_tool_id(id: &str) -> String {
    use sha2::{Digest, Sha256};
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return id.to_owned();
    }
    let digest = Sha256::digest(id.as_bytes());
    format!("toolu_{digest:x}")[..38].to_owned()
}

fn resolve_anthropic_budgets(
    configured_max_tokens: Option<u32>,
    reasoning_effort: Option<&str>,
) -> (u32, Option<u32>) {
    let raw_thinking = reasoning_effort.and_then(|effort| {
        let normalized = effort.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" | "none" | "off" | "disabled" => None,
            "minimal" => Some(1_024),
            "low" => Some(4_096),
            "medium" => Some(10_000),
            "high" => Some(16_384),
            "xhigh" | "max" => Some(31_999),
            other => other.parse::<u32>().ok().filter(|&tokens| tokens >= 1_024),
        }
    });
    match (configured_max_tokens, raw_thinking) {
        (Some(max_tokens), Some(budget)) if max_tokens >= 2_048 => {
            (max_tokens, Some(budget.min(max_tokens - 1_024)))
        }
        (Some(max_tokens), _) => (max_tokens.max(1), None),
        (None, Some(budget)) => (
            ANTHROPIC_DEFAULT_MAX_TOKENS.max(budget.saturating_add(8_192)),
            Some(budget),
        ),
        (None, None) => (ANTHROPIC_DEFAULT_MAX_TOKENS, None),
    }
}

fn push_anthropic_turn(turns: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if let Some((last_role, last_blocks)) = turns.last_mut()
        && last_role == role
    {
        last_blocks.extend(blocks);
        return;
    }
    turns.push((role.to_owned(), blocks));
}

#[allow(clippy::too_many_lines)]
fn build_anthropic_payload(
    provider: &AnthropicProvider,
    request: &ProviderRequest,
) -> Result<(Value, bool), ProviderError> {
    let (max_tokens, thinking_budget) =
        resolve_anthropic_budgets(provider.max_tokens, provider.reasoning_effort.as_deref());
    let ephemeral = || serde_json::json!({ "type": "ephemeral" });
    let last_plain_user_idx = request
        .messages
        .iter()
        .rposition(|msg| matches!(msg, Message::User { .. }));

    let mut system_blocks = Vec::<Value>::new();
    let mut turns = Vec::<(String, Vec<Value>)>::new();
    for (msg_idx, message) in request.messages.iter().enumerate() {
        match message {
            Message::System { content } => {
                if !content.is_empty() {
                    system_blocks.push(serde_json::json!({
                        "type": "text",
                        "text": content,
                    }));
                }
            }
            Message::User { content } => {
                let text = if content.is_empty() {
                    " "
                } else {
                    content.as_str()
                };
                push_anthropic_turn(
                    &mut turns,
                    "user",
                    vec![serde_json::json!({ "type": "text", "text": text })],
                );
            }
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => {
                push_anthropic_turn(
                    &mut turns,
                    "user",
                    vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": sanitize_anthropic_tool_id(tool_call_id),
                        "content": content,
                    })],
                );
            }
            Message::Assistant {
                content,
                thinking_blocks,
                tool_calls,
                ..
            } => {
                let mut blocks = Vec::<Value>::new();
                let mut had_unsigned_thinking = false;
                for block in thinking_blocks {
                    match block {
                        ThinkingBlock::Thinking {
                            thinking,
                            signature: Some(sig),
                        } if !sig.is_empty() => {
                            blocks.push(serde_json::json!({
                                "type": "thinking",
                                "thinking": thinking,
                                "signature": sig,
                            }));
                        }
                        ThinkingBlock::RedactedThinking { data } if !data.is_empty() => {
                            blocks.push(serde_json::json!({
                                "type": "redacted_thinking",
                                "data": data,
                            }));
                        }
                        ThinkingBlock::Thinking { .. } | ThinkingBlock::RedactedThinking { .. } => {
                            had_unsigned_thinking = true;
                        }
                    }
                }
                let in_open_continuation =
                    last_plain_user_idx.is_none_or(|user_idx| msg_idx > user_idx);
                if in_open_continuation && !tool_calls.is_empty() && had_unsigned_thinking {
                    return Err(ProviderError::Malformed(
                        "signed thinking block could not be replayed verbatim after transcript secret redaction"
                            .into(),
                    ));
                }
                if let Some(text) = content.as_deref().filter(|s| !s.is_empty()) {
                    blocks.push(serde_json::json!({ "type": "text", "text": text }));
                }
                for call in tool_calls {
                    let input = if call.input.is_object() {
                        call.input.clone()
                    } else {
                        serde_json::json!({})
                    };
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": sanitize_anthropic_tool_id(&call.id),
                        "name": call.name,
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": " " }));
                }
                push_anthropic_turn(&mut turns, "assistant", blocks);
            }
        }
    }
    if turns.first().is_some_and(|(role, _)| role == "assistant") {
        turns.insert(
            0,
            (
                "user".to_owned(),
                vec![serde_json::json!({ "type": "text", "text": "[continued]" })],
            ),
        );
    }

    let mut wire_tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect();

    if provider.prompt_caching {
        if let Some(last_tool) = wire_tools.last_mut() {
            last_tool["cache_control"] = ephemeral();
        }
        if let Some(last_sys) = system_blocks.last_mut() {
            last_sys["cache_control"] = ephemeral();
        }
        let last_turn_idx = turns.len().checked_sub(1);
        let mut tagged_repo = false;
        for (turn_idx, (role, blocks)) in turns.iter_mut().enumerate() {
            if tagged_repo {
                break;
            }
            if role != "user" {
                continue;
            }
            let block_count = blocks.len();
            for (block_idx, block) in blocks.iter_mut().enumerate() {
                let is_repo = block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with("<repository-context>"));
                let is_overall_tail =
                    Some(turn_idx) == last_turn_idx && block_idx + 1 == block_count;
                if is_repo && !is_overall_tail {
                    block["cache_control"] = ephemeral();
                    tagged_repo = true;
                    break;
                }
            }
        }
        if let Some((_, blocks)) = turns.last_mut()
            && let Some(last_block) = blocks.last_mut()
        {
            last_block["cache_control"] = ephemeral();
        }
    }

    let wire_messages: Vec<Value> = turns
        .into_iter()
        .map(|(role, content)| serde_json::json!({ "role": role, "content": content }))
        .collect();

    let mut body = serde_json::json!({
        "model": provider.model,
        "max_tokens": max_tokens,
        "messages": wire_messages,
    });
    if !system_blocks.is_empty() {
        body["system"] = Value::Array(system_blocks);
    }
    if !wire_tools.is_empty() {
        body["tools"] = Value::Array(wire_tools);
        body["tool_choice"] = serde_json::json!({ "type": "auto" });
    }
    if let Some(budget) = thinking_budget {
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    } else if let Some(temperature) = provider.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if provider.streaming {
        body["stream"] = Value::Bool(true);
    }
    Ok((body, thinking_budget.is_some()))
}

impl Provider for AnthropicProvider {
    #[allow(clippy::too_many_lines)]
    fn complete(&self, request: ProviderRequest, context: ProviderContext) -> ProviderFuture<'_> {
        let this = self.clone();
        Box::pin(async move {
            if context.cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let (body, thinking_enabled) = build_anthropic_payload(&this, &request)?;
            let mut attempt = 1_u32;
            loop {
                if context.cancellation.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                emit_provider_event(&context, ProviderEvent::Attempt { number: attempt });
                let remaining = context
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .min(this.timeout);
                if remaining.is_zero() {
                    return Err(ProviderError::Timeout);
                }
                let sent = tokio::time::timeout(
                    remaining,
                    this.request_builder(&request.session_ref, thinking_enabled)
                        .json(&body)
                        .send(),
                )
                .await;
                let response = match sent {
                    Err(_) => return Err(ProviderError::Timeout),
                    Ok(Err(error)) if error.is_connect() && attempt < this.max_attempts => {
                        retry_pause(&context, attempt, None).await?;
                        attempt += 1;
                        continue;
                    }
                    Ok(Err(error)) => {
                        return Err(if error.is_timeout() {
                            ProviderError::Timeout
                        } else {
                            ProviderError::Transport(error.to_string())
                        });
                    }
                    Ok(Ok(response)) => response,
                };
                if !response.status().is_success() {
                    let status = response.status().as_u16();
                    if is_retryable_status(status) && attempt < this.max_attempts {
                        let retry_after = parse_retry_after(response.headers());
                        retry_pause(&context, attempt, retry_after).await?;
                        attempt += 1;
                        continue;
                    }
                    let request_id = extract_request_id(response.headers());
                    let error_body = tokio::time::timeout(this.timeout, response.bytes())
                        .await
                        .map_err(|_| ProviderError::Timeout)?
                        .map_err(|error| ProviderError::Transport(error.to_string()))?;
                    if this.streaming
                        && matches!(status, 400 | 404 | 415 | 422)
                        && error_body.is_empty()
                    {
                        let mut inline_body = body;
                        if let Some(obj) = inline_body.as_object_mut() {
                            obj.remove("stream");
                        }
                        let fallback = send_json_once(
                            &inline_body,
                            this.timeout,
                            &context,
                            this.request_builder(&request.session_ref, thinking_enabled),
                        )
                        .await?;
                        let wire: AnthropicWireMessage =
                            tokio::time::timeout(this.timeout, fallback.json())
                                .await
                                .map_err(|_| ProviderError::Timeout)?
                                .map_err(|error| ProviderError::Malformed(error.to_string()))?;
                        return decode_anthropic_message(wire);
                    }
                    return Err(classify_http_error(status, request_id, &error_body));
                }
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if this.streaming && content_type.contains("text/event-stream") {
                    let mut emitted_delta = false;
                    match parse_anthropic_sse(response, this.timeout, &context, &mut emitted_delta)
                        .await
                    {
                        Err(ProviderError::Http {
                            status: 529,
                            retryable: true,
                            ..
                        }) if !emitted_delta && attempt < this.max_attempts => {
                            retry_pause(&context, attempt, None).await?;
                            attempt += 1;
                            continue;
                        }
                        other => return other,
                    }
                }
                let wire: AnthropicWireMessage =
                    tokio::time::timeout(this.timeout, response.json())
                        .await
                        .map_err(|_| ProviderError::Timeout)?
                        .map_err(|error| ProviderError::Malformed(error.to_string()))?;
                return decode_anthropic_message(wire);
            }
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            parallel_tool_calls: true,
            input_request: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[allow(clippy::struct_field_names)]
struct AnthropicWireUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl AnthropicWireUsage {
    fn merge(&mut self, newer: Self) {
        if let Some(v) = newer.input_tokens {
            self.input_tokens = Some(v);
        }
        if let Some(v) = newer.output_tokens {
            self.output_tokens = Some(v);
        }
        if let Some(v) = newer.cache_creation_input_tokens {
            self.cache_creation_input_tokens = Some(v);
        }
        if let Some(v) = newer.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(v);
        }
    }

    fn into_provider_usage(self) -> ProviderUsage {
        let has_input = self.input_tokens.is_some()
            || self.cache_creation_input_tokens.is_some()
            || self.cache_read_input_tokens.is_some();
        let total_input = has_input.then(|| {
            self.input_tokens
                .unwrap_or(0)
                .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
                .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
        });
        let total_tokens = match (total_input, self.output_tokens) {
            (Some(inp), Some(out)) => Some(inp.saturating_add(out)),
            (Some(inp), None) => Some(inp),
            (None, Some(out)) => Some(out),
            (None, None) => None,
        };
        ProviderUsage {
            input_tokens: total_input,
            output_tokens: self.output_tokens,
            total_tokens,
            cached_tokens: self.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicWireMessage {
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicWireUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
}

fn anthropic_finish_reason(reason: String) -> FinishReason {
    match reason.as_str() {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Other(reason),
    }
}

fn build_anthropic_provider_state(
    reasoning_parts: &[String],
    thinking_blocks: &[ThinkingBlock],
) -> Option<Value> {
    let reasoning = reasoning_parts.join("\n\n");
    if reasoning.is_empty() && thinking_blocks.is_empty() {
        return None;
    }
    let mut state = serde_json::Map::new();
    if !reasoning.is_empty() {
        state.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if !thinking_blocks.is_empty()
        && let Ok(serialized) = serde_json::to_value(thinking_blocks)
    {
        state.insert("thinking_blocks".into(), serialized);
    }
    Some(Value::Object(state))
}

fn decode_anthropic_message(wire: AnthropicWireMessage) -> Result<ProviderResponse, ProviderError> {
    let mut text = String::new();
    let mut reasoning_parts = Vec::<String>::new();
    let mut thinking_blocks = Vec::<ThinkingBlock>::new();
    let mut tool_calls = Vec::<ToolCall>::new();
    for block in wire.content {
        match block.kind.as_str() {
            "text" => {
                if let Some(chunk) = block.text {
                    text.push_str(&chunk);
                }
            }
            "thinking" => {
                let thought = block.thinking.unwrap_or_default();
                if !thought.is_empty() {
                    reasoning_parts.push(thought.clone());
                }
                thinking_blocks.push(ThinkingBlock::Thinking {
                    thinking: thought,
                    signature: block.signature.filter(|sig| !sig.is_empty()),
                });
            }
            "redacted_thinking" => {
                if let Some(data) = block.data.filter(|d| !d.is_empty()) {
                    thinking_blocks.push(ThinkingBlock::RedactedThinking { data });
                }
            }
            "tool_use" => {
                let id = block
                    .id
                    .ok_or_else(|| ProviderError::Malformed("tool_use missing id".into()))?;
                let name = block
                    .name
                    .ok_or_else(|| ProviderError::Malformed("tool_use missing name".into()))?;
                let input = block.input.unwrap_or_else(|| serde_json::json!({}));
                if !input.is_object() {
                    return Err(ProviderError::Malformed(
                        "tool_use input must be a JSON object".into(),
                    ));
                }
                tool_calls.push(ToolCall { id, name, input });
            }
            _ => {}
        }
    }
    validate_tool_calls(&tool_calls)?;
    let provider_state = build_anthropic_provider_state(&reasoning_parts, &thinking_blocks);
    Ok(ProviderResponse {
        message: (!text.is_empty()).then_some(text),
        tool_calls,
        input_request: None,
        usage: wire.usage.map_or_else(
            ProviderUsage::default,
            AnthropicWireUsage::into_provider_usage,
        ),
        finish_reason: wire.stop_reason.map(anthropic_finish_reason),
        provider_state,
    })
}

enum AnthropicStreamBlock {
    Empty,
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
        initial_input: Option<Value>,
    },
}

#[derive(Default)]
struct AnthropicStreamAssembly {
    blocks: Vec<AnthropicStreamBlock>,
    stop_reason: Option<String>,
    usage: AnthropicWireUsage,
    saw_start: bool,
    emitted_delta: bool,
}

async fn parse_anthropic_sse(
    response: reqwest::Response,
    idle_timeout: Duration,
    context: &ProviderContext,
    emitted_delta_out: &mut bool,
) -> Result<ProviderResponse, ProviderError> {
    let mut assembly = AnthropicStreamAssembly::default();
    let stream_res = stream_sse_events(response, idle_timeout, context, |data, ctx| {
        consume_anthropic_sse_event(data, &mut assembly, ctx)
    })
    .await;
    *emitted_delta_out = assembly.emitted_delta;
    let done = stream_res?;
    if !done || !assembly.saw_start {
        return Err(ProviderError::Malformed(
            "Anthropic SSE stream ended without message_stop".into(),
        ));
    }
    let mut text = String::new();
    let mut reasoning_parts = Vec::<String>::new();
    let mut thinking_blocks = Vec::<ThinkingBlock>::new();
    let mut tool_calls = Vec::<ToolCall>::new();
    for block in assembly.blocks {
        match block {
            AnthropicStreamBlock::Empty => {}
            AnthropicStreamBlock::Text(chunk) => text.push_str(&chunk),
            AnthropicStreamBlock::Thinking {
                thinking,
                signature,
            } => {
                if !thinking.is_empty() {
                    reasoning_parts.push(thinking.clone());
                }
                thinking_blocks.push(ThinkingBlock::Thinking {
                    thinking,
                    signature: (!signature.is_empty()).then_some(signature),
                });
            }
            AnthropicStreamBlock::RedactedThinking { data } => {
                if !data.is_empty() {
                    thinking_blocks.push(ThinkingBlock::RedactedThinking { data });
                }
            }
            AnthropicStreamBlock::ToolUse {
                id,
                name,
                input_json,
                initial_input,
            } => {
                let input = if input_json.trim().is_empty() {
                    initial_input.unwrap_or_else(|| serde_json::json!({}))
                } else {
                    serde_json::from_str::<Value>(&input_json)
                        .map_err(|error| ProviderError::Malformed(error.to_string()))?
                };
                if !input.is_object() {
                    return Err(ProviderError::Malformed(
                        "stream tool_use input must be a JSON object".into(),
                    ));
                }
                tool_calls.push(ToolCall { id, name, input });
            }
        }
    }
    validate_tool_calls(&tool_calls)?;
    let provider_state = build_anthropic_provider_state(&reasoning_parts, &thinking_blocks);
    Ok(ProviderResponse {
        message: (!text.is_empty()).then_some(text),
        tool_calls,
        input_request: None,
        usage: assembly.usage.into_provider_usage(),
        finish_reason: assembly.stop_reason.map(anthropic_finish_reason),
        provider_state,
    })
}

#[allow(clippy::too_many_lines)]
fn consume_anthropic_sse_event(
    data: &str,
    assembly: &mut AnthropicStreamAssembly,
    context: &ProviderContext,
) -> Result<bool, ProviderError> {
    if data == "[DONE]" {
        return Ok(assembly.saw_start && assembly.stop_reason.is_some());
    }
    let event: Value = serde_json::from_str(data).map_err(|error| {
        ProviderError::Malformed(format!("invalid Anthropic SSE JSON: {error}"))
    })?;
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "message_stop" => Ok(true),
        "error" => {
            if body_signals_context_overflow(data.as_bytes()) {
                return Err(ProviderError::ContextOverflow {
                    request_id: String::new(),
                });
            }
            let err_type = event
                .get("error")
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if err_type == "overloaded_error" {
                return Err(ProviderError::Http {
                    status: 529,
                    request_id: String::new(),
                    retryable: true,
                });
            }
            let msg = event
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("stream error");
            Err(ProviderError::Malformed(format!(
                "Anthropic stream error: {msg}"
            )))
        }
        "message_start" => {
            assembly.saw_start = true;
            if let Some(usage_val) = event.get("message").and_then(|m| m.get("usage"))
                && let Ok(usage) = serde_json::from_value::<AnthropicWireUsage>(usage_val.clone())
            {
                assembly.usage.merge(usage);
            }
            Ok(false)
        }
        "content_block_start" => {
            let index = event
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|idx| usize::try_from(idx).ok())
                .ok_or_else(|| {
                    ProviderError::Malformed("content_block_start missing index".into())
                })?;
            if index >= MAX_SSE_CONTENT_BLOCKS {
                return Err(ProviderError::Malformed(
                    "too many Anthropic content blocks".into(),
                ));
            }
            while assembly.blocks.len() <= index {
                assembly.blocks.push(AnthropicStreamBlock::Empty);
            }
            let block: AnthropicContentBlock = event
                .get("content_block")
                .cloned()
                .ok_or_else(|| ProviderError::Malformed("missing content_block".into()))
                .and_then(|val| {
                    serde_json::from_value(val)
                        .map_err(|err| ProviderError::Malformed(err.to_string()))
                })?;
            assembly.blocks[index] = match block.kind.as_str() {
                "text" => {
                    let initial = block.text.unwrap_or_default();
                    if !initial.is_empty() {
                        assembly.emitted_delta = true;
                        emit_provider_event(
                            context,
                            ProviderEvent::AssistantDelta {
                                text: initial.clone(),
                            },
                        );
                    }
                    AnthropicStreamBlock::Text(initial)
                }
                "thinking" => AnthropicStreamBlock::Thinking {
                    thinking: block.thinking.unwrap_or_default(),
                    signature: block.signature.unwrap_or_default(),
                },
                "redacted_thinking" => AnthropicStreamBlock::RedactedThinking {
                    data: block.data.unwrap_or_default(),
                },
                "tool_use" => {
                    let id = block
                        .id
                        .ok_or_else(|| ProviderError::Malformed("tool_use missing id".into()))?;
                    let name = block
                        .name
                        .ok_or_else(|| ProviderError::Malformed("tool_use missing name".into()))?;
                    AnthropicStreamBlock::ToolUse {
                        id,
                        name,
                        input_json: String::new(),
                        initial_input: block.input,
                    }
                }
                _ => AnthropicStreamBlock::Empty,
            };
            Ok(false)
        }
        "content_block_delta" => {
            let index = event
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|idx| usize::try_from(idx).ok())
                .ok_or_else(|| {
                    ProviderError::Malformed("content_block_delta missing index".into())
                })?;
            let target = assembly.blocks.get_mut(index).ok_or_else(|| {
                ProviderError::Malformed("content_block_delta for unstarted block".into())
            })?;
            let delta = event.get("delta").ok_or_else(|| {
                ProviderError::Malformed("content_block_delta missing delta".into())
            })?;
            let delta_type = delta
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match (target, delta_type) {
                (AnthropicStreamBlock::Text(buf), "text_delta") => {
                    if let Some(chunk) = delta.get("text").and_then(Value::as_str)
                        && !chunk.is_empty()
                    {
                        buf.push_str(chunk);
                        if buf.len() > MAX_SSE_BODY_BYTES {
                            return Err(ProviderError::Malformed(
                                "stream assistant content exceeds limit".into(),
                            ));
                        }
                        assembly.emitted_delta = true;
                        emit_provider_event(
                            context,
                            ProviderEvent::AssistantDelta {
                                text: chunk.to_owned(),
                            },
                        );
                    }
                }
                (AnthropicStreamBlock::Thinking { thinking, .. }, "thinking_delta") => {
                    if let Some(chunk) = delta.get("thinking").and_then(Value::as_str) {
                        thinking.push_str(chunk);
                        if thinking.len() > MAX_SSE_BODY_BYTES {
                            return Err(ProviderError::Malformed(
                                "stream thinking content exceeds limit".into(),
                            ));
                        }
                    }
                }
                (AnthropicStreamBlock::Thinking { signature, .. }, "signature_delta") => {
                    if let Some(chunk) = delta.get("signature").and_then(Value::as_str) {
                        signature.push_str(chunk);
                    }
                }
                (AnthropicStreamBlock::ToolUse { input_json, .. }, "input_json_delta") => {
                    if let Some(chunk) = delta.get("partial_json").and_then(Value::as_str) {
                        input_json.push_str(chunk);
                        if input_json.len() > MAX_SSE_EVENT_BYTES {
                            return Err(ProviderError::Malformed(
                                "stream tool_use partial_json exceeds limit".into(),
                            ));
                        }
                    }
                }
                _ => {}
            }
            Ok(false)
        }
        "message_delta" => {
            if let Some(reason) = event
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
            {
                assembly.stop_reason = Some(reason.to_owned());
            }
            if let Some(usage_val) = event.get("usage")
                && let Ok(usage) = serde_json::from_value::<AnthropicWireUsage>(usage_val.clone())
            {
                assembly.usage.merge(usage);
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}
#[cfg(test)]
fn tool_schema(name: &str) -> Value {
    match name {
        "read_file" => {
            serde_json::json!({"type":"object","required":["path"],"properties":{"path":path_schema(),"offset":{"type":"integer","minimum":1,"maximum":1_000_000},"limit":{"type":"integer","minimum":1,"maximum":10_000},"max_output":output_cap_schema()},"additionalProperties":false})
        }
        "list_directory" => {
            serde_json::json!({"type":"object","required":[],"properties":{"path":path_schema(),"max_depth":{"type":"integer","minimum":1,"maximum":8},"glob":{"type":"string","minLength":1,"maxLength":1024},"max_entries":{"type":"integer","minimum":1,"maximum":10_000}},"additionalProperties":false})
        }
        "search" => {
            serde_json::json!({"type":"object","required":["query"],"properties":{"query":{"type":"string","minLength":1,"maxLength":4096},"path":path_schema(),"glob":{"type":"string","minLength":1,"maxLength":1024},"regex":{"type":"boolean"},"max_results":{"type":"integer","minimum":1,"maximum":10_000},"max_output":output_cap_schema()},"additionalProperties":false})
        }
        "read_project_manifest" | "git_diff" => {
            serde_json::json!({"type":"object","required":[],"properties":{"max_output":output_cap_schema()},"additionalProperties":false})
        }
        "edit_file" => {
            serde_json::json!({"type":"object","required":["path","after"],"properties":{"path":path_schema(),"before":{"type":"string","minLength":1},"anchor":{"type":"string","minLength":1},"after":{"type":"string"},"precondition":digest_schema()},"additionalProperties":false})
        }
        "write_file" => {
            serde_json::json!({"type":"object","required":["path","content"],"properties":{"path":path_schema(),"content":{"type":"string"},"create_intent":{"type":"boolean"},"precondition":digest_schema()},"additionalProperties":false})
        }
        "process" => {
            serde_json::json!({"type":"object","required":[],"properties":{"argv":{"type":"array","minItems":1,"maxItems":256,"items":{"type":"string","minLength":1,"maxLength":4096}},"shell":{"type":"string","minLength":1,"maxLength":16384},"cwd":path_schema(),"env":{"type":"object","maxProperties":128,"additionalProperties":{"type":"string","maxLength":16384}},"timeout_ms":{"type":"integer","minimum":1,"maximum":600_000},"grace_ms":{"type":"integer","minimum":0,"maximum":30_000},"stdout_cap":{"type":"integer","minimum":1,"maximum":1_048_576},"stderr_cap":{"type":"integer","minimum":1,"maximum":1_048_576}},"additionalProperties":false})
        }
        _ => {
            serde_json::json!({"type":"object","required":[],"properties":{},"additionalProperties":false})
        }
    }
}

#[cfg(test)]
fn path_schema() -> Value {
    serde_json::json!({"type":"string","minLength":1,"maxLength":4096})
}

#[cfg(test)]
fn output_cap_schema() -> Value {
    serde_json::json!({"type":"integer","minimum":1,"maximum":65536})
}

#[cfg(test)]
fn digest_schema() -> Value {
    serde_json::json!({"type":"string","pattern":"^[0-9a-f]{64}$"})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };
    fn context() -> ProviderContext {
        ProviderContext {
            deadline: Instant::now() + Duration::from_secs(2),
            cancellation: latte_engine::CancellationToken::new(),
            events: None,
        }
    }
    fn server(status: &str, body: &str, delay: u64) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let body = body.to_owned();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer);
            thread::sleep(Duration::from_millis(delay));
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
        });
        format!("http://{address}")
    }

    fn capturing_server(body: &str) -> (String, mpsc::Receiver<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_owned();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 16 * 1024];
            let count = socket.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            tx.send(serde_json::from_slice(&request[body_start..]).unwrap())
                .unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), rx)
    }

    #[test]
    fn context_overflow_classification_covers_codes_messages_and_plain_text() {
        let rid = String::new();
        // Structured code variants emitted by OpenAI-family servers.
        for body in [
            r#"{"error":{"code":"context_length_exceeded","message":"bad"}}"#,
            r#"{"error":{"type":"string_above_max_length","message":"x"}}"#,
        ] {
            assert!(matches!(
                classify_http_error(400, rid.clone(), body.as_bytes()),
                ProviderError::ContextOverflow { .. }
            ));
        }
        // Code absent, message carries the marker (proxies, DeepSeek-style).
        let body = r#"{"error":{"message":"This model's maximum context length is 8192 tokens."}}"#;
        assert!(matches!(
            classify_http_error(400, rid.clone(), body.as_bytes()),
            ProviderError::ContextOverflow { .. }
        ));
        // 413 with a plain-text marker still classifies.
        let body = "error: request exceeds the context window";
        assert!(matches!(
            classify_http_error(413, rid.clone(), body.as_bytes()),
            ProviderError::ContextOverflow { .. }
        ));
        // A generic bad request stays a plain HTTP error.
        let body = r#"{"error":{"code":"invalid_request","message":"model not found"}}"#;
        assert!(matches!(
            classify_http_error(400, rid.clone(), body.as_bytes()),
            ProviderError::Http { status: 400, .. }
        ));
        // Overflow-shaped body on an unrelated status is never reclassified.
        let body = r#"{"error":{"code":"context_length_exceeded"}}"#;
        assert!(matches!(
            classify_http_error(500, rid.clone(), body.as_bytes()),
            ProviderError::Http { status: 500, .. }
        ));
        // Transient statuses keep their retryability verdict.
        assert!(matches!(
            classify_http_error(503, rid, b"busy"),
            ProviderError::Http {
                status: 503,
                retryable: true,
                ..
            }
        ));
    }
    fn sequence_server(responses: Vec<(&str, &str)>) -> (String, mpsc::Receiver<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let responses: Vec<_> = responses
            .into_iter()
            .map(|(s, b)| (s.to_owned(), b.to_owned()))
            .collect();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for (index, (status, body)) in responses.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 16 * 1024];
                let _ = socket.read(&mut buffer);
                tx.send(index + 1).unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}"), rx)
    }

    /// Captures the raw request head so a test can assert on wire headers
    /// rather than on the builder that produced them.
    fn header_capturing_server(
        responses: Vec<(&str, &str)>,
    ) -> (String, mpsc::Receiver<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let responses: Vec<_> = responses
            .into_iter()
            .map(|(status, body)| (status.to_owned(), body.to_owned()))
            .collect();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 16 * 1024];
                let count = socket.read(&mut buffer).unwrap();
                let head = String::from_utf8_lossy(&buffer[..count]);
                let head = head.split("\r\n\r\n").next().unwrap_or_default();
                tx.send(
                    head.lines()
                        .skip(1)
                        .map(|line| line.trim().to_ascii_lowercase())
                        .collect(),
                )
                .unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        (format!("http://{address}"), rx)
    }

    fn sse_server(chunks: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 16 * 1024];
            let _ = socket.read(&mut request);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            for chunk in chunks {
                socket.write_all(&chunk).unwrap();
                socket.flush().unwrap();
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn normalizes_usage_finish_reason_and_retries_only_eligible_statuses() {
        let ok = r#"{"choices":[{"finish_reason":"length","message":{"content":"ok"}}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":2}}}"#;
        let (endpoint, attempts) =
            sequence_server(vec![("503 Service Unavailable", "{}"), ("200 OK", ok)]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.usage,
            ProviderUsage {
                input_tokens: Some(7),
                output_tokens: Some(3),
                total_tokens: Some(10),
                cached_tokens: Some(2)
            }
        );
        assert_eq!(response.finish_reason, Some(FinishReason::Length));
        assert_eq!(attempts.recv().unwrap(), 1);
        assert_eq!(attempts.recv().unwrap(), 2);

        let (endpoint, attempts) =
            sequence_server(vec![("401 Unauthorized", "{}"), ("200 OK", ok)]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        assert!(matches!(
            provider
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![]
                    },
                    context()
                )
                .await,
            Err(ProviderError::Http { status: 401, .. })
        ));
        assert_eq!(attempts.recv().unwrap(), 1);
        assert!(attempts.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test]
    async fn sse_handles_crlf_comments_multidata_utf8_splits_and_real_deltas() {
        #[derive(Default)]
        struct Events(Mutex<Vec<ProviderEvent>>);
        impl ProviderEventSink for Events {
            fn observe(&self, event: ProviderEvent) {
                self.0.lock().unwrap().push(event);
            }
        }
        let payload = concat!(
            ": keepalive\r\n\r\n",
            "data: {\"choices\":[\r\n",
            "data: {\"delta\":{\"content\":\"hé\"}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\",\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"a\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let mut chunks = Vec::new();
        let bytes = payload.as_bytes();
        for part in bytes.chunks(7) {
            chunks.push(part.to_vec());
        }
        let events = Arc::new(Events::default());
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_secs(2),
                    cancellation: latte_engine::CancellationToken::new(),
                    events: Some(events.clone()),
                },
            )
            .await
            .unwrap();
        assert_eq!(response.message.as_deref(), Some("héllo"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            response.tool_calls[0].input,
            serde_json::json!({"path":"a"})
        );
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        for _ in 0..100 {
            if events.0.lock().unwrap().iter().any(
                |event| matches!(event, ProviderEvent::AssistantDelta { text } if text == "llo"),
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            events.0.lock().unwrap().iter().any(
                |event| matches!(event, ProviderEvent::AssistantDelta { text } if text == "llo")
            )
        );
    }

    #[tokio::test]
    async fn streaming_falls_back_once_only_for_zero_body_unsupported_response() {
        let (endpoint, attempts) = sequence_server(vec![
            ("400 Bad Request", ""),
            (
                "200 OK",
                r#"{"choices":[{"message":{"content":"inline"}}]}"#,
            ),
        ]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true)
            .with_max_attempts(3);
        let output = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(output.message.as_deref(), Some("inline"));
        assert_eq!(attempts.recv().unwrap(), 1);
        assert_eq!(attempts.recv().unwrap(), 2);
        assert!(attempts.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test]
    async fn maps_all_chat_finish_reasons() {
        for (wire, expected) in [
            ("stop", FinishReason::Stop),
            ("tool_calls", FinishReason::ToolCalls),
            ("length", FinishReason::Length),
            ("content_filter", FinishReason::ContentFilter),
            ("vendor", FinishReason::Other("vendor".into())),
        ] {
            let body = format!(
                r#"{{"choices":[{{"finish_reason":"{wire}","message":{{"content":"ok"}}}}]}}"#
            );
            let provider =
                OpenAiProvider::new(server("200 OK", &body, 0), "m", "k", Duration::from_secs(1))
                    .unwrap();
            let response = provider
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![],
                    },
                    context(),
                )
                .await
                .unwrap();
            assert_eq!(response.finish_reason, Some(expected));
            assert!(response.provider_state.is_none());
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn retry_is_bounded_cancel_aware_and_never_retries_malformed_success() {
        let (endpoint, attempts) = sequence_server(vec![
            ("503 Service Unavailable", "{}"),
            ("503 Service Unavailable", "{}"),
        ]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        assert!(matches!(
            provider
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![]
                    },
                    context()
                )
                .await,
            Err(ProviderError::Http { status: 503, .. })
        ));
        assert_eq!(attempts.recv().unwrap(), 1);
        assert_eq!(attempts.recv().unwrap(), 2);

        let (endpoint, attempts) = sequence_server(vec![
            ("200 OK", "{"),
            (
                "200 OK",
                r#"{"choices":[{"message":{"content":"unexpected"}}]}"#,
            ),
        ]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        assert!(matches!(
            provider
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![]
                    },
                    context()
                )
                .await,
            Err(ProviderError::Malformed(_))
        ));
        assert_eq!(attempts.recv().unwrap(), 1);
        assert!(attempts.recv_timeout(Duration::from_millis(50)).is_err());

        let (endpoint, attempts) = sequence_server(vec![
            ("503 Service Unavailable", "{}"),
            (
                "200 OK",
                r#"{"choices":[{"message":{"content":"unexpected"}}]}"#,
            ),
        ]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        let cancellation = latte_engine::CancellationToken::new();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.cancel();
        });
        let result = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_secs(2),
                    cancellation,
                    events: None,
                },
            )
            .await;
        assert!(matches!(result, Err(ProviderError::Cancelled)));
        assert_eq!(attempts.recv().unwrap(), 1);
        assert!(attempts.recv_timeout(Duration::from_millis(50)).is_err());

        let (endpoint, attempts) = sequence_server(vec![
            ("503 Service Unavailable", "{}"),
            (
                "200 OK",
                r#"{"choices":[{"message":{"content":"unexpected"}}]}"#,
            ),
        ]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        let result = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_millis(50),
                    cancellation: latte_engine::CancellationToken::new(),
                    events: None,
                },
            )
            .await;
        assert!(matches!(result, Err(ProviderError::Timeout)));
        assert_eq!(attempts.recv().unwrap(), 1);
        assert!(attempts.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test]
    async fn chat_rejects_provider_state_explicitly() {
        let provider = OpenAiProvider::new(
            server(
                "200 OK",
                r#"{"choices":[{"message":{"content":"ok"}}],"provider_state":{"cursor":"x"}}"#,
                0,
            ),
            "m",
            "k",
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            provider
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![]
                    },
                    context()
                )
                .await,
            Err(ProviderError::Capability(_))
        ));
    }
    #[tokio::test]
    async fn parses_structured_response_and_redacts_auth() {
        let (endpoint, captured) = capturing_server(
            r#"{"choices":[{"message":{"content":"ok","tool_calls":[{"id":"1","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]}}]}"#,
        );
        let provider =
            OpenAiProvider::new(endpoint, "m", "super-secret", Duration::from_secs(1)).unwrap();
        let tools = [ToolDescriptor {
            name: "read_file".into(),
            description: "Engine-owned read operation".into(),
            input_schema: tool_schema("read_file"),
            version: 1,
            effect: "read".into(),
        }];
        let messages = vec![
            Message::Assistant {
                content: None,
                reasoning_content: None,
                thinking_blocks: vec![],
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path":"a"}),
                }],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                name: Some("read_file".into()),
                content: "result".into(),
            },
        ];
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages,
                    tools: tools.into(),
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(response.tool_calls[0].name, "read_file");
        let outbound = captured.recv().unwrap();
        assert_eq!(outbound["model"], "m");
        assert_eq!(outbound["tool_choice"], "auto");
        assert_eq!(outbound["tools"].as_array().unwrap().len(), 1);
        assert_eq!(outbound["messages"][0]["role"], "assistant");
        assert!(outbound["messages"][0]["content"].is_null());
        assert_eq!(outbound["messages"][0]["tool_calls"][0]["id"], "call-1");
        assert_eq!(outbound["messages"][1]["tool_call_id"], "call-1");
        assert_eq!(
            outbound["tools"][0],
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Engine-owned read operation",
                    "parameters": tool_schema("read_file")
                }
            })
        );
        let debug = format!("{provider:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("super-secret"));
    }
    #[tokio::test]
    async fn model_options_are_sent_only_when_configured() {
        let response = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
        let (endpoint, captured) = capturing_server(response);
        OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_sampling_options(Some(0.25), Some(321))
            .with_reasoning_effort(Some("high".into()))
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        let configured = captured.recv().unwrap();
        assert_eq!(configured["temperature"], 0.25);
        assert_eq!(configured["max_tokens"], 321);
        assert_eq!(configured["reasoning_effort"], "high");

        let (endpoint, captured) = capturing_server(response);
        OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        let absent = captured.recv().unwrap();
        assert!(absent.get("temperature").is_none());
        assert!(absent.get("max_tokens").is_none());
        assert!(absent.get("reasoning_effort").is_none());
    }
    #[tokio::test]
    async fn classifies_malformed_http_error_and_timeout() {
        let duplicate = OpenAiProvider::new(server("200 OK", r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"x","function":{"name":"read_file","arguments":"{}"}},{"id":"x","function":{"name":"read_file","arguments":"{}"}}]}}]}"#, 0), "m", "k", Duration::from_secs(1)).unwrap();
        assert!(matches!(
            duplicate
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![]
                    },
                    context()
                )
                .await,
            Err(ProviderError::Malformed(_))
        ));
        let malformed =
            OpenAiProvider::new(server("200 OK", "{}", 0), "m", "k", Duration::from_secs(1))
                .unwrap();
        assert!(matches!(
            malformed
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![]
                    },
                    context()
                )
                .await,
            Err(ProviderError::Malformed(_))
        ));
        let error = OpenAiProvider::new(
            server("500 Internal Server Error", "{}", 0),
            "m",
            "k",
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            error
                .complete(
                    ProviderRequest {
                        session_ref: "session-ref".into(),
                        messages: vec![],
                        tools: vec![]
                    },
                    context()
                )
                .await,
            Err(ProviderError::Http { .. })
        ));
        let slow = OpenAiProvider::new(
            server("200 OK", "{}", 200),
            "m",
            "k",
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(matches!(
            slow.complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![]
                },
                context()
            )
            .await,
            Err(ProviderError::Timeout)
        ));
    }

    #[test]
    fn every_advertised_tool_has_an_exact_bounded_schema() {
        for name in [
            "read_file",
            "list_directory",
            "search",
            "read_project_manifest",
            "edit_file",
            "write_file",
            "git_diff",
            "process",
        ] {
            let schema = tool_schema(name);
            assert_eq!(schema["type"], "object", "{name}");
            assert_eq!(schema["additionalProperties"], false, "{name}");
            assert!(schema.get("required").is_some(), "{name}");
            assert!(schema["properties"].is_object(), "{name}");
        }
        assert_eq!(
            tool_schema("read_file")["properties"]["max_output"]["maximum"],
            65_536
        );
        assert_eq!(
            tool_schema("list_directory")["properties"]["max_entries"]["maximum"],
            10_000
        );
        assert_eq!(
            tool_schema("search")["properties"]["max_results"]["minimum"],
            1
        );
        assert_eq!(
            tool_schema("process")["properties"]["timeout_ms"]["maximum"],
            600_000
        );
        assert!(tool_schema("process").get("oneOf").is_none());
        assert!(tool_schema("edit_file").get("anyOf").is_none());
        assert_eq!(
            tool_schema("process")["properties"]["grace_ms"]["minimum"],
            0
        );
        assert_eq!(
            tool_schema("edit_file")["properties"]["precondition"]["pattern"],
            "^[0-9a-f]{64}$"
        );
    }

    #[test]
    fn tool_call_ids_follow_safe_openai_chat_grammar() {
        assert!(valid_tool_call_id("call_abc-123_DEF"));
        assert!(valid_tool_call_id(&"x".repeat(256)));
        assert!(!valid_tool_call_id(""));
        assert!(!valid_tool_call_id(&"x".repeat(257)));
        assert!(!valid_tool_call_id("bad\ncall"));
        assert!(!valid_tool_call_id("bad\u{85}call"));
        assert!(!valid_tool_call_id("token=value"));
        assert!(!valid_tool_call_id("call:unsafe"));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn local_provider_boundaries_fail_closed_before_network_and_preserve_wire_roles() {
        let request = ProviderRequest {
            session_ref: "session-ref".into(),
            messages: vec![],
            tools: vec![],
        };
        let fake = FakeProvider::default();
        fake.push_error("offline");
        assert!(matches!(
            fake.complete(request.clone(), context()).await,
            Err(ProviderError::Transport(message)) if message == "offline"
        ));
        assert!(matches!(
            fake.complete(request.clone(), context()).await,
            Err(ProviderError::Malformed(message)) if message == "fake provider exhausted"
        ));
        assert_eq!(
            fake.capabilities(),
            ProviderCapabilities {
                tools: true,
                parallel_tool_calls: true,
                input_request: true,
            }
        );

        let provider = OpenAiProvider::new(
            "http://127.0.0.1:1",
            "model",
            "private-key",
            Duration::from_secs(1),
        )
        .unwrap();
        let cancelled = latte_engine::CancellationToken::new();
        cancelled.cancel();
        let cancelled_context = ProviderContext {
            deadline: Instant::now() + Duration::from_secs(1),
            cancellation: cancelled,
            events: None,
        };
        assert!(matches!(
            provider
                .complete(request.clone(), cancelled_context.clone())
                .await,
            Err(ProviderError::Cancelled)
        ));
        assert!(matches!(
            complete_inline_once(
                &provider,
                serde_json::json!({}),
                "session-ref",
                &cancelled_context
            )
            .await,
            Err(ProviderError::Cancelled)
        ));
        assert!(matches!(
            retry_pause(&cancelled_context, 1, Some(Duration::from_millis(10))).await,
            Err(ProviderError::Cancelled)
        ));

        let expired_context = ProviderContext {
            deadline: Instant::now(),
            cancellation: latte_engine::CancellationToken::new(),
            events: None,
        };
        assert!(matches!(
            provider.complete(request, expired_context.clone()).await,
            Err(ProviderError::Timeout)
        ));
        assert!(matches!(
            complete_inline_once(
                &provider,
                serde_json::json!({}),
                "session-ref",
                &expired_context
            )
            .await,
            Err(ProviderError::Timeout)
        ));
        assert!(matches!(
            retry_pause(&expired_context, 5, None).await,
            Err(ProviderError::Timeout)
        ));

        for message in [
            Message::System {
                content: "system".into(),
            },
            Message::User {
                content: "user".into(),
            },
            Message::Assistant {
                content: Some("assistant".into()),
                reasoning_content: None,
                thinking_blocks: vec![],
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path":"a.txt"}),
                }],
            },
            Message::Tool {
                tool_call_id: "call_1".into(),
                name: Some("read_file".into()),
                content: "contents".into(),
            },
        ] {
            let role = match &message {
                Message::System { .. } => "system",
                Message::User { .. } => "user",
                Message::Assistant { .. } => "assistant",
                Message::Tool { .. } => "tool",
            };
            assert!(message.is_role(role));
            assert_eq!(
                serde_json::to_value(wire_message(&message).unwrap()).unwrap()["role"],
                role
            );
        }

        // A plain assistant answer must not carry `tool_calls: []`. Providers
        // that reject an empty array would fail every follow-up turn of a
        // Session whose history contains one — absence and emptiness are not
        // interchangeable here.
        let plain = serde_json::to_value(
            wire_message(&Message::Assistant {
                content: Some("just an answer".into()),
                reasoning_content: None,
                thinking_blocks: vec![],
                tool_calls: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            plain.get("tool_calls").is_none(),
            "an assistant turn with no calls must omit the key: {plain}"
        );
        let calling = serde_json::to_value(
            wire_message(&Message::Assistant {
                content: None,
                reasoning_content: Some("thinking step".into()),
                thinking_blocks: vec![],
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            calling["tool_calls"].as_array().map(Vec::len),
            Some(1),
            "a real call is still serialized: {calling}"
        );
        assert_eq!(calling["reasoning_content"], "thinking step");

        let missing: Wire = serde_json::from_value(serde_json::json!({"choices":[]})).unwrap();
        assert!(matches!(
            decode_wire(missing, false),
            Err(ProviderError::Malformed(message)) if message == "missing choices"
        ));
        let input_wire = || {
            serde_json::from_value::<Wire>(serde_json::json!({
                "choices":[{"message":{"input_request":{
                    "id":"question-1","prompt":"continue?","secret":false
                }}}]
            }))
            .unwrap()
        };
        assert!(matches!(
            decode_wire(input_wire(), false),
            Err(ProviderError::Malformed(message)) if message.contains("compatibility_input_request")
        ));
        assert_eq!(
            decode_wire(input_wire(), true)
                .unwrap()
                .input_request
                .unwrap()
                .id,
            "question-1"
        );
        assert!(!format!("{provider:?}").contains("private-key"));
        assert!(!provider.capabilities().input_request);
        assert!(
            provider
                .with_compatibility_input_request(true)
                .capabilities()
                .input_request
        );
    }

    // -- Message / Provider trait coverage ----------------------------------

    #[test]
    fn message_content_covers_all_variants() {
        assert_eq!(
            Message::System {
                content: "s".into()
            }
            .content(),
            Some("s")
        );
        assert_eq!(
            Message::User {
                content: "u".into()
            }
            .content(),
            Some("u")
        );
        assert_eq!(
            Message::Tool {
                tool_call_id: "t".into(),
                name: None,
                content: "r".into(),
            }
            .content(),
            Some("r")
        );
        assert_eq!(
            Message::Assistant {
                content: Some("a".into()),
                reasoning_content: None,
                thinking_blocks: vec![],
                tool_calls: vec![],
            }
            .content(),
            Some("a")
        );
        assert_eq!(
            Message::Assistant {
                content: None,
                reasoning_content: None,
                thinking_blocks: vec![],
                tool_calls: vec![],
            }
            .content(),
            None
        );
    }

    #[test]
    fn default_provider_capabilities_are_full() {
        struct BareProvider;
        impl Provider for BareProvider {
            fn complete(&self, _: ProviderRequest, _: ProviderContext) -> ProviderFuture<'_> {
                Box::pin(async {
                    Ok(ProviderResponse {
                        message: None,
                        tool_calls: vec![],
                        input_request: None,
                        usage: ProviderUsage::default(),
                        finish_reason: None,
                        provider_state: None,
                    })
                })
            }
        }
        let caps = BareProvider.capabilities();
        assert!(caps.tools && caps.parallel_tool_calls && caps.input_request);
    }

    #[test]
    fn tool_schema_default_covers_unknown_tools() {
        let schema = tool_schema("unknown_tool");
        assert!(schema.is_object());
    }

    // -- HTTP error paths ----------------------------------------------------

    fn header_server(status: &str, body: &str, headers: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let body = body.to_owned();
        let headers = headers.to_owned();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn http_timeout_returns_timeout_error() {
        let endpoint = server("200 OK", "{}", 500);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_millis(50)).unwrap();
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Timeout));
    }

    #[tokio::test]
    async fn connect_error_retries_before_transport_failure() {
        // Bind-and-drop gives a port that refuses connections on every
        // platform (port 1 is handled inconsistently by Windows http.sys).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let provider = OpenAiProvider::new(
            format!("http://127.0.0.1:{port}"),
            "m",
            "k",
            Duration::from_secs(1),
        )
        .unwrap()
        .with_max_attempts(2);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        // A refused connection is a Transport error; on some Windows
        // configurations the OS withholds the RST and the request times out
        // instead. Both outcomes prove the retry path ran and propagated the
        // failure rather than panicking or returning a success.
        assert!(
            matches!(err, ProviderError::Transport(_) | ProviderError::Timeout),
            "expected transport or timeout error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn retry_after_header_controls_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            // First request: 503 with Retry-After: 0 (no wait).
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut buffer);
            let _ = socket.write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            );
            // Second request: 200.
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut buffer);
            let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
            let _ = socket.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        let provider = OpenAiProvider::new(
            format!("http://{address}"),
            "m",
            "k",
            Duration::from_secs(1),
        )
        .unwrap()
        .with_max_attempts(2);
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(response.message.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn request_id_header_is_reported_in_http_error() {
        let endpoint = header_server(
            "500 Internal Server Error",
            "{}",
            "x-request-id: req-123\r\n",
        );
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1)).unwrap();
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        match err {
            ProviderError::Http { request_id, .. } => assert!(request_id.contains("req-123")),
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn streaming_fallback_second_failure_is_http_error() {
        let (endpoint, _attempts) = sequence_server(vec![
            ("400 Bad Request", ""),
            ("500 Internal Server Error", "{}"),
        ]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true)
            .with_max_attempts(3);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Http { status: 500, .. }));
    }

    #[tokio::test]
    async fn streaming_fallback_invalid_json_is_malformed() {
        let (endpoint, _attempts) =
            sequence_server(vec![("400 Bad Request", ""), ("200 OK", "not json")]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true)
            .with_max_attempts(3);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)));
    }

    #[tokio::test]
    async fn invalid_tool_arguments_are_malformed() {
        let body = r#"{"choices":[{"message":{"tool_calls":[{"id":"call-1","function":{"name":"read_file","arguments":"not json"}}]}}]}"#;
        let provider =
            OpenAiProvider::new(server("200 OK", body, 0), "m", "k", Duration::from_secs(1))
                .unwrap();
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)));
    }

    // -- SSE error paths -----------------------------------------------------

    #[tokio::test]
    async fn sse_cancellation_returns_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer);
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            thread::sleep(Duration::from_secs(5));
        });
        let provider = OpenAiProvider::new(
            format!("http://{address}"),
            "m",
            "k",
            Duration::from_secs(5),
        )
        .unwrap()
        .with_streaming(true);
        let cancellation = latte_engine::CancellationToken::new();
        let cancellation_clone = cancellation.clone();
        let context = ProviderContext {
            deadline: Instant::now() + Duration::from_secs(5),
            cancellation,
            events: None,
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancellation_clone.cancel();
        });
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Cancelled));
    }

    #[tokio::test]
    async fn sse_line_without_colon_is_ignored() {
        let chunks = vec![
            b"no-colon-line\r\n\r\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\n".to_vec(),
            b"data: [DONE]\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(response.message.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn sse_oversized_event_is_malformed() {
        let large = "x".repeat(MAX_SSE_EVENT_BYTES + 1);
        let chunks = vec![format!("data: {large}\r\n\r\n").into_bytes()];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("SSE event exceeds limit")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_non_utf8_data_is_malformed() {
        let chunks = vec![b"data: \xFF\xFE\r\n\r\n".to_vec()];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("not UTF-8")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_stream_without_done_is_malformed() {
        let chunks =
            vec![b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\n".to_vec()];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("[DONE]")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_tool_call_missing_id_is_malformed() {
        let chunks = vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\r\n\r\n".to_vec(),
            b"data: [DONE]\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("missing id")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_tool_call_missing_name_is_malformed() {
        let chunks = vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"arguments\":\"{}\"}}]}}]}\r\n\r\n".to_vec(),
            b"data: [DONE]\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("missing name")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_invalid_tool_arguments_is_malformed() {
        let chunks = vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"not json\"}}]}}]}\r\n\r\n".to_vec(),
            b"data: [DONE]\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)));
    }

    #[tokio::test]
    async fn sse_invalid_json_event_is_malformed() {
        let chunks = vec![b"data: not-json\r\n\r\n".to_vec()];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("invalid SSE JSON")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_event_without_choices_is_ignored() {
        let chunks = vec![
            b"data: {\"usage\":{\"total_tokens\":1}}\r\n\r\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\n".to_vec(),
            b"data: [DONE]\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(response.message.as_deref(), Some("ok"));
        assert_eq!(response.usage.total_tokens, Some(1));
    }

    #[tokio::test]
    async fn sse_too_many_tool_calls_is_malformed() {
        let mut chunks = Vec::new();
        for index in 0..=MAX_SSE_TOOL_CALLS {
            chunks.push(
                format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":{index}}}]}}}}]}}\r\n\r\n"
                )
                .into_bytes(),
            );
        }
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("too many stream tool calls")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_tool_id_changed_is_malformed() {
        let chunks = vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\"}]}}]}\r\n\r\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"b\"}]}}]}\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("tool id changed")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_tool_name_changed_is_malformed() {
        let chunks = vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"a\"}}]}}]}\r\n\r\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"b\"}}]}}]}\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("tool name changed")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn sse_tool_arguments_oversized_is_malformed() {
        let large = "x".repeat(200_000);
        let chunks = vec![
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"function\":{{\"name\":\"r\",\"arguments\":\"{large}\"}}}}]}}}}]}}\r\n\r\n"
            )
            .into_bytes(),
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{large}\"}}}}]}}}}]}}\r\n\r\n"
            )
            .into_bytes(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
                    session_ref: "session-ref".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(ref m) if m.contains("arguments exceed limit")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn configured_headers_reach_the_wire_with_the_session_reference_expanded() {
        let (endpoint, heads) = header_capturing_server(vec![(
            "200 OK",
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
        )]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_headers(BTreeMap::from([
                ("x-session-id".into(), "${session_id}".into()),
                ("x-tenant".into(), "acme".into()),
            ]));
        provider
            .complete(
                ProviderRequest {
                    session_ref: "abc123".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        let head = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.contains(&"x-session-id: abc123".to_owned()),
            "{head:?}"
        );
        assert!(head.contains(&"x-tenant: acme".to_owned()), "{head:?}");
    }

    #[tokio::test]
    async fn configured_headers_cannot_replace_the_credential_or_the_wire_format() {
        let (endpoint, heads) = header_capturing_server(vec![(
            "200 OK",
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
        )]);
        // Construction is the last line of defence: config validation already
        // rejects these, but a Provider built in code must not be able to send
        // a credential the resolver did not produce.
        let provider = OpenAiProvider::new(endpoint, "m", "real-key", Duration::from_secs(1))
            .unwrap()
            .with_headers(BTreeMap::from([
                ("Authorization".into(), "Bearer stolen".into()),
                ("Content-Type".into(), "text/plain".into()),
            ]));
        provider
            .complete(
                ProviderRequest {
                    session_ref: "abc123".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        let head = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.contains(&"authorization: bearer real-key".to_owned()),
            "{head:?}"
        );
        assert!(
            head.contains(&"content-type: application/json".to_owned()),
            "{head:?}"
        );
        assert!(!head.iter().any(|line| line.contains("stolen")), "{head:?}");
    }

    #[tokio::test]
    async fn the_inline_fallback_carries_the_same_headers_as_the_stream_attempt() {
        // The fallback builds a second request from scratch. A Provider that
        // rejects the stream *because* of a missing header would otherwise be
        // retried without it and fail again for the same reason.
        let (endpoint, heads) = header_capturing_server(vec![
            ("400 Bad Request", ""),
            (
                "200 OK",
                r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
            ),
        ]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true)
            .with_headers(BTreeMap::from([(
                "x-session-id".into(),
                "${session_id}".into(),
            )]));
        provider
            .complete(
                ProviderRequest {
                    session_ref: "abc123".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        let streamed = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        let fallback = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            streamed.contains(&"x-session-id: abc123".to_owned()),
            "{streamed:?}"
        );
        assert!(
            fallback.contains(&"x-session-id: abc123".to_owned()),
            "{fallback:?}"
        );
    }

    #[tokio::test]
    async fn a_provider_without_configured_headers_sends_none() {
        let (endpoint, heads) = header_capturing_server(vec![(
            "200 OK",
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
        )]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1)).unwrap();
        provider
            .complete(
                ProviderRequest {
                    session_ref: "abc123".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        let head = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            !head.iter().any(|line| line.contains("abc123")),
            "an unconfigured provider must not leak the session reference: {head:?}"
        );
    }

    #[test]
    fn the_session_reference_is_stable_per_session_and_hides_the_session_id() {
        let session_id = latte_core::SessionId::from_uuid(uuid::Uuid::now_v7());
        let other = latte_core::SessionId::from_uuid(uuid::Uuid::now_v7());
        let reference = session_ref_for(session_id);
        assert_eq!(
            reference,
            session_ref_for(session_id),
            "every turn of one session must present the same reference"
        );
        assert_ne!(reference, session_ref_for(other));
        assert_eq!(reference.len(), 32);
        assert!(reference.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            !reference.contains(&session_id.to_string()),
            "the internal identifier must not survive into the header"
        );
        // A hyphen-free hex string also survives every Provider's header parser.
        assert!(reqwest::header::HeaderValue::try_from(reference).is_ok());
    }

    #[tokio::test]
    async fn a_header_value_that_cannot_be_sent_is_skipped_rather_than_failing_the_turn() {
        let (endpoint, heads) = header_capturing_server(vec![(
            "200 OK",
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
        )]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_headers(BTreeMap::from([
                // Validation rejects this at load time; a value assembled in
                // code could still reach here, and losing the conversation
                // would be worse than sending the request without the header.
                ("x-broken".into(), "line\nbreak".into()),
                ("x-fine".into(), "kept".into()),
            ]));
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "abc123".into(),
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(response.message.as_deref(), Some("ok"));
        let head = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.contains(&"x-fine: kept".to_owned()), "{head:?}");
        assert!(
            !head.iter().any(|line| line.starts_with("x-broken")),
            "{head:?}"
        );
    }

    fn count_cache_controls(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(count_cache_controls).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count_cache_controls).sum(),
            _ => 0,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn anthropic_payload_places_up_to_four_cache_breakpoints_coalesces_turns_and_filters_unsigned_thinking()
     {
        assert_eq!(resolve_anthropic_budgets(None, None), (16_384, None));
        assert_eq!(resolve_anthropic_budgets(None, Some("off")), (16_384, None));
        assert_eq!(
            resolve_anthropic_budgets(None, Some("minimal")),
            (16_384, Some(1_024))
        );
        assert_eq!(
            resolve_anthropic_budgets(None, Some("low")),
            (16_384, Some(4_096))
        );
        assert_eq!(
            resolve_anthropic_budgets(None, Some("medium")),
            (18_192, Some(10_000))
        );
        assert_eq!(
            resolve_anthropic_budgets(Some(65_536), Some("xhigh")),
            (65_536, Some(31_999))
        );
        assert_eq!(
            resolve_anthropic_budgets(Some(4_096), Some("high")),
            (4_096, Some(3_072))
        );
        assert_eq!(
            resolve_anthropic_budgets(Some(1_024), Some("low")),
            (1_024, None)
        );
        assert_eq!(
            resolve_anthropic_budgets(Some(8_192), Some("2048")),
            (8_192, Some(2_048))
        );

        let overlong_id = "call_".to_owned() + &"a".repeat(80);
        let sanitized = sanitize_anthropic_tool_id(&overlong_id);
        assert_eq!(sanitized.len(), 38);
        assert_eq!(sanitized, sanitize_anthropic_tool_id(&overlong_id));
        assert_eq!(sanitize_anthropic_tool_id("toolu_valid-1"), "toolu_valid-1");

        let provider = AnthropicProvider::new(
            "http://127.0.0.1:1/v1/messages",
            "seed-evolving",
            "secret-anth-key",
            Duration::from_secs(5),
        )
        .unwrap()
        .with_sampling_options(Some(0.3), Some(32_768))
        .with_reasoning_effort(Some("high".into()))
        .with_headers(BTreeMap::from([
            ("x-api-key".into(), "attacker".into()),
            ("authorization".into(), "Bearer attacker".into()),
            ("anthropic-version".into(), "1999-01-01".into()),
            ("x-trace".into(), "sid-${session_id}".into()),
        ]));
        assert!(!format!("{provider:?}").contains("secret-anth-key"));
        assert!(format!("{provider:?}").contains("[REDACTED]"));
        assert_eq!(
            provider.capabilities(),
            ProviderCapabilities {
                tools: true,
                parallel_tool_calls: true,
                input_request: false,
            }
        );

        let tools = vec![
            ToolDescriptor {
                name: "Read".into(),
                description: "Read file".into(),
                input_schema: tool_schema("read_file"),
                version: 1,
                effect: "read".into(),
            },
            ToolDescriptor {
                name: "Edit".into(),
                description: "Edit file".into(),
                input_schema: tool_schema("edit_file"),
                version: 1,
                effect: "mutate".into(),
            },
        ];
        let request = ProviderRequest {
            session_ref: "sess123".into(),
            tools,
            messages: vec![
                Message::System {
                    content: "System instructions".into(),
                },
                Message::User {
                    content: "<repository-context>\nAGENTS.md\n</repository-context>".into(),
                },
                Message::User {
                    content: "Fix src/lib.rs".into(),
                },
                Message::Assistant {
                    content: Some("Inspecting first.".into()),
                    reasoning_content: Some("unsigned foreign reasoning".into()),
                    thinking_blocks: vec![
                        ThinkingBlock::Thinking {
                            thinking: "unsigned block".into(),
                            signature: None,
                        },
                        ThinkingBlock::Thinking {
                            thinking: "signed thought".into(),
                            signature: Some("sig-abc".into()),
                        },
                        ThinkingBlock::RedactedThinking {
                            data: "enc-xyz".into(),
                        },
                    ],
                    tool_calls: vec![ToolCall {
                        id: overlong_id.clone(),
                        name: "Read".into(),
                        input: serde_json::json!({"path": "src/lib.rs"}),
                    }],
                },
                Message::Tool {
                    tool_call_id: overlong_id,
                    name: Some("Read".into()),
                    content: "fn main() {}".into(),
                },
                Message::User {
                    content: "<system-reminder>mode: ask</system-reminder>".into(),
                },
            ],
        };

        let (payload, thinking_on) = build_anthropic_payload(&provider, &request).unwrap();
        assert!(thinking_on);
        assert_eq!(count_cache_controls(&payload), 4);
        // Thinking enabled omits `temperature` and sets `thinking.budget_tokens`.
        assert!(payload.get("temperature").is_none());
        assert_eq!(payload["thinking"]["budget_tokens"], 16_384);
        assert_eq!(payload["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(payload["tools"][0].get("cache_control").is_none());
        assert_eq!(payload["tools"][1]["cache_control"]["type"], "ephemeral");
        // Consecutive User messages and Tool + User messages coalesce into strictly alternating roles.
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(
            msgs[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "repository-context block carries breakpoint #3"
        );
        assert_eq!(msgs[1]["role"], "assistant");
        let asst_blocks = msgs[1]["content"].as_array().unwrap();
        // Unsigned thinking was dropped; signed thinking + redacted_thinking + text + tool_use remain.
        assert_eq!(asst_blocks.len(), 4);
        assert_eq!(asst_blocks[0]["type"], "thinking");
        assert_eq!(asst_blocks[0]["signature"], "sig-abc");
        assert_eq!(asst_blocks[1]["type"], "redacted_thinking");
        assert_eq!(asst_blocks[2]["type"], "text");
        assert_eq!(asst_blocks[3]["type"], "tool_use");
        assert_eq!(asst_blocks[3]["id"], sanitized);
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], sanitized);
        assert_eq!(msgs[2]["content"][1]["cache_control"]["type"], "ephemeral");

        // Disabling prompt_caching emits zero cache_control markers; disabling thinking keeps temperature.
        let no_cache = provider
            .with_prompt_caching(false)
            .with_reasoning_effort(Some("none".into()));
        let (plain_payload, plain_thinking) = build_anthropic_payload(
            &no_cache,
            &ProviderRequest {
                session_ref: "s".into(),
                tools: vec![],
                messages: vec![Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    thinking_blocks: vec![],
                    tool_calls: vec![],
                }],
            },
        )
        .unwrap();
        assert!(!plain_thinking);
        assert_eq!(count_cache_controls(&plain_payload), 0);
        assert_eq!(plain_payload["temperature"], 0.3);
        assert_eq!(plain_payload["messages"][0]["role"], "user");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn anthropic_sse_streams_thinking_signature_text_and_tools_and_accounts_cache_tokens() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<ProviderEvent>>);
        impl ProviderEventSink for Sink {
            fn observe(&self, event: ProviderEvent) {
                self.0.lock().unwrap().push(event);
            }
        }

        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":400}}}\n\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me check \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"src/lib.rs.\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_stream_1\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_blob\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"Reading \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"file.\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"Read\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"src/lib.rs\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_02\",\"name\":\"GitDiff\",\"input\":{}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":4}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":55}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (hdr_tx, hdr_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 8192];
            let n = sock.read(&mut buf).unwrap();
            hdr_tx
                .send(String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase())
                .unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse_body}",
                sse_body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });

        let sink = Arc::new(Sink::default());
        let provider = AnthropicProvider::new(
            format!("http://{addr}/v1/messages"),
            "seed-evolving",
            "cpk-test-token",
            Duration::from_secs(2),
        )
        .unwrap()
        .with_reasoning_effort(Some("high".into()));
        let response = provider
            .complete(
                ProviderRequest {
                    session_ref: "sess_abc".into(),
                    messages: vec![Message::User {
                        content: "Inspect".into(),
                    }],
                    tools: vec![],
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_secs(2),
                    cancellation: latte_engine::CancellationToken::new(),
                    events: Some(sink.clone()),
                },
            )
            .await
            .unwrap();

        let raw_req = hdr_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(raw_req.contains("x-api-key: cpk-test-token"));
        assert!(
            !raw_req.contains("authorization:"),
            "default Anthropic auth must send only x-api-key without conflicting Authorization header: {raw_req}"
        );
        assert!(raw_req.contains("anthropic-version: 2023-06-01"));
        assert!(raw_req.contains("anthropic-beta: interleaved-thinking-2025-05-14"));

        assert_eq!(response.message.as_deref(), Some("Reading file."));
        assert_eq!(
            response.reasoning_content(),
            Some("Let me check src/lib.rs.")
        );
        assert_eq!(
            response.thinking_blocks(),
            vec![
                ThinkingBlock::Thinking {
                    thinking: "Let me check src/lib.rs.".into(),
                    signature: Some("sig_stream_1".into()),
                },
                ThinkingBlock::RedactedThinking {
                    data: "opaque_blob".into(),
                },
            ]
        );
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].name, "Read");
        assert_eq!(
            response.tool_calls[0].input,
            serde_json::json!({"file_path": "src/lib.rs"})
        );
        assert_eq!(response.tool_calls[1].name, "GitDiff");
        assert_eq!(response.tool_calls[1].input, serde_json::json!({}));
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(
            response.usage,
            ProviderUsage {
                input_tokens: Some(520),
                output_tokens: Some(55),
                total_tokens: Some(575),
                cached_tokens: Some(400),
            }
        );
        let observed = sink.0.lock().unwrap().clone();
        assert!(observed.contains(&ProviderEvent::AssistantDelta {
            text: "Reading ".into()
        }));
        assert!(observed.contains(&ProviderEvent::AssistantDelta {
            text: "file.".into()
        }));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn anthropic_retries_529_falls_back_on_empty_400_and_classifies_overflow_and_errors() {
        // 1) 529 Overloaded retries then succeeds on inline JSON; 400 "prompt is too long" -> ContextOverflow.
        let (endpoint, _heads) = header_capturing_server(vec![
            (
                "529 Overloaded",
                r#"{"error":{"type":"overloaded_error","message":"busy"}}"#,
            ),
            (
                "200 OK",
                r#"{"type":"message","content":[{"type":"thinking","thinking":"ok","signature":"s1"},{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#,
            ),
        ]);
        let ok_resp = AnthropicProvider::new(endpoint, "m", "k", Duration::from_secs(2))
            .unwrap()
            .with_max_attempts(2)
            .with_streaming(false)
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(ok_resp.message.as_deref(), Some("done"));
        assert_eq!(ok_resp.finish_reason, Some(FinishReason::Stop));
        assert_eq!(ok_resp.thinking_blocks().len(), 1);

        // 2) Empty-body 400 on stream=true falls back to non-streaming inline JSON.
        let (fallback_ep, fallback_heads) = header_capturing_server(vec![
            ("400 Bad Request", ""),
            (
                "200 OK",
                r#"{"type":"message","content":[{"type":"text","text":"inline-ok"}],"stop_reason":"max_tokens","usage":{"output_tokens":9}}"#,
            ),
        ]);
        let fb = AnthropicProvider::new(fallback_ep, "m", "k", Duration::from_secs(2))
            .unwrap()
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap();
        let _ = fallback_heads.recv_timeout(Duration::from_secs(1));
        assert_eq!(fb.message.as_deref(), Some("inline-ok"));
        assert_eq!(fb.finish_reason, Some(FinishReason::Length));

        // 3) Anthropic "prompt is too long" HTTP 400 -> ContextOverflow.
        let overflow_ep = server(
            "400 Bad Request",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
            0,
        );
        let overflow_err = AnthropicProvider::new(overflow_ep, "m", "k", Duration::from_secs(1))
            .unwrap()
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            overflow_err,
            ProviderError::ContextOverflow { .. }
        ));

        // 4) Truncated SSE stream (missing message_stop) and SSE overloaded_error fail closed.
        let mut truncated = AnthropicStreamAssembly::default();
        assert!(
            !consume_anthropic_sse_event(
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                &mut truncated,
                &context(),
            )
            .unwrap()
        );
        let sse_overloaded = consume_anthropic_sse_event(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
            &mut truncated,
            &context(),
        )
        .unwrap_err();
        assert!(matches!(
            sse_overloaded,
            ProviderError::Http {
                status: 529,
                retryable: true,
                ..
            }
        ));
        let sse_overflow = consume_anthropic_sse_event(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#,
            &mut truncated,
            &context(),
        )
        .unwrap_err();
        assert!(matches!(
            sse_overflow,
            ProviderError::ContextOverflow { .. }
        ));
        assert_eq!(
            anthropic_finish_reason("refusal".into()),
            FinishReason::ContentFilter
        );
        assert_eq!(
            anthropic_finish_reason("custom".into()),
            FinishReason::Other("custom".into())
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn anthropic_provider_registry_profile_and_aliased_translation_boundaries() {
        use crate::{profile::ProfileCatalog, registry::ProviderRegistry};
        use latte_core::{
            CompactionPolicy, CompactionStrategy, ContextPolicy, SessionProviderBinding,
            ToolPresentationStyle,
        };
        use latte_engine::EngineBuilder;
        use std::collections::BTreeMap;

        assert!(
            ProviderRegistry::parse_jsonc(
                r#"{version:1,default_model:"c/m",providers:{c:{type:"anthropic-messages",models:["m"],base_url:"https://api.anthropic.com/v1",api_key:"k",temperature:1.5}}}"#
            )
            .is_err()
        );
        let bad_hdr_reg = ProviderRegistry::parse_jsonc(
            r#"{version:1,default_model:"c/m",providers:{c:{type:"anthropic-messages",models:["m"],base_url:"https://api.anthropic.com/v1/",api_key:"k",headers:{"x-api-key":"override"}}}}"#,
        )
        .unwrap();
        assert!(bad_hdr_reg.resolve_model("c", "m", &[]).is_err());

        let tmp = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(tmp.path())
            .build()
            .unwrap();
        let mut descriptors = engine.tool_descriptors();
        if !descriptors.iter().any(|d| d.name == "process") {
            descriptors.push(latte_engine::ToolDescriptor {
                name: "process".into(),
                description: "run".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "argv": { "type": "array" } }
                }),
                version: 1,
                effect: "process".into(),
            });
            descriptors.sort_by(|a, b| a.name.cmp(&b.name));
        }

        let dual_reg = std::sync::Arc::new(
            ProviderRegistry::parse_jsonc(
                r#"{
                    version: 1,
                    default_model: "claude/m",
                    providers: {
                        claude: {
                            type: "anthropic-messages",
                            models: { m: { options: { context_window: 128000, reasoning_effort: "low", max_tokens: 4096 } } },
                            base_url: "https://api.anthropic.com/v1/",
                            api_key: "secret-anth",
                            prompt_caching: false
                        },
                        oai_claude: {
                            type: "openai-chat",
                            models: ["m2"],
                            endpoint: "http://127.0.0.1:9/v1/chat/completions",
                            api_key: "secret-oai",
                            tool_style: "claude_code"
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let base_ctx = ContextPolicy {
            compaction: CompactionPolicy {
                strategy: CompactionStrategy::ElideToolResultsThenSummarize,
                ..CompactionPolicy::default()
            },
            ..ContextPolicy::default()
        };
        let catalog = ProfileCatalog::new(base_ctx, std::sync::Arc::clone(&dual_reg)).unwrap();
        let anth_binding: SessionProviderBinding = dual_reg
            .session_binding_for_model("claude", "m", &descriptors)
            .unwrap();
        let anth_prof = catalog.resolve(&anth_binding).unwrap();
        assert_eq!(
            anth_prof.tool_presentation(),
            ToolPresentationStyle::ClaudeCode
        );
        assert!(anth_prof.compaction().reuse_prefix);
        assert!(anth_prof.system_prompt("").unwrap().contains("`Edit`"));

        let oai_binding = dual_reg
            .session_binding_for_model("oai_claude", "m2", &descriptors)
            .unwrap();
        let oai_prof = catalog.resolve(&oai_binding).unwrap();
        assert_eq!(
            oai_prof.tool_presentation(),
            ToolPresentationStyle::ClaudeCode
        );
        assert!(oai_prof.system_prompt("").unwrap().contains("`Bash`"));

        let serve_seq = |replies: Vec<(&'static str, &'static str, &'static str)>| -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            thread::spawn(move || {
                for (status, ctype, body) in replies {
                    let (mut sock, _) = listener.accept().unwrap();
                    let mut buf = [0_u8; 16 * 1024];
                    let _ = sock.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nrequest-id: req_test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes());
                }
            });
            format!("http://{addr}/v1/messages")
        };

        let ep_fallback = serve_seq(vec![
            ("400 Bad Request", "application/json", ""),
            (
                "200 OK",
                "application/json",
                r#"{"type":"message","content":[{"type":"redacted_thinking","data":"enc1"},{"type":"text","text":"fb-ok"}],"stop_reason":"max_tokens","usage":{"output_tokens":7}}"#,
            ),
        ]);
        let prov = AnthropicProvider::new(ep_fallback, "m", "secret-token", Duration::from_secs(2))
            .unwrap()
            .with_prompt_caching(false)
            .with_sampling_options(Some(0.2), None)
            .with_reasoning_effort(Some("off".into()))
            .with_headers(BTreeMap::from([
                ("x-api-key".into(), "ignored".into()),
                ("anthropic-beta".into(), "custom-beta".into()),
            ]));
        assert!(format!("{prov:?}").contains("[REDACTED]"));
        assert!(!prov.capabilities().input_request);
        let overlong = "call_".to_owned() + &"z".repeat(80);
        let fb_out = prov
            .complete(
                ProviderRequest {
                    session_ref: "s1".into(),
                    tools: vec![],
                    messages: vec![
                        Message::User {
                            content: "prior".into(),
                        },
                        Message::Assistant {
                            content: Some("prior done".into()),
                            reasoning_content: None,
                            thinking_blocks: vec![ThinkingBlock::Thinking {
                                thinking: "unsigned-historical".into(),
                                signature: None,
                            }],
                            tool_calls: vec![],
                        },
                        Message::User {
                            content: "current".into(),
                        },
                        Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            thinking_blocks: vec![ThinkingBlock::Thinking {
                                thinking: "signed-open".into(),
                                signature: Some("sig_open_ok".into()),
                            }],
                            tool_calls: vec![ToolCall {
                                id: overlong.clone(),
                                name: "Read".into(),
                                input: serde_json::json!("non-object"),
                            }],
                        },
                        Message::Tool {
                            tool_call_id: overlong,
                            name: Some("Read".into()),
                            content: "ok".into(),
                        },
                    ],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(fb_out.message.as_deref(), Some("fb-ok"));
        assert_eq!(fb_out.finish_reason, Some(FinishReason::Length));
        assert_eq!(fb_out.thinking_blocks().len(), 1);

        for (effort, max_tok, stop, expected_reason) in [
            ("minimal", None, "refusal", FinishReason::ContentFilter),
            ("low", Some(1024), "stop_sequence", FinishReason::Stop),
            (
                "medium",
                None,
                "custom_stop",
                FinishReason::Other("custom_stop".into()),
            ),
            ("xhigh", Some(65_536), "end_turn", FinishReason::Stop),
            ("2048", Some(8_192), "end_turn", FinishReason::Stop),
        ] {
            let body = format!(
                r#"{{"type":"message","content":[{{"type":"text","text":"ok"}}],"stop_reason":{stop:?},"usage":{{"input_tokens":5}}}}"#
            );
            let ep = server("200 OK", &body, 0);
            let res = AnthropicProvider::new(ep, "m", "k", Duration::from_secs(2))
                .unwrap()
                .with_streaming(false)
                .with_sampling_options(Some(0.5), max_tok)
                .with_reasoning_effort(Some(effort.into()))
                .complete(
                    ProviderRequest {
                        session_ref: "s".into(),
                        tools: vec![],
                        messages: vec![Message::User {
                            content: String::new(),
                        }],
                    },
                    context(),
                )
                .await
                .unwrap();
            assert_eq!(res.finish_reason, Some(expected_reason));
        }

        let ep_sse_malformed = serve_seq(vec![
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"text\",\"text\":\"x\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"index\":0}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"Read\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"seed\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0}\n\n",
            ),
        ]);
        let sse_prov =
            AnthropicProvider::new(ep_sse_malformed, "m", "k", Duration::from_secs(2)).unwrap();
        for _ in 0..6 {
            assert!(matches!(
                sse_prov
                    .complete(
                        ProviderRequest {
                            session_ref: "s".into(),
                            tools: vec![],
                            messages: vec![
                                Message::User {
                                    content: "u".into()
                                },
                                Message::Assistant {
                                    content: None,
                                    reasoning_content: None,
                                    thinking_blocks: vec![],
                                    tool_calls: vec![],
                                },
                            ],
                        },
                        context(),
                    )
                    .await,
                Err(ProviderError::Malformed(_))
            ));
        }

        let ep_claude = serve_seq(vec![(
            "200 OK",
            "application/json",
            r#"{"type":"message","content":[
                {"type":"tool_use","id":"c1","name":"GitDiff","input":{}},
                {"type":"tool_use","id":"c2","name":"ReadManifest","input":{}},
                {"type":"tool_use","id":"c3","name":"Grep","input":{"pattern":"a+b","literal":true}},
                {"type":"tool_use","id":"c4","name":"Write","input":{"file_path":"w.txt","content":"hi","create_intent":true}},
                {"type":"tool_use","id":"c5","name":"Bash","input":{"argv":["ls"],"cwd":".","timeout_ms":500}},
                {"type":"tool_use","id":"c6","name":"Bash","input":{"command":"FOO=1 env"}}
            ],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":4}}"#,
        )]);
        let reg = ProviderRegistry::parse_jsonc(&format!(
            r#"{{version:1,default_model:"c/m",providers:{{c:{{type:"anthropic-messages",models:["m"],endpoint:{ep_claude:?},api_key:"k",streaming:false}}}}}}"#
        ))
        .unwrap();
        assert!(format!("{reg:?}").contains("[REDACTED]"));
        let resolved = reg.resolve_default(&descriptors).unwrap();
        let out = resolved
            .provider
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    tools: descriptors.clone(),
                    messages: vec![
                        Message::User {
                            content: "check".into(),
                        },
                        Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            thinking_blocks: vec![],
                            tool_calls: vec![
                                ToolCall {
                                    id: "h1".into(),
                                    name: "search".into(),
                                    input: serde_json::json!({"query": "lit", "regex": false, "glob": "*.rs"}),
                                },
                                ToolCall {
                                    id: "h2".into(),
                                    name: "process".into(),
                                    input: serde_json::json!({"shell": "echo a | cat", "cwd": ".", "timeout_ms": 1000}),
                                },
                                ToolCall {
                                    id: "h3".into(),
                                    name: "read_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "offset": 1, "limit": 10}),
                                },
                                ToolCall {
                                    id: "h4".into(),
                                    name: "edit_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "before": "x", "after": "y", "replace_all": true}),
                                },
                                ToolCall {
                                    id: "h5".into(),
                                    name: "write_file".into(),
                                    input: serde_json::json!({"path": "a.rs", "content": "z"}),
                                },
                                ToolCall {
                                    id: "h6".into(),
                                    name: "list_directory".into(),
                                    input: serde_json::json!({"path": "src"}),
                                },
                                ToolCall {
                                    id: "h7".into(),
                                    name: "git_diff".into(),
                                    input: serde_json::json!("raw"),
                                },
                            ],
                        },
                        Message::Tool {
                            tool_call_id: "h1".into(),
                            name: Some("search".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h2".into(),
                            name: Some("process".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h3".into(),
                            name: Some("read_file".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h4".into(),
                            name: Some("edit_file".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h5".into(),
                            name: Some("write_file".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h6".into(),
                            name: Some("list_directory".into()),
                            content: "ok".into(),
                        },
                        Message::Tool {
                            tool_call_id: "h7".into(),
                            name: Some("git_diff".into()),
                            content: "ok".into(),
                        },
                    ],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(out.tool_calls[0].name, "git_diff");
        assert_eq!(out.tool_calls[1].name, "read_project_manifest");
        assert_eq!(out.tool_calls[2].name, "search");
        assert_eq!(out.tool_calls[2].input["regex"], false);
        assert_eq!(out.tool_calls[3].name, "write_file");
        assert_eq!(out.tool_calls[3].input["create_intent"], true);
        assert_eq!(out.tool_calls[4].name, "process");
        assert_eq!(out.tool_calls[4].input["argv"], serde_json::json!(["ls"]));
        assert_eq!(out.tool_calls[5].name, "process");
        assert_eq!(out.tool_calls[5].input["shell"], "FOO=1 env");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn cross_origin_redirects_are_blocked_without_credential_egress_while_same_origin_succeeds()
     {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Target server on a different loopback port that must NEVER receive credentials.
        let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let target_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits_clone = std::sync::Arc::clone(&target_hits);
        let _target_guard = thread::spawn(move || {
            let _ = target_listener.set_nonblocking(true);
            for _ in 0..30 {
                if let Ok((mut sock, _)) = target_listener.accept() {
                    hits_clone.fetch_add(1, Ordering::SeqCst);
                    let mut buf = [0_u8; 4096];
                    let _ = sock.read(&mut buf);
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
                }
                thread::sleep(Duration::from_millis(10));
            }
        });

        let redirect_origin = |location: String| -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    let mut buf = [0_u8; 4096];
                    let _ = sock.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(resp.as_bytes());
                }
            });
            format!("http://{addr}/v1/messages")
        };

        let anth_ep = redirect_origin(format!("http://{target_addr}/steal"));
        let anth = AnthropicProvider::new(anth_ep, "m", "anth-secret-xyz", Duration::from_secs(2))
            .unwrap()
            .with_max_attempts(1);
        let anth_err = anth
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    tools: vec![],
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(anth_err, ProviderError::Transport(_)),
            "{anth_err:?}"
        );

        let oai_ep = redirect_origin(format!("http://{target_addr}/steal-oai"));
        let oai = OpenAiProvider::new(oai_ep, "m", "oai-secret-xyz", Duration::from_secs(2))
            .unwrap()
            .with_max_attempts(1);
        let oai_err = oai
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    tools: vec![],
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(oai_err, ProviderError::Transport(_)),
            "{oai_err:?}"
        );
        assert_eq!(
            target_hits.load(Ordering::SeqCst),
            0,
            "cross-origin redirect target must never be contacted"
        );

        // Same-origin redirect (identical scheme + host + port) is permitted.
        let same_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let same_addr = same_listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s1, _) = same_listener.accept().unwrap();
            let mut b1 = [0_u8; 4096];
            let _ = s1.read(&mut b1);
            let r1 = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{same_addr}/v1/messages-v2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = s1.write_all(r1.as_bytes());

            let (mut s2, _) = same_listener.accept().unwrap();
            let mut b2 = [0_u8; 4096];
            let n = s2.read(&mut b2).unwrap();
            let req_text = String::from_utf8_lossy(&b2[..n]).to_ascii_lowercase();
            assert!(req_text.contains("x-api-key: same-origin-key"));
            let body = r#"{"type":"message","content":[{"type":"text","text":"same-ok"}],"stop_reason":"end_turn","usage":{"output_tokens":2}}"#;
            let r2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s2.write_all(r2.as_bytes());
        });
        let same_prov = AnthropicProvider::new(
            format!("http://{same_addr}/v1/messages"),
            "m",
            "same-origin-key",
            Duration::from_secs(2),
        )
        .unwrap()
        .with_streaming(false);
        let same_out = same_prov
            .complete(
                ProviderRequest {
                    session_ref: "s".into(),
                    tools: vec![],
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(same_out.message.as_deref(), Some("same-ok"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn anthropic_payload_rejects_unsigned_thinking_only_on_open_tool_continuations() {
        let prov = AnthropicProvider::new(
            "http://127.0.0.1:9/v1/messages",
            "m",
            "k",
            Duration::from_secs(1),
        )
        .unwrap()
        .with_reasoning_effort(Some("low".into()));

        // Open tool continuation whose signed thinking was stripped -> Malformed error (#13).
        let open_err = build_anthropic_payload(
            &prov,
            &ProviderRequest {
                session_ref: "s".into(),
                tools: vec![],
                messages: vec![
                    Message::User {
                        content: "do work".into(),
                    },
                    Message::Assistant {
                        content: None,
                        reasoning_content: None,
                        thinking_blocks: vec![ThinkingBlock::Thinking {
                            thinking: "[REDACTED]".into(),
                            signature: None,
                        }],
                        tool_calls: vec![ToolCall {
                            id: "call_1".into(),
                            name: "Read".into(),
                            input: serde_json::json!({"file_path":"a.txt"}),
                        }],
                    },
                    Message::Tool {
                        tool_call_id: "call_1".into(),
                        name: Some("Read".into()),
                        content: "hello".into(),
                    },
                ],
            },
        )
        .unwrap_err();
        assert!(
            matches!(open_err, ProviderError::Malformed(_)),
            "{open_err:?}"
        );

        // Mixed signed + unsigned thinking in an open tool continuation MUST fail closed
        // even when one valid signed block is present and even if reasoning_effort is off!
        let plain_prov = AnthropicProvider::new(
            "http://127.0.0.1:9/v1/messages",
            "m",
            "k",
            Duration::from_secs(1),
        )
        .unwrap();
        for blocks in [
            vec![
                ThinkingBlock::Thinking {
                    thinking: "valid".into(),
                    signature: Some("sig_ok".into()),
                },
                ThinkingBlock::Thinking {
                    thinking: "[REDACTED]".into(),
                    signature: None,
                },
            ],
            vec![
                ThinkingBlock::Thinking {
                    thinking: "valid".into(),
                    signature: Some("sig_ok".into()),
                },
                ThinkingBlock::RedactedThinking {
                    data: String::new(),
                },
            ],
        ] {
            let mixed_err = build_anthropic_payload(
                &plain_prov,
                &ProviderRequest {
                    session_ref: "s".into(),
                    tools: vec![],
                    messages: vec![
                        Message::User {
                            content: "do work".into(),
                        },
                        Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            thinking_blocks: blocks,
                            tool_calls: vec![ToolCall {
                                id: "call_1".into(),
                                name: "Read".into(),
                                input: serde_json::json!({"file_path":"a.txt"}),
                            }],
                        },
                        Message::Tool {
                            tool_call_id: "call_1".into(),
                            name: Some("Read".into()),
                            content: "hello".into(),
                        },
                    ],
                },
            )
            .unwrap_err();
            assert!(
                matches!(mixed_err, ProviderError::Malformed(_)),
                "mixed signed+unsigned thinking must fail closed: {mixed_err:?}"
            );
        }

        // `model_context_window_exceeded` maps to `FinishReason::Length`.
        assert_eq!(
            anthropic_finish_reason("model_context_window_exceeded".into()),
            FinishReason::Length
        );

        // `parse_retry_after` honors Retry-After > 2s and retry-after-ms without 2s clamping.
        let mut hdrs = reqwest::header::HeaderMap::new();
        hdrs.insert("retry-after", "5".parse().unwrap());
        assert_eq!(parse_retry_after(&hdrs), Some(Duration::from_secs(5)));
        hdrs.insert("retry-after-ms", "2500".parse().unwrap());
        assert_eq!(parse_retry_after(&hdrs), Some(Duration::from_millis(2500)));

        // Historical completed turn with unsigned thinking followed by a final Assistant turn succeeds.
        assert!(
            build_anthropic_payload(
                &prov,
                &ProviderRequest {
                    session_ref: "s".into(),
                    tools: vec![],
                    messages: vec![
                        Message::User {
                            content: "turn 1".into(),
                        },
                        Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            thinking_blocks: vec![ThinkingBlock::Thinking {
                                thinking: "[REDACTED]".into(),
                                signature: None,
                            }],
                            tool_calls: vec![ToolCall {
                                id: "call_1".into(),
                                name: "Read".into(),
                                input: serde_json::json!({"file_path":"a.txt"}),
                            }],
                        },
                        Message::Tool {
                            tool_call_id: "call_1".into(),
                            name: Some("Read".into()),
                            content: "hello".into(),
                        },
                        Message::Assistant {
                            content: Some("done".into()),
                            reasoning_content: None,
                            thinking_blocks: vec![],
                            tool_calls: vec![],
                        },
                        Message::User {
                            content: "turn 2".into(),
                        },
                    ],
                },
            )
            .is_ok()
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn anthropic_bearer_auth_mode_retry_after_over_2s_and_pre_delta_sse_overloaded_retry() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (hdr_tx, hdr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Attempt 1: 429 with Retry-After: 3 where deadline is 1s -> must fail immediately with Timeout
            // (proving Retry-After > 2s is NOT clamped down to 2s or ignored!).
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let _ = hdr_tx.send(String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase());
                let _ = sock.write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
            // Attempt 2: SSE stream returning `overloaded_error` BEFORE any text delta -> retried!
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let sse_err = concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
                    "event: error\n",
                    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n"
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_err.len(),
                    sse_err
                );
                let _ = sock.write_all(resp.as_bytes());
            }
            // Attempt 3: SSE stream succeeds!
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let sse_ok = concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
                    "event: content_block_start\n",
                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"recovered!\"}}\n\n",
                    "event: message_delta\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_ok.len(),
                    sse_ok
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });

        let prov = AnthropicProvider::new(
            format!("http://{addr}/v1/messages"),
            "claude-sonnet-4-5",
            "bearer-only-secret",
            Duration::from_secs(2),
        )
        .unwrap()
        .with_bearer_auth(true)
        .with_max_attempts(3);

        // 1. Short deadline (1.5s) vs `Retry-After: 3` -> fails with Timeout without clamping 3s to 2s!
        let short_err = prov
            .complete(
                ProviderRequest {
                    session_ref: "s1".into(),
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                    tools: vec![],
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_millis(1500),
                    cancellation: latte_engine::CancellationToken::new(),
                    events: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(short_err, ProviderError::Timeout), "{short_err:?}");
        let raw_hdr = hdr_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(raw_hdr.contains("authorization: bearer bearer-only-secret"));
        assert!(
            !raw_hdr.contains("x-api-key:"),
            "bearer_auth mode must send only Authorization header: {raw_hdr}"
        );

        // 2. Pre-delta SSE `overloaded_error` retries within `max_attempts` and succeeds on the next attempt!
        let ok_resp = prov
            .complete(
                ProviderRequest {
                    session_ref: "s2".into(),
                    messages: vec![Message::User {
                        content: "hi".into(),
                    }],
                    tools: vec![],
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_secs(5),
                    cancellation: latte_engine::CancellationToken::new(),
                    events: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(ok_resp.message.as_deref(), Some("recovered!"));
    }
}
