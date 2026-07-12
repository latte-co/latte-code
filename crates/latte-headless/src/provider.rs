use latte_engine::ToolDescriptor;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::VecDeque, sync::Mutex, time::Duration};
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
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProviderResponse {
    pub message: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub input_request: Option<InputRequest>,
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
    #[error("provider timeout")]
    Timeout,
    #[error("provider transport: {0}")]
    Transport(String),
    #[error("malformed provider response: {0}")]
    Malformed(String),
}

pub trait Provider: Send + Sync {
    fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDescriptor],
    ) -> impl std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send;
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
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDescriptor],
    ) -> Result<ProviderResponse, ProviderError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Malformed("fake provider exhausted".into()))?
            .map_err(ProviderError::Transport)
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    timeout: Duration,
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
        })
    }
}
#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<WireRequestMessage<'a>>,
    tools: Vec<WireTool>,
    tool_choice: &'static str,
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
}
#[derive(Deserialize)]
struct Choice {
    message: WireMessage,
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
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDescriptor],
    ) -> Result<ProviderResponse, ProviderError> {
        let send = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&Request {
                model: &self.model,
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
                            description: format!("engine-owned {} operation", tool.effect),
                            parameters: tool_schema(&tool.name),
                        },
                    })
                    .collect(),
                tool_choice: "auto",
            })
            .send();
        let response = tokio::time::timeout(self.timeout, send)
            .await
            .map_err(|_| ProviderError::Timeout)?
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Transport(e.to_string())
                }
            })?;
        if !response.status().is_success() {
            return Err(ProviderError::Transport(format!(
                "http {}",
                response.status()
            )));
        }
        let wire: Wire = response
            .json()
            .await
            .map_err(|e| ProviderError::Malformed(e.to_string()))?;
        let message = wire
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Malformed("missing choices".into()))?
            .message;
        let input_request = message.input_request;
        let tool_calls: Vec<ToolCall> = message
            .tool_calls
            .into_iter()
            .map(|call| {
                Ok(ToolCall {
                    id: call.id,
                    name: call.function.name,
                    input: serde_json::from_str(&call.function.arguments)
                        .map_err(|e| ProviderError::Malformed(e.to_string()))?,
                })
            })
            .collect::<Result<_, ProviderError>>()?;
        let mut ids = std::collections::BTreeSet::new();
        if tool_calls
            .iter()
            .any(|call| !valid_tool_call_id(&call.id) || !ids.insert(call.id.clone()))
        {
            return Err(ProviderError::Malformed(
                "tool call ids must be nonempty and unique".into(),
            ));
        }
        Ok(ProviderResponse {
            message: message.content,
            tool_calls,
            input_request,
        })
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

fn path_schema() -> Value {
    serde_json::json!({"type":"string","minLength":1,"maxLength":4096})
}

fn output_cap_schema() -> Value {
    serde_json::json!({"type":"integer","minimum":1,"maximum":65536})
}

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
    #[tokio::test]
    async fn parses_structured_response_and_redacts_auth() {
        let (endpoint, captured) = capturing_server(
            r#"{"choices":[{"message":{"content":"ok","tool_calls":[{"id":"1","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]}}]}"#,
        );
        let provider =
            OpenAiProvider::new(endpoint, "m", "super-secret", Duration::from_secs(1)).unwrap();
        let tools = [ToolDescriptor {
            name: "read_file".into(),
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
        let response = provider.complete(&messages, &tools).await.unwrap();
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
                    "description": "engine-owned read operation",
                    "parameters": tool_schema("read_file")
                }
            })
        );
        let debug = format!("{provider:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("super-secret"));
    }
    #[tokio::test]
    async fn classifies_malformed_http_error_and_timeout() {
        let duplicate = OpenAiProvider::new(server("200 OK", r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"x","function":{"name":"read_file","arguments":"{}"}},{"id":"x","function":{"name":"read_file","arguments":"{}"}}]}}]}"#, 0), "m", "k", Duration::from_secs(1)).unwrap();
        assert!(matches!(
            duplicate.complete(&[], &[]).await,
            Err(ProviderError::Malformed(_))
        ));
        let malformed =
            OpenAiProvider::new(server("200 OK", "{}", 0), "m", "k", Duration::from_secs(1))
                .unwrap();
        assert!(matches!(
            malformed.complete(&[], &[]).await,
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
            error.complete(&[], &[]).await,
            Err(ProviderError::Transport(_))
        ));
        let slow = OpenAiProvider::new(
            server("200 OK", "{}", 200),
            "m",
            "k",
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(matches!(
            slow.complete(&[], &[]).await,
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
    fn tool_call_ids_are_bounded_opaque_and_control_free() {
        assert!(valid_tool_call_id("call_:+./?[]{}-✓"));
        assert!(valid_tool_call_id(&"x".repeat(256)));
        assert!(!valid_tool_call_id(""));
        assert!(!valid_tool_call_id(&"x".repeat(257)));
        assert!(!valid_tool_call_id("bad\ncall"));
        assert!(!valid_tool_call_id("bad\u{85}call"));
    }
}
