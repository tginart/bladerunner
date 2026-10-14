use super::*;
use crate::bearer_token_file::BearerTokenFile;
use axum::response::IntoResponse;
use xai_grok_config::BearerTokenPath;

type Captures = Arc<parking_lot::Mutex<Vec<(String, Vec<String>)>>>;
async fn capture(
    axum::extract::State(seen): axum::extract::State<Captures>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    seen.lock().push((
        method.to_string(),
        headers
            .get_all("authorization")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect(),
    ));
    if method == axum::http::Method::GET {
        ([("content-type", "text/event-stream")], "").into_response()
    } else {
        axum::http::StatusCode::ACCEPTED.into_response()
    }
}

#[tokio::test]
async fn rotated_file_overrides_auth_token_and_custom_headers_for_all_methods() {
    let seen = Captures::default();
    let app = axum::Router::new()
        .route("/mcp", axum::routing::any(capture))
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri: Arc<str> = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, b"first\n").unwrap();
    let client = McpHttpClient::new(reqwest::Client::new(), "test", WarnBudget::default())
        .with_bearer_token_file(Some(BearerTokenFile::new(
            BearerTokenPath::try_from(path.clone()).unwrap(),
        )));
    let headers = || {
        HashMap::from([(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer forbidden-static"),
        )])
    };
    for (index, token) in ["first", "atomic", "in-place"].iter().enumerate() {
        if index == 1 {
            let replacement = dir.path().join("replacement");
            std::fs::write(&replacement, token).unwrap();
            std::fs::rename(replacement, &path).unwrap();
        }
        if index == 2 {
            std::fs::write(&path, token).unwrap();
        }
        drop(
            client
                .get_stream(
                    Arc::clone(&uri),
                    "sid".into(),
                    None,
                    Some("forbidden-rmcp".into()),
                    headers(),
                )
                .await
                .unwrap(),
        );
        let message: ClientJsonRpcMessage = serde_json::from_value(
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .unwrap();
        client
            .post_message(
                Arc::clone(&uri),
                message,
                Some("sid".into()),
                Some("forbidden-rmcp".into()),
                headers(),
            )
            .await
            .unwrap();
        client
            .delete_session(
                Arc::clone(&uri),
                "sid".into(),
                Some("forbidden-rmcp".into()),
                headers(),
            )
            .await
            .unwrap();
    }
    let captured = seen.lock().clone();
    assert_eq!(captured.len(), 9);
    for (index, (method, auth)) in captured.iter().enumerate() {
        assert_eq!(method, ["GET", "POST", "DELETE"][index % 3]);
        assert_eq!(
            auth,
            &vec![format!(
                "Bearer {}",
                ["first", "atomic", "in-place"][index / 3]
            )]
        );
    }
    std::fs::remove_file(path).unwrap();
    assert!(
        client
            .delete_session(uri, "sid".into(), None, headers())
            .await
            .is_err()
    );
    assert_eq!(
        seen.lock().len(),
        9,
        "missing file never falls back to static credentials"
    );
    server.abort();
}

#[tokio::test]
async fn absent_setting_preserves_auth_and_headers() {
    let client = McpHttpClient::new(reqwest::Client::new(), "test", WarnBudget::default());
    let input = HashMap::from([(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer static"),
    )]);
    let (token, headers) = client
        .resolve_auth::<std::io::Error>(Some("sdk-token".into()), input.clone())
        .await
        .unwrap();
    assert_eq!(token.as_deref(), Some("sdk-token"));
    assert_eq!(headers, input);
}
