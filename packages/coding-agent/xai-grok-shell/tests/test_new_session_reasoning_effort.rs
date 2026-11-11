//! `/new` and `/clear` open a session without an effort in `_meta`. The
//! effort that the user chose last must still reach the wire, on both spawn
//! paths:
//! - no `modelId`: the session sampler is built at spawn time;
//! - a `modelId`: a model switch rebuilds the sampler from the catalog.
//!
//! The catalog default of the model is `high` and the chosen effort is `low`,
//! so a sampler that falls back to the catalog shows on the wire.

mod acp_harness;

use acp_harness::{AutoApproveClient, RPC_TIMEOUT, connect_and_auth, prompt_turn, run_agent_test};
use agent_client_protocol::{self as acp, Agent as _};
use serde_json::json;
use xai_grok_test_support::{MockInferenceServer, MockModelEntry};

/// The `reasoning_effort` of each main-turn chat-completion request, in order.
fn wire_efforts(server: &MockInferenceServer) -> Vec<Option<String>> {
    server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| {
            r.body
                .as_ref()
                .and_then(|body| body.pointer("/reasoning_effort"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .collect()
}

async fn new_session_with_meta(
    conn: &acp::ClientSideConnection,
    cwd: &std::path::Path,
    meta: Option<serde_json::Value>,
) -> acp::SessionId {
    let request = acp::NewSessionRequest::new(cwd.to_path_buf())
        .meta(meta.and_then(|m| m.as_object().cloned()));
    tokio::time::timeout(RPC_TIMEOUT, conn.new_session(request))
        .await
        .expect("session/new timed out")
        .expect("session/new failed")
        .session_id
}

#[test]
fn new_session_keeps_the_chosen_reasoning_effort() {
    run_agent_test(|cwd, server| async move {
        server.set_models(vec![
            MockModelEntry::new("test-model")
                .with_supports_reasoning_effort(true)
                .with_reasoning_effort("high"),
        ]);
        let (conn, _init) = connect_and_auth(AutoApproveClient, "test-client").await;

        // Choose `low` on a first session, as `/effort low` does.
        let first = new_session_with_meta(&conn, &cwd, None).await;
        tokio::time::timeout(
            RPC_TIMEOUT,
            conn.set_session_model(
                acp::SetSessionModelRequest::new(first.clone(), acp::ModelId::new("test-model"))
                    .meta(json!({ "reasoningEffort": "low" }).as_object().cloned()),
            ),
        )
        .await
        .expect("session/set_model timed out")
        .expect("session/set_model failed");
        prompt_turn(&conn, &first, "first").await;
        assert_eq!(wire_efforts(&server), [Some("low".to_owned())]);

        // `/clear` without a model: the spawn path builds the sampler.
        let cleared = new_session_with_meta(&conn, &cwd, None).await;
        prompt_turn(&conn, &cleared, "after clear").await;
        assert_eq!(
            wire_efforts(&server).last(),
            Some(&Some("low".to_owned())),
            "a new session without a model must keep the chosen effort"
        );

        // `/new` with a model: the follow-up model switch rebuilds the sampler.
        let with_model =
            new_session_with_meta(&conn, &cwd, Some(json!({ "modelId": "test-model" }))).await;
        prompt_turn(&conn, &with_model, "after new").await;
        assert_eq!(
            wire_efforts(&server).last(),
            Some(&Some("low".to_owned())),
            "the model switch of a new session must keep the chosen effort"
        );
    });
}
