use super::support::*;
use super::*;
use xai_grok_agent::prompt::user_message::UserMessageTemplate;
/// Helper: build an actor with `mcp_state` pre-loaded with the given
/// configs and a translated init-progress state. Reuses
/// `create_test_actor` then drives the typed transitions to match
/// the `(initialized, initializing_servers)` shape callers express.
///
/// Mapping:
/// - `initialized=false, init_servers=[]` → `InitProgress::NotStarted`
/// - `initialized=false, init_servers=[...]` → `Starting{handshaking=…}`
/// - `initialized=true,  init_servers=[]` → `Finished{handshaking=∅}`
/// - `initialized=true,  init_servers=[...]` → `Finished{handshaking=…}`
///   (early-finish + bg handshakes still in flight)
async fn actor_with_mcp(
    configs: Vec<acp::McpServer>,
    initialized: bool,
    initializing_servers: Vec<String>,
) -> SessionActor {
    let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
    {
        let mut state = actor.mcp_state.lock().await;
        state.configs = configs;
        state.cancel_init();
        if initialized || !initializing_servers.is_empty() {
            assert!(state.try_start_init());
            state.mark_servers_initializing(initializing_servers);
            if initialized {
                state.finish_init();
            }
        }
    }
    actor
}
fn dummy_stdio_config(name: &str) -> acp::McpServer {
    acp::McpServer::Stdio(
        acp::McpServerStdio::new(name.to_string(), "true")
            .args(vec![])
            .env(vec![]),
    )
}
#[tokio::test(flavor = "current_thread")]
async fn returns_immediately_for_default_template() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_mcp(
                vec![dummy_stdio_config("linear")],
                false,
                vec!["linear".into()],
            )
            .await;
            let start = std::time::Instant::now();
            actor
                .wait_for_mcp_templated_prefix_ready(&UserMessageTemplate::Default)
                .await;
            assert!(start.elapsed() < std::time::Duration::from_millis(50));
        })
        .await;
}

/// Purpose: Verify that a partial completion cannot release a batch waiter.
/// Input: An optional custom template and two pending MCP server names.
/// Output: A pending waiter until both servers complete.
async fn assert_waits_for_complete_batch(template: Option<UserMessageTemplate>) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let actor = actor_with_mcp(
                vec![dummy_stdio_config("fast"), dummy_stdio_config("slow")],
                true,
                vec!["fast".into(), "slow".into()],
            )
            .await;
            let wait = async {
                if let Some(template) = template {
                    actor.wait_for_mcp_templated_prefix_ready(&template).await;
                } else {
                    actor
                        .wait_for_mcp_handshakes_bounded(Duration::from_secs(3))
                        .await;
                }
            };
            tokio::pin!(wait);
            assert!(futures::poll!(&mut wait).is_pending());
            actor.mcp_state.lock().await.mark_server_ready("fast");
            actor.mcp_handshakes_done.notify_waiters();
            assert!(futures::poll!(&mut wait).is_pending());
            actor.mcp_state.lock().await.mark_server_ready("slow");
            actor.mcp_handshakes_done.notify_waiters();
            assert!(futures::poll!(&mut wait).is_ready());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn templated_prefix_waits_after_an_individual_server_completes() {
    assert_waits_for_complete_batch(Some(UserMessageTemplate::Custom("fixture".into()))).await;
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_waits_after_an_individual_server_completes() {
    assert_waits_for_complete_batch(None).await;
}

/// Purpose: Keep one deadline across partial completion notifications.
/// Input: A three-second budget and a notification after two seconds.
/// Output: A timeout at the original deadline.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn partial_completion_does_not_reset_the_wait_deadline() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let actor = actor_with_mcp(
                vec![dummy_stdio_config("fast"), dummy_stdio_config("slow")],
                true,
                vec!["fast".into(), "slow".into()],
            )
            .await;
            let wait = actor.wait_for_mcp_handshake_completion(Duration::from_secs(3));
            tokio::pin!(wait);
            assert!(futures::poll!(&mut wait).is_pending());
            tokio::time::advance(Duration::from_secs(2)).await;
            actor.mcp_state.lock().await.mark_server_ready("fast");
            actor.mcp_handshakes_done.notify_waiters();
            assert!(futures::poll!(&mut wait).is_pending());
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(!wait.await);
        })
        .await;
}
