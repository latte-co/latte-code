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
pub type ProviderOutcome = ProviderResponse;

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
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
    #[error("provider timeout")]
    Timeout,
    #[error("provider transport: {0}")]
    Transport(String),
    #[error("malformed provider response: {0}")]
    Malformed(String),
}

pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderOutcome, ProviderError>> + Send + 'a>>;
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
}
impl OpenAiProvider {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
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
            streaming: false,
        })
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
            tool_calls,
        } => WireRequestMessage::Assistant {
            content: content.as_deref(),
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
            let mut attempt = 0;
            let response = loop {
                attempt += 1;
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
                    this.client
                        .post(&this.endpoint)
                        .bearer_auth(&this.api_key)
                        .json(&body)
                        .send(),
                )
                .await;
                let response = match sent {
                    Err(_) => return Err(ProviderError::Timeout),
                    Ok(Err(error)) if error.is_connect() && attempt < this.max_attempts => {
                        retry_pause(&context, attempt, None).await?;
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
                if !response.status().is_success()
                    && matches!(status, 408 | 429 | 502 | 503 | 504)
                    && attempt < this.max_attempts
                {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    retry_pause(&context, attempt, retry_after).await?;
                    continue;
                }
                break response;
            };
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let request_id = response
                    .headers()
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| format!(" (request {v})"))
                    .unwrap_or_default();
                if this.streaming && matches!(status, 400 | 404 | 415 | 422) {
                    // A fallback is allowed only when the stream request was
                    // rejected before it produced *any* response body bytes.
                    let error_body = response
                        .bytes()
                        .await
                        .map_err(|error| ProviderError::Transport(error.to_string()))?;
                    if error_body.is_empty() {
                        let mut inline_body = serde_json::to_value(&body)
                            .map_err(|error| ProviderError::Malformed(error.to_string()))?;
                        inline_body
                            .as_object_mut()
                            .expect("request serializes to object")
                            .remove("stream");
                        return complete_inline_once(&this, inline_body, &context).await;
                    }
                }
                return Err(ProviderError::Http {
                    status,
                    request_id,
                    retryable: matches!(status, 408 | 429 | 502 | 503 | 504),
                });
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if this.streaming && content_type.contains("text/event-stream") {
                return parse_openai_sse(response, &context).await;
            }
            let wire: Wire = response
                .json()
                .await
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

fn emit_provider_event(context: &ProviderContext, event: ProviderEvent) {
    // Rendering observers are intentionally outside the provider critical
    // path. A slow terminal reducer cannot hold the network response open.
    if let Some(sink) = &context.events {
        let sink = Arc::clone(sink);
        tokio::task::spawn_blocking(move || sink.observe(event));
    }
}

async fn complete_inline_once(
    provider: &OpenAiProvider,
    body: Value,
    context: &ProviderContext,
) -> Result<ProviderResponse, ProviderError> {
    if context.cancellation.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    let remaining = context
        .deadline
        .saturating_duration_since(Instant::now())
        .min(provider.timeout);
    if remaining.is_zero() {
        return Err(ProviderError::Timeout);
    }
    let response = tokio::time::timeout(
        remaining,
        provider
            .client
            .post(&provider.endpoint)
            .bearer_auth(&provider.api_key)
            .json(&body)
            .send(),
    )
    .await
    .map_err(|_| ProviderError::Timeout)?
    .map_err(|error| ProviderError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        return Err(ProviderError::Http {
            status,
            request_id: response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .map(|value| format!(" (request {value})"))
                .unwrap_or_default(),
            retryable: false,
        });
    }
    let wire = response
        .json()
        .await
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
    Ok(ProviderResponse {
        message: message.content,
        tool_calls,
        input_request: message.input_request,
        usage,
        finish_reason: choice.finish_reason.map(finish_reason),
        provider_state: None,
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

async fn parse_openai_sse(
    mut response: reqwest::Response,
    context: &ProviderContext,
) -> Result<ProviderResponse, ProviderError> {
    let mut buffer = Vec::<u8>::new();
    let mut data_lines = Vec::<String>::new();
    let mut total = 0_usize;
    let mut done = false;
    let mut assembly = StreamAssembly::default();
    loop {
        let next = tokio::select! {
            chunk = response.chunk() => chunk.map_err(|error| ProviderError::Transport(error.to_string()))?,
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
                    done =
                        consume_sse_event(&data_lines.join("\n"), &mut assembly, context)? || done;
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
        done = consume_sse_event(&data_lines.join("\n"), &mut assembly, context)?;
    }
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
    Ok(ProviderResponse {
        message: (!assembly.content.is_empty()).then_some(assembly.content),
        tool_calls: calls,
        input_request: None,
        usage: usage_from_wire(assembly.usage),
        finish_reason: assembly.finish_reason.map(finish_reason),
        provider_state: None,
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
async fn retry_pause(
    context: &ProviderContext,
    attempt: u32,
    retry_after: Option<Duration>,
) -> Result<(), ProviderError> {
    let exponential =
        Duration::from_millis(100_u64.saturating_mul(1_u64 << attempt.saturating_sub(1).min(4)));
    let delay = retry_after
        .unwrap_or(exponential)
        .min(Duration::from_secs(2));
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
#[cfg(test)]
fn tool_schema(name: &str) -> Value {
    match name {
        "read_file" => {
            serde_json::json!({"type":"object","required":["path"],"properties":{"path":path_schema(),"max_output":output_cap_schema()},"additionalProperties":false})
        }
        "list_directory" => {
            serde_json::json!({"type":"object","required":["path"],"properties":{"path":path_schema(),"max_entries":{"type":"integer","minimum":1,"maximum":10000}},"additionalProperties":false})
        }
        "search" => {
            serde_json::json!({"type":"object","required":["query"],"properties":{"query":{"type":"string","minLength":1,"maxLength":4096},"regex":{"type":"boolean"},"max_results":{"type":"integer","minimum":1,"maximum":10000},"max_output":output_cap_schema()},"additionalProperties":false})
        }
        "read_project_manifest" | "git_diff" => {
            serde_json::json!({"type":"object","required":[],"properties":{"max_output":output_cap_schema()},"additionalProperties":false})
        }
        "edit_file" => {
            serde_json::json!({"type":"object","required":["path","after","precondition"],"properties":{"path":path_schema(),"before":{"type":"string","minLength":1},"anchor":{"type":"string","minLength":1},"after":{"type":"string"},"precondition":digest_schema()},"anyOf":[{"required":["before"]},{"required":["anchor"]}],"additionalProperties":false})
        }
        "write_file" => {
            serde_json::json!({"type":"object","required":["path","content","create_intent"],"properties":{"path":path_schema(),"content":{"type":"string"},"create_intent":{"type":"boolean"},"precondition":digest_schema()},"additionalProperties":false})
        }
        "process" => {
            serde_json::json!({"type":"object","required":[],"properties":{"argv":{"type":"array","minItems":1,"maxItems":256,"items":{"type":"string","minLength":1,"maxLength":4096}},"shell":{"type":"string","minLength":1,"maxLength":16384},"cwd":path_schema(),"env":{"type":"object","maxProperties":128,"additionalProperties":{"type":"string","maxLength":16384}},"timeout_ms":{"type":"integer","minimum":1,"maximum":600_000},"grace_ms":{"type":"integer","minimum":0,"maximum":30_000},"stdout_cap":{"type":"integer","minimum":1,"maximum":1_048_576},"stderr_cap":{"type":"integer","minimum":1,"maximum":1_048_576}},"oneOf":[{"required":["argv"]},{"required":["shell"]}],"additionalProperties":false})
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
        assert_eq!(tool_schema("process")["oneOf"].as_array().unwrap().len(), 2);
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
            complete_inline_once(&provider, serde_json::json!({}), &cancelled_context).await,
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
            complete_inline_once(&provider, serde_json::json!({}), &expired_context).await,
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
                tool_calls: vec![],
            }
            .content(),
            Some("a")
        );
        assert_eq!(
            Message::Assistant {
                content: None,
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
            fn complete(
                &self,
                _: ProviderRequest,
                _: ProviderContext,
            ) -> ProviderFuture<'_> {
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
        let provider =
            OpenAiProvider::new(endpoint, "m", "k", Duration::from_millis(50)).unwrap();
        let err = provider
            .complete(
                ProviderRequest {
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
        let provider = OpenAiProvider::new("http://127.0.0.1:1", "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        let err = provider
            .complete(
                ProviderRequest {
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)));
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
        let provider = OpenAiProvider::new(format!("http://{address}"), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_max_attempts(2);
        let response = provider
            .complete(
                ProviderRequest {
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
        let (endpoint, _attempts) =
            sequence_server(vec![("400 Bad Request", ""), ("500 Internal Server Error", "{}")]);
        let provider = OpenAiProvider::new(endpoint, "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true)
            .with_max_attempts(3);
        let err = provider
            .complete(
                ProviderRequest {
                    messages: vec![],
                    tools: vec![],
                },
                context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Http {
                status: 500,
                ..
            }
        ));
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
            OpenAiProvider::new(server("200 OK", body, 0), "m", "k", Duration::from_secs(1)).unwrap();
        let err = provider
            .complete(
                ProviderRequest {
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
        let provider = OpenAiProvider::new(format!("http://{address}"), "m", "k", Duration::from_secs(5))
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
        let chunks = vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\n".to_vec(),
        ];
        let provider = OpenAiProvider::new(sse_server(chunks), "m", "k", Duration::from_secs(1))
            .unwrap()
            .with_streaming(true);
        let err = provider
            .complete(
                ProviderRequest {
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
                    "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":{index}}}]}}]}}\r\n\r\n"
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
}
