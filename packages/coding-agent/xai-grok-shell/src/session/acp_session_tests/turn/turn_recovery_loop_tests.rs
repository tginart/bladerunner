//! Real-turn-loop coverage of turn recovery against a mock Messages server:
//! the `RetryTransient` arm (resubmit, exhaustion, kill switch) and the
//! length-salvage flow (continuation, complete truncated tool calls).
//!
//! Ported from upstream `transient_retry_loop_tests.rs` and
//! `turn/length_salvage_tests.rs`. Upstream drives a 503; here the sampler
//! owns HTTP 5xx, so the transient case is an in-stream `overloaded_error`.
//! The clock is paused, so the 2/10/30 s backoff costs no wall time.

use super::support::*;
use super::*;
use std::sync::Arc;
use std::time::Duration;
use xai_grok_test_support::{MockInferenceServer, MockModelEntry, ScriptedResponse, SseEvent};

const MODEL: &str = "messages-model";

/// `x.ai/session_notification` payloads the client was sent.
type XaiUpdates = Arc<parking_lot::Mutex<Vec<serde_json::Value>>>;

/// The turn future needs a session-sized stack (spawn.rs runs sessions on
/// 8 MiB); the default 2 MiB test stack overflows in debug builds.
fn on_session_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(test)
        .expect("spawn test thread")
        .join()
        .expect("test thread panicked");
}

fn run_paused<F: std::future::Future>(fut: impl FnOnce() -> F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("test runtime");
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        fut().await;
    }));
}

fn drain_gateway(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> XaiUpdates {
    let captured = XaiUpdates::default();
    let sink = captured.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                xai_acp_lib::AcpClientMessage::ExtNotification(args) => {
                    if let Ok(value) = serde_json::from_str(args.params.get()) {
                        sink.lock().push(value);
                    }
                }
                _ => {}
            }
        }
    });
    captured
}

fn drain_persistence(mut rx: tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = msg {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
}

/// `(attempt, max_retries)` of every `retry_state: retrying` update.
fn retrying_events(updates: &XaiUpdates) -> Vec<(u64, u64)> {
    updates
        .lock()
        .iter()
        .filter_map(|value| {
            let update = value.get("update")?;
            if update.get("sessionUpdate")? != "retry_state" || update.get("type")? != "retrying" {
                return None;
            }
            Some((
                update.get("attempt")?.as_u64()?,
                update.get("max_retries")?.as_u64()?,
            ))
        })
        .collect()
}

/// Actor on the Messages backend with an API key, a real sampler that does
/// no retries of its own, and the transient-retry switch as given.
async fn messages_actor(
    server: &MockInferenceServer,
    transient_retry_enabled: bool,
) -> (Arc<SessionActor>, XaiUpdates) {
    let sampling_cfg = xai_grok_sampler::SamplerConfig {
        api_key: Some("test-key".to_string()),
        base_url: server.url(),
        model: MODEL.to_string(),
        api_backend: xai_grok_sampler::ApiBackend::Messages,
        context_window: 256_000,
        max_retries: Some(0),
        idle_timeout_secs: Some(30),
        ..Default::default()
    };
    let (sampler_event_tx, sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_grok_sampler::SamplingEvent>();
    let sampler_handle = xai_grok_sampler::SamplerActor::spawn(
        sampling_cfg,
        xai_grok_sampler::RetryPolicy {
            max_retries: 0,
            rate_limit_retry_threshold: 0,
            ..Default::default()
        },
        sampler_event_tx,
    );

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let xai_updates = drain_gateway(gateway_rx);
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    drain_persistence(persistence_rx);

    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.sampler_handle = sampler_handle;
    // The per-turn config push carries the actor's max_retries; keep the
    // sampler from retrying so each submission is one request.
    actor.max_retries = 0;
    actor.transient_retry.set(if transient_retry_enabled {
        TransientRetryState::for_session(false)
    } else {
        TransientRetryState::default()
    });

    let mut cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor has sampling config");
    cfg.base_url = server.url();
    cfg.api_backend = xai_grok_sampling_types::ApiBackend::Messages;
    cfg.model = MODEL.to_string();
    actor.chat_state_handle.update_sampling_config(cfg);
    let mut creds = actor.chat_state_handle.get_credentials().await;
    creds.api_key = Some("test-key".to_string());
    creds.auth_type = xai_chat_state::AuthType::ApiKey;
    actor.chat_state_handle.update_credentials(creds);

    actor
        .workspace_ops
        .bind_local_session(
            &actor.session_id_string(),
            actor.tool_context.cwd.as_path().to_path_buf(),
            actor.tool_context.hunk_tracker_handle.clone(),
            actor.agent.borrow().tool_bridge().toolset(),
            None,
        )
        .expect("bind_local_session");

    let actor = Arc::new(actor);
    {
        let drainer = actor.clone();
        let mut sampler_event_rx = sampler_event_rx;
        tokio::task::spawn_local(async move {
            while let Some(event) = sampler_event_rx.recv().await {
                drainer.handle_sampling_event(event).await;
            }
        });
    }
    (actor, xai_updates)
}

async fn run_prompt(
    actor: &Arc<SessionActor>,
    prompt_id: &str,
) -> Result<crate::session::commands::PromptTurnOk, acp::Error> {
    let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    let fut = crate::session::acp_session::box_turn_future(|| {
        actor.handle_prompt(
            prompt_id,
            prompt_blocks,
            PromptMode::Agent,
            None,
            None,
            None,
            None,
            true,
            /* send_now */ false,
            None,
            None,
            None,
        )
    });
    tokio::time::timeout(Duration::from_secs(600), fut)
        .await
        .expect("turn must finish within timeout")
}

/// The turn's `/v1/messages` request bodies. The one-shot title call is
/// excluded by its forced `session_title` tool.
fn turn_requests(server: &MockInferenceServer) -> Vec<serde_json::Value> {
    server
        .requests()
        .into_iter()
        .filter(|e| e.path.ends_with("/messages"))
        .filter_map(|e| e.body)
        .filter(|body| {
            !body
                .get("tools")
                .and_then(|t| t.as_array())
                .is_some_and(|tools| {
                    tools
                        .iter()
                        .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("session_title"))
                })
        })
        .collect()
}

fn message_start() -> SseEvent {
    SseEvent::data(
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test", "type": "message", "role": "assistant",
                "content": [], "model": MODEL, "stop_reason": null,
                "usage": {
                    "input_tokens": 10, "output_tokens": 0,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
                }
            }
        })
        .to_string(),
    )
}

/// The stream starts, then the server sends an in-stream overload error.
fn overloaded_mid_stream() -> ScriptedResponse {
    ScriptedResponse::sse(vec![
        message_start(),
        SseEvent::data(
            serde_json::json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}
            })
            .to_string(),
        ),
    ])
}

/// A complete tool call whose response still stops at `max_tokens`.
fn tool_call_cut_at_max_tokens() -> ScriptedResponse {
    ScriptedResponse::sse(vec![
        message_start(),
        SseEvent::data(
            serde_json::json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "call_cut", "name": "read_file", "input": {}}
            })
            .to_string(),
        ),
        SseEvent::data(
            serde_json::json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"a.rs\"}"}
            })
            .to_string(),
        ),
        SseEvent::data(serde_json::json!({"type": "content_block_stop", "index": 0}).to_string()),
        SseEvent::data(
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "max_tokens"},
                "usage": {"output_tokens": 5, "input_tokens": 10}
            })
            .to_string(),
        ),
        SseEvent::data(serde_json::json!({"type": "message_stop"}).to_string()),
    ])
}

async fn messages_server() -> MockInferenceServer {
    MockInferenceServer::start_with_models(vec![
        MockModelEntry::new(MODEL).with_api_backend("messages"),
    ])
    .await
    .expect("mock inference server")
}

#[test]
fn transient_mid_stream_error_resubmits_until_success() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = messages_server().await;
            server.enqueue_response("/v1/messages", overloaded_mid_stream());
            server.enqueue_response("/v1/messages", overloaded_mid_stream());
            let (actor, updates) = messages_actor(&server, true).await;

            let outcome = run_prompt(&actor, "transient-recovers").await;

            assert!(outcome.is_ok(), "the third attempt succeeds: {outcome:?}");
            assert_eq!(turn_requests(&server).len(), 3, "original + two resubmits");
            assert_eq!(retrying_events(&updates), vec![(1, 3), (2, 3)]);
        });
    });
}

#[test]
fn transient_mid_stream_error_exhausts_to_the_terminal_failure() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = messages_server().await;
            for _ in 0..4 {
                server.enqueue_response("/v1/messages", overloaded_mid_stream());
            }
            let (actor, updates) = messages_actor(&server, true).await;

            let outcome = run_prompt(&actor, "transient-exhausts").await;

            assert!(outcome.is_err(), "the fourth failure ends the turn");
            assert_eq!(
                turn_requests(&server).len(),
                4,
                "original + three resubmits"
            );
            assert_eq!(retrying_events(&updates).len(), 3);
        });
    });
}

#[test]
fn transient_retry_switched_off_fails_on_the_first_error() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = messages_server().await;
            server.enqueue_response("/v1/messages", overloaded_mid_stream());
            let (actor, updates) = messages_actor(&server, false).await;

            let outcome = run_prompt(&actor, "transient-off").await;

            assert!(outcome.is_err(), "switch off: the first error is terminal");
            assert_eq!(turn_requests(&server).len(), 1, "no resubmits");
            assert!(retrying_events(&updates).is_empty());
        });
    });
}

/// Every response stops at `max_tokens`: the turn continues twice, then
/// completes truncated. Each continuation request ends with a user message
/// that carries the continue reminder, never with the cut assistant text.
#[test]
fn length_truncated_answer_continues_with_user_terminated_requests() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = messages_server().await;
            server.set_messages_stop_reason("max_tokens");
            let (actor, _updates) = messages_actor(&server, true).await;

            let outcome = run_prompt(&actor, "length-continues").await;

            assert!(
                outcome.is_ok(),
                "a cut answer completes the turn: {outcome:?}"
            );
            let requests = turn_requests(&server);
            assert_eq!(requests.len(), 3, "original + two continuations");
            for (i, body) in requests.iter().enumerate().skip(1) {
                let last = body["messages"]
                    .as_array()
                    .and_then(|m| m.last())
                    .expect("request has messages");
                assert_eq!(
                    last["role"], "user",
                    "continuation {i} must end with a user turn"
                );
                assert!(
                    last.to_string().contains("exceeded the output token limit"),
                    "continuation {i} carries the reminder: {last}"
                );
            }
        });
    });
}

/// A response that stops at `max_tokens` after a complete tool call runs
/// the call; the next request carries its result.
#[test]
fn complete_truncated_tool_call_runs() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = messages_server().await;
            server.enqueue_response("/v1/messages", tool_call_cut_at_max_tokens());
            let (actor, _updates) = messages_actor(&server, true).await;

            let outcome = run_prompt(&actor, "length-tool-call").await;

            assert!(outcome.is_ok(), "the salvaged call runs: {outcome:?}");
            let requests = turn_requests(&server);
            assert_eq!(requests.len(), 2, "the tool step, then the final answer");
            let follow_up = requests[1].to_string();
            assert!(
                follow_up.contains("\"tool_use_id\":\"call_cut\""),
                "the follow-up carries the tool result: {follow_up}"
            );
        });
    });
}
