//! v1 HTTP+SSE API 契约测试（对应 docs/design/versioned-rpc-contract.md）。
//!
//! 这些测试把设计文档中的正式契约固化为可执行断言：
//! - 每个端点的响应 JSON 字段集合（schema 快照），防止意外 breaking change；
//! - 错误类型枚举完整性（六种 type 与状态码的一一对应）；
//! - SSE 事件类型完整性；
//! - 分页契约（cursor 不透明、limit=0 空页、next_cursor 终止语义）。
//!
//! 与 `http.rs` 内联测试的区别：这里只走公开 API（`latte_server::http::router`
//! + 公开 DTO），模拟真实客户端视角；内联测试覆盖内部分支与边界。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use latte_core::ThreadProviderBindingV2;
use latte_headless::provider::{FakeProvider, ProviderResponse, ProviderUsage};
use latte_headless::registry::{ProviderBinding, ResolvedProvider};
use latte_server::http::router;
use latte_server::{ServerState, WorkspaceRuntimeBuilder, new_state};
use serde_json::Value;
use tower::util::ServiceExt;

const TOKEN: &str = "contract-token";

/// 构造一个合法的、可通过 validate() 的 binding。
fn valid_binding() -> ThreadProviderBindingV2 {
    ThreadProviderBindingV2 {
        version: 1,
        provider_name: "contract".into(),
        provider_type: "openai-chat".into(),
        protocol: "chat".into(),
        model: "test".into(),
        config_fingerprint: "config".into(),
        tools_fingerprint: "tools".into(),
        aliases: std::collections::BTreeMap::new(),
        credential_ref_id: "env:CONTRACT_KEY".into(),
        data_scope_id: "workspace".into(),
        credential_generation: 1,
    }
}

/// Server state whose provider factory completes every turn in one step, so a
/// created session reaches a durable idle state.
fn completing_state() -> Arc<ServerState> {
    let factory: latte_headless::thread::ThreadProviderFactory =
        Arc::new(|binding: &ThreadProviderBindingV2| {
            let provider = FakeProvider::scripted([ProviderResponse {
                message: Some("done".into()),
                tool_calls: Vec::new(),
                input_request: None,
                usage: ProviderUsage::default(),
                finish_reason: Some(latte_headless::provider::FinishReason::Stop),
                provider_state: None,
            }]);
            Ok(ResolvedProvider {
                provider: Arc::new(provider),
                binding: ProviderBinding {
                    version: binding.version,
                    provider_name: binding.provider_name.clone(),
                    provider_type: binding.provider_type.clone(),
                    protocol: binding.protocol.clone(),
                    model: binding.model.clone(),
                    config_fingerprint: binding.config_fingerprint.clone(),
                    tools_fingerprint: binding.tools_fingerprint.clone(),
                    aliases: binding.aliases.clone(),
                },
            })
        });
    let builder: WorkspaceRuntimeBuilder = Arc::new(move |root: &std::path::Path| {
        let db = root.join(".latte/state.db");
        std::fs::create_dir_all(db.parent().unwrap()).map_err(|e| e.to_string())?;
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(root)
            .database_path(&db)
            .conversation_root(root.join(".latte/sessions"))
            .build()
            .map_err(|e| e.to_string())?;
        let runtime = latte_headless::thread::ThreadRuntimeService::new(
            engine.clone(),
            root,
            Default::default(),
            factory.clone(),
        );
        let registry = Arc::new(
            latte_headless::registry::ProviderRegistry::parse_jsonc(
                r#"{version:1,default_model:'p/m',providers:{p:{type:'openai-chat',models:['m'],base_url:'https://api.example/v1',api_key:{source:'env',name:'KEY'}}}}"#,
            )
            .map_err(|e| e.to_string())?,
        );
        Ok(latte_server::BuiltWorkspace {
            engine,
            runtime,
            registry,
        })
    });
    new_state(TOKEN.to_string(), builder, Arc::new(|_| None))
}

/// Server state whose workspace builder always fails, so workspace creation
/// surfaces the `failed` (500) error path.
fn failing_builder_state() -> Arc<ServerState> {
    let builder: WorkspaceRuntimeBuilder =
        Arc::new(|_| Err("contract: simulated builder failure".to_string()));
    new_state(TOKEN.to_string(), builder, Arc::new(|_| None))
}

async fn call(
    state: &Arc<ServerState>,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_with_headers(state, method, uri, body, &[]).await
}

async fn call_with_headers(
    state: &Arc<ServerState>,
    method: Method,
    uri: &str,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .uri(uri)
        .method(method)
        .header("authorization", format!("Bearer {TOKEN}"));
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Creates a workspace backed by a caller-owned temp directory (the caller
/// must keep the `TempDir` alive for the test's duration).
async fn create_workspace(state: &Arc<ServerState>, path: &str) -> String {
    let (status, body) = call(
        state,
        Method::POST,
        "/v1/workspaces",
        Some(serde_json::json!({ "path": path })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create workspace: {body:?}");
    body["workspace_id"].as_str().unwrap().to_string()
}

/// Creates a session and waits until it is durably idle.
async fn completed_session(state: &Arc<ServerState>, workspace_id: &str) -> (String, u64) {
    let command_id = uuid::Uuid::now_v7().to_string();
    let thread_id = uuid::Uuid::now_v7().to_string();
    let (status, body) = call_with_headers(
        state,
        Method::POST,
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(serde_json::json!({
            "thread_id": thread_id,
            "command_id": command_id,
            "prompt": "hello",
            "binding": valid_binding(),
        })),
        &[("idempotency-key", &command_id)],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "create session: {body:?}");
    let session_id = body["session_id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        let (status, body) = call(
            state,
            Method::GET,
            &format!("/v1/sessions/{session_id}"),
            None,
        )
        .await;
        if status == StatusCode::OK && body["snapshot"]["lifecycle"].as_str() == Some("ready") {
            return (session_id, body["snapshot"]["revision"].as_u64().unwrap());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("session did not reach a durable idle state");
}

/// 断言 JSON 对象的字段集合与期望完全一致（schema 快照）。
fn assert_keys(value: &Value, expected: &[&str]) {
    let object = value.as_object().expect("expected JSON object");
    let mut actual: Vec<&str> = object.keys().map(String::as_str).collect();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected, "unexpected JSON shape: {value}");
}

// ---------------------------------------------------------------------------
// 响应 schema 快照（设计文档 §3.3）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn response_schema_snapshots() {
    let state = completing_state();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_id = create_workspace(&state, &workspace.path().to_string_lossy()).await;

    // WorkspaceResponse
    let workspace_two = tempfile::tempdir().unwrap();
    let (status, body) = call(
        &state,
        Method::POST,
        "/v1/workspaces",
        Some(serde_json::json!({ "path": workspace_two.path().to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_keys(&body, &["workspace_id", "path"]);

    // SessionCreatedResponse
    let command_id = uuid::Uuid::now_v7().to_string();
    let (status, body) = call_with_headers(
        &state,
        Method::POST,
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(serde_json::json!({
            "thread_id": uuid::Uuid::now_v7().to_string(),
            "command_id": command_id,
            "prompt": "hello",
            "binding": valid_binding(),
        })),
        &[("idempotency-key", &command_id)],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_keys(&body, &["session_id", "accepted_revision"]);
    let session_id = body["session_id"].as_str().unwrap().to_string();

    // 等待 session durable，避免后续断言竞态。
    for _ in 0..200 {
        let (status, body) = call(
            &state,
            Method::GET,
            &format!("/v1/sessions/{session_id}"),
            None,
        )
        .await;
        if status == StatusCode::OK && body["snapshot"]["lifecycle"].as_str() == Some("ready") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // SessionResponse（GET /v1/sessions/{id}）
    let (status, body) = call(
        &state,
        Method::GET,
        &format!("/v1/sessions/{session_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_keys(&body, &["snapshot"]);
    assert_keys(
        &body["snapshot"],
        &[
            "thread_id",
            "revision",
            "sequence",
            "lifecycle",
            "binding",
            "latest_run_id",
            "active_run_id",
            "runs",
            "transcript",
        ],
    );

    // FollowUpResponse
    let command_id = uuid::Uuid::now_v7().to_string();
    let (status, body) = call_with_headers(
        &state,
        Method::POST,
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(serde_json::json!({
            "prompt": "again",
            "expected_thread_revision": body["snapshot"]["revision"].as_u64().unwrap_or(1),
        })),
        &[("idempotency-key", &command_id)],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "follow-up: {body:?}");
    assert_keys(&body, &["accepted_revision", "workspace_id"]);
    assert_eq!(body["workspace_id"].as_str(), Some(workspace_id.as_str()));

    // SessionListResponse（list / search / exact-title 同一信封）
    for uri in [
        format!("/v1/workspaces/{workspace_id}/sessions"),
        format!("/v1/workspaces/{workspace_id}/sessions/search?q=hello"),
        format!("/v1/workspaces/{workspace_id}/sessions/exact-title?q=hello"),
    ] {
        let (status, body) = call(&state, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::OK, "list uri {uri}");
        assert_keys(&body, &["sessions", "next_cursor"]);
    }

    // BindingsResponse
    let (status, body) = call(
        &state,
        Method::GET,
        &format!("/v1/workspaces/{workspace_id}/bindings"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_keys(&body, &["bindings"]);

    // QueueResponse：idle session 上 queue 返回 409（邮箱只在运行中接受排队），
    // 两种结果的信封字段都在契约内。
    let (status, body) = call(
        &state,
        Method::POST,
        &format!("/v1/sessions/{session_id}/queue"),
        Some(serde_json::json!({ "prompt": "queued" })),
    )
    .await;
    assert!(
        status == StatusCode::ACCEPTED || status == StatusCode::CONFLICT,
        "queue unexpected status {status}: {body:?}"
    );
    if status == StatusCode::ACCEPTED {
        assert_keys(&body, &["position"]);
    } else {
        assert_keys(&body["error"], &["type", "message"]);
    }
}

// ---------------------------------------------------------------------------
// 错误类型枚举完整性（设计文档 §4.1）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_enum_completeness() {
    let state = completing_state();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_id = create_workspace(&state, &workspace.path().to_string_lossy()).await;
    let (session_id, revision) = completed_session(&state, &workspace_id).await;

    // unauthorized（401）：无 token。
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/workspaces")
                .method(Method::POST)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/tmp"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // rejected（400）：JSON 语法错误 → 类型化错误信封。
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/v1/workspaces/{workspace_id}/sessions"))
                .method(Method::POST)
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from("{ not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"].as_str(), Some("rejected"));
    assert_keys(&body["error"], &["type", "message"]);

    // not_found（404）：不存在的 session。
    let (status, body) = call(
        &state,
        Method::GET,
        &format!("/v1/sessions/{}", uuid::Uuid::now_v7()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"].as_str(), Some("not_found"));

    // conflict（409）：过期的 revision fence。
    let (status, body) = call(
        &state,
        Method::POST,
        &format!("/v1/sessions/{session_id}/cancel"),
        Some(serde_json::json!({
            "expected_thread_revision": revision + 100,
            "expected_run_revision": 0,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "conflict: {body:?}");
    assert_eq!(body["error"]["type"].as_str(), Some("conflict"));
    assert!(
        body["error"]["current_revision"].is_u64(),
        "conflict must carry current_revision: {body:?}"
    );

    // idempotency_mismatch（422）：同一 Idempotency-Key 搭配不同 payload。
    let key = uuid::Uuid::now_v7().to_string();
    let thread_a = uuid::Uuid::now_v7().to_string();
    let _ = call_with_headers(
        &state,
        Method::POST,
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(serde_json::json!({
            "thread_id": thread_a,
            "command_id": key,
            "prompt": "first",
            "binding": valid_binding(),
        })),
        &[("idempotency-key", &key)],
    )
    .await;
    let (status, body) = call_with_headers(
        &state,
        Method::POST,
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(serde_json::json!({
            "thread_id": uuid::Uuid::now_v7().to_string(),
            "command_id": key,
            "prompt": "different payload",
            "binding": valid_binding(),
        })),
        &[("idempotency-key", &key)],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "422: {body:?}");
    assert_eq!(body["error"]["type"].as_str(), Some("idempotency_mismatch"));

    // failed（500）：workspace builder 失败。
    let failing = failing_builder_state();
    let failing_workspace = tempfile::tempdir().unwrap();
    let (status, body) = call(
        &failing,
        Method::POST,
        "/v1/workspaces",
        Some(serde_json::json!({ "path": failing_workspace.path().to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "500: {body:?}");
    assert_eq!(body["error"]["type"].as_str(), Some("failed"));

    // 错误信封形状恒定：只有 type/message，conflict 额外带 current_revision。
    let (_, body) = call(
        &state,
        Method::GET,
        &format!("/v1/sessions/{}", uuid::Uuid::now_v7()),
        None,
    )
    .await;
    assert_keys(&body["error"], &["type", "message"]);
}

// ---------------------------------------------------------------------------
// SSE 事件类型完整性（设计文档 §5.2）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_event_type_completeness() {
    // ServerEvent 的 serde tag 必须固定为三个契约事件名。
    let changed = serde_json::to_value(latte_server::http::ServerEvent::ThreadChanged {
        session_id: "s".into(),
        revision: 1,
    })
    .unwrap();
    assert_eq!(changed["type"].as_str(), Some("thread_changed"));
    assert_eq!(changed["session_id"].as_str(), Some("s"));
    assert_eq!(changed["revision"].as_u64(), Some(1));

    let progress = serde_json::to_value(latte_server::http::ServerEvent::Progress {
        session_id: "s".into(),
        run_id: "r".into(),
        progress: serde_json::json!({"delta": "x"}),
    })
    .unwrap();
    assert_eq!(progress["type"].as_str(), Some("progress"));

    let resync = serde_json::to_value(latte_server::http::ServerEvent::ResyncRequired).unwrap();
    assert_eq!(resync["type"].as_str(), Some("resync_required"));

    // 端到端：创建 session 后，事件流必须出现 event: thread_changed 帧。
    let state = completing_state();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_id = create_workspace(&state, &workspace.path().to_string_lossy()).await;

    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/v1/workspaces/{workspace_id}/events"))
                .method(Method::GET)
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("accept", "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("text/event-stream"),
        "unexpected content-type {content_type}"
    );

    // 在后台消费事件流，同时创建 session 触发 thread_changed。
    let state_for_task = state.clone();
    let workspace_for_task = workspace_id.clone();
    let _ = completed_session(&state_for_task, &workspace_for_task).await;

    let mut stream = response.into_body().into_data_stream();
    use futures::StreamExt;
    let mut saw_thread_changed = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        let text = String::from_utf8_lossy(&chunk);
        if text.contains("event: thread_changed") {
            saw_thread_changed = true;
            break;
        }
    }
    assert!(
        saw_thread_changed,
        "SSE stream never emitted thread_changed"
    );
}

// ---------------------------------------------------------------------------
// 分页契约（设计文档 §6.2）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pagination_contract() {
    let state = completing_state();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_id = create_workspace(&state, &workspace.path().to_string_lossy()).await;

    // 创建 3 个 session。
    let mut created = Vec::new();
    for _ in 0..3 {
        let (session_id, _) = completed_session(&state, &workspace_id).await;
        created.push(session_id);
    }

    // limit=0 → 空页（不是错误）。
    let (status, body) = call(
        &state,
        Method::GET,
        &format!("/v1/workspaces/{workspace_id}/sessions?limit=0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["sessions"].as_array().unwrap().is_empty());
    assert!(body["next_cursor"].is_null());

    // limit=2 → 第一页 2 条 + cursor；cursor 不透明（不含 thread_id 等可解析字段）。
    let (status, page1) = call(
        &state,
        Method::GET,
        &format!("/v1/workspaces/{workspace_id}/sessions?limit=2"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["sessions"].as_array().unwrap().len(), 2);
    let cursor = page1["next_cursor"].as_str().expect("cursor present");
    for id in &created {
        assert!(!cursor.contains(id), "cursor must be opaque");
    }

    // 第二页 1 条，next_cursor 为 null 表示终止。
    let (status, page2) = call(
        &state,
        Method::GET,
        &format!("/v1/workspaces/{workspace_id}/sessions?limit=2&cursor={cursor}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page2["sessions"].as_array().unwrap().len(), 1);
    assert!(page2["next_cursor"].is_null());

    // 非法 cursor → 400 rejected。
    let (status, body) = call(
        &state,
        Method::GET,
        &format!("/v1/workspaces/{workspace_id}/sessions?cursor=garbage"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"].as_str(), Some("rejected"));

    // limit 超过 200 被截断为 200（不报错）。
    let (status, _) = call(
        &state,
        Method::GET,
        &format!("/v1/workspaces/{workspace_id}/sessions?limit=10000"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Binding 类型化与持久化兼容（设计文档 §3.2 / §9）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn typed_binding_round_trips_through_persistence() {
    let state = completing_state();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_id = create_workspace(&state, &workspace.path().to_string_lossy()).await;
    let binding = valid_binding();
    let command_id = uuid::Uuid::now_v7().to_string();
    let thread_id = uuid::Uuid::now_v7().to_string();
    let (status, body) = call_with_headers(
        &state,
        Method::POST,
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(serde_json::json!({
            "thread_id": thread_id,
            "command_id": command_id,
            "prompt": "hello",
            "binding": binding,
        })),
        &[("idempotency-key", &command_id)],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "create: {body:?}");
    let session_id = body["session_id"].as_str().unwrap().to_string();

    // snapshot 中的 binding 必须与发送的一致（持久化 JSON 反序列化兼容）。
    for _ in 0..200 {
        let (status, body) = call(
            &state,
            Method::GET,
            &format!("/v1/sessions/{session_id}"),
            None,
        )
        .await;
        if status == StatusCode::OK && body["snapshot"]["lifecycle"].as_str() == Some("ready") {
            let persisted: ThreadProviderBindingV2 =
                serde_json::from_value(body["snapshot"]["binding"].clone()).unwrap();
            assert_eq!(persisted, binding, "binding round-trip mismatch");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("session never became ready");
}

#[tokio::test]
async fn invalid_binding_is_rejected_with_typed_error_envelope() {
    let state = completing_state();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_id = create_workspace(&state, &workspace.path().to_string_lossy()).await;
    let command_id = uuid::Uuid::now_v7().to_string();
    let (status, body) = call_with_headers(
        &state,
        Method::POST,
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(serde_json::json!({
            "thread_id": uuid::Uuid::now_v7().to_string(),
            "command_id": command_id,
            "prompt": "hello",
            "binding": {"version": 1},
        })),
        &[("idempotency-key", &command_id)],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"].as_str(), Some("rejected"));
    assert_keys(&body["error"], &["type", "message"]);
}
