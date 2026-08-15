//! E2E tests for the HTTP server.

#[cfg(test)]
mod e2e_tests {
    use latte_server::{new_state, run_http};
    use reqwest::Client;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn test_server_e2e() {
        // Start server on a random port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let state = new_state("test-token".to_string(), std::sync::Arc::new(|_| Err("test".to_string())));
        let server = tokio::spawn(async move {
            let app = latte_server::http::router(state);
            axum::serve(listener, app).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        // Test health check (no auth)
        let response = client
            .get(format!("{}/health", base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Test auth required
        let response = client
            .post(format!("{}/v1/workspaces", base))
            .json(&json!({"path": "/tmp"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);

        // Test create workspace
        let response = client
            .post(format!("{}/v1/workspaces", base))
            .header("authorization", "Bearer test-token")
            .json(&json!({"path": "/tmp"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let workspace: serde_json::Value = response.json().await.unwrap();
        assert!(workspace.get("workspace_id").is_some());

        // Test get session not found
        let response = client
            .get(format!(
                "{}/v1/sessions/00000000-0000-0000-0000-000000000000",
                base
            ))
            .header("authorization", "Bearer test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);

        // Cleanup
        server.abort();
    }
}
