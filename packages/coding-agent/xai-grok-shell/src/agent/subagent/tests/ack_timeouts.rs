//! The child completion path must not hang on a parent or a child actor
//! that never answers. Each test parks the responder and checks that the
//! bounded wait gives up after its timeout.

use super::super::handle_request::{
    CHILD_ACTOR_ACK_TIMEOUT, PARENT_ACK_TIMEOUT, await_parent_ack, child_actor_query,
    mark_child_usage_not_applied_with_fallback, record_subagent_usage,
};
use super::*;

/// Hold every command that arrives so that its responder is never dropped
/// and never answered, like a parent whose `cmd_rx` is starved.
fn park_commands(mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>) {
    tokio::spawn(async move {
        let mut parked = Vec::new();
        while let Some(cmd) = cmd_rx.recv().await {
            parked.push(cmd);
        }
        std::future::pending::<()>().await;
    });
}

#[tokio::test(start_paused = true)]
async fn usage_fold_gives_up_when_the_parent_never_acks() {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    park_commands(cmd_rx);
    let started = tokio::time::Instant::now();

    let acked = record_subagent_usage(
        Some(&cmd_tx),
        Some(vec![(
            "model".to_string(),
            xai_chat_state::UsageTotals::default(),
        )]),
        None,
        false,
    )
    .await;

    assert!(!acked, "an unanswered fold must count as not applied");
    assert!(started.elapsed() >= PARENT_ACK_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn not_applied_mark_falls_back_to_the_coordinator_after_the_timeout() {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    park_commands(cmd_rx);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let coordinator = tokio::spawn(async move {
        let Some(SubagentEvent::MarkUsageNotApplied(request)) = event_rx.recv().await else {
            panic!("expected the coordinator fallback mark");
        };
        let _ = request.respond_to.send(());
        request.prompt_id
    });

    mark_child_usage_not_applied_with_fallback(
        Some(&cmd_tx),
        &event_tx,
        "parent-session",
        Some("parent-prompt".to_string()),
    )
    .await;

    assert_eq!(coordinator.await.unwrap(), "parent-prompt");
}

#[tokio::test(start_paused = true)]
async fn parent_ack_returns_none_after_the_timeout() {
    let started = tokio::time::Instant::now();
    let ack = await_parent_ack("never", std::future::pending::<()>()).await;
    assert!(ack.is_none());
    assert_eq!(started.elapsed(), PARENT_ACK_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn child_actor_query_uses_the_fallback_after_the_timeout() {
    let started = tokio::time::Instant::now();
    let value = child_actor_query("never", std::future::pending::<u64>(), 7).await;
    assert_eq!(value, 7);
    assert_eq!(started.elapsed(), CHILD_ACTOR_ACK_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn child_actor_query_returns_a_prompt_answer() {
    let value = child_actor_query("ready", async { 3u64 }, 7).await;
    assert_eq!(value, 3);
}
