use super::*;
use crate::bearer_token_file::BearerTokenFile;
use xai_grok_config::{BearerTokenPath, mcp_bearer_token_file::bearer_token_file_meta};

#[tokio::test]
async fn token_file_skips_oauth_and_bad_metadata_fails_server() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    let app = axum::Router::new().fallback(move || {
        let counter = Arc::clone(&counted);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            axum::http::StatusCode::UNAUTHORIZED
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let ew = xai_file_utils::events::EventWriter::noop();
    let ctx = McpSpawnCtx::session_less(&ew);
    for sse in [false, true] {
        let meta = Some(bearer_token_file_meta("/isolated/nonexistent-token"));
        let server = if sse {
            acp::McpServer::Sse(acp::McpServerSse::new("test", url.clone()).meta(meta))
        } else {
            acp::McpServer::Http(acp::McpServerHttp::new("test", url.clone()).meta(meta))
        };
        let client = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            start_mcp_server(server, None, None, None, &ctx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!client.has_auth());
        assert!(
            client
                .http_config
                .as_ref()
                .unwrap()
                .bearer_token_file
                .is_some()
        );
        assert!(
            client.reset_transport().await,
            "reconnect snapshot keeps HTTP config"
        );
    }
    for value in [serde_json::json!(7), serde_json::json!("relative/token")] {
        let server = acp::McpServer::Http(acp::McpServerHttp::new("test", url.clone()).meta(Some(
            acp::Meta::from_iter([("x.ai/mcp/bearerTokenFile".into(), value)]),
        )));
        assert!(
            start_mcp_server(server, None, None, None, &ctx)
                .await
                .is_err()
        );
    }
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "token-file credentials skip all OAuth discovery requests"
    );
    server_task.abort();
}

#[tokio::test]
async fn actual_transport_strips_static_defaults_preserves_figma_user_agent_and_rotates() {
    use super::tests::{CallToolBehavior, FakeMcpState, fake_handle_get, fake_handle_post};
    use std::sync::atomic::AtomicUsize;
    let captures: Arc<parking_lot::Mutex<Vec<(Vec<String>, String)>>> = Arc::default();
    let capture = Arc::clone(&captures);
    let state = FakeMcpState {
        behavior: CallToolBehavior::HangThenOk { hang_ms: 0 },
        inits: Arc::new(AtomicUsize::new(0)),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::get(fake_handle_get).post(fake_handle_post),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let seen = Arc::clone(&capture);
                async move {
                    let auth = req
                        .headers()
                        .get_all("authorization")
                        .iter()
                        .map(|v| v.to_str().unwrap().to_owned())
                        .collect();
                    let ua = req
                        .headers()
                        .get("user-agent")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    seen.lock().push((auth, ua));
                    next.run(req).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "first").unwrap();
    let client = McpClient::new_http(
        "figma".into(),
        HttpConfig {
            url,
            headers: vec![("aUtHoRiZaTiOn".into(), "Bearer forbidden-static".into())],
            bearer_token_file: Some(BearerTokenFile::new(
                BearerTokenPath::try_from(path.clone()).unwrap(),
            )),
        },
        None,
        None,
    );
    let service = client.ensure_initialized().await.unwrap();
    std::fs::write(&path, "rotated").unwrap();
    service.list_tools(None).await.unwrap();
    let seen = captures.lock().clone();
    assert!(
        seen.iter()
            .any(|(auth, _)| auth == &vec!["Bearer first".to_owned()])
    );
    assert!(
        seen.iter()
            .any(|(auth, _)| auth == &vec!["Bearer rotated".to_owned()])
    );
    for (auth, ua) in seen {
        assert_eq!(auth.len(), 1);
        assert!(!auth[0].contains("forbidden"));
        assert_eq!(ua, "grok-cli");
    }
    drop(service);
    drop(client);
    server.abort();
}
