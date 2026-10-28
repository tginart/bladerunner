//! Purpose: Verify that child bootstrap uses immutable n-shot launch context.
//! Input: Frozen conversations and a different live parent conversation.
//! Output: Bootstrap results preserve the snapshot, including an empty conversation.

use super::*;
use xai_grok_sampling_types::conversation::ConversationItem;
use xai_grok_tools::implementations::grok_build::task::types::SubagentForkSnapshot;

/// Purpose: Keep later parent messages out of every queued shot.
/// Input: A frozen parent conversation and a live parent with different content.
/// Output: The child inherits only the frozen content.
#[tokio::test]
async fn frozen_launch_bootstrap_ignores_later_parent_messages() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = bootstrap_test_request(true);
    let frozen = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("FROZEN_LAUNCH_CONTEXT"),
        ConversationItem::assistant("ready"),
    ];
    request.runtime_overrides.fork_snapshot = Some(Arc::new(SubagentForkSnapshot {
        workspace_path: temp.path().to_path_buf(),
        conversation: serde_json::to_value(frozen).unwrap(),
    }));
    let mut ctx = ctx_with_toggle(HashMap::new());
    let chat = spawn_test_parent_chat_state("grok-4.5");
    chat.replace_conversation(vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("LATER_PARENT_CONTEXT"),
        ConversationItem::assistant("later response"),
    ]);
    ctx.parent_chat_state = Some(chat);
    ctx.parent_session_info = None;
    let child = SessionInfo {
        id: acp::SessionId::new("frozen-launch-child"),
        cwd: temp.path().to_string_lossy().into_owned(),
    };
    let result = bootstrap_initial_context(
        &request,
        None,
        &ctx,
        &child,
        temp.path(),
        "test-model",
        128_000,
    )
    .await;
    let BootstrapInitialContext::Ready(initial) = result else {
        panic!("valid frozen context must bootstrap");
    };
    assert_eq!(initial.source, InitialContextSource::Forked);
    let text = initial
        .conversation
        .iter()
        .map(ConversationItem::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("FROZEN_LAUNCH_CONTEXT"));
    assert!(!text.contains("LATER_PARENT_CONTEXT"));
}

/// Purpose: Permit n-shot as the first command without a live-context fallback.
/// Input: An empty frozen conversation and a later nonempty live parent.
/// Output: The child starts fresh from the empty snapshot.
#[tokio::test]
async fn frozen_launch_bootstrap_accepts_empty_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = bootstrap_test_request(true);
    request.runtime_overrides.fork_snapshot = Some(Arc::new(SubagentForkSnapshot {
        workspace_path: temp.path().to_path_buf(),
        conversation: serde_json::json!([]),
    }));
    let mut ctx = ctx_with_toggle(HashMap::new());
    let chat = spawn_test_parent_chat_state("grok-4.5");
    chat.replace_conversation(vec![ConversationItem::user("must not inherit this")]);
    ctx.parent_chat_state = Some(chat);
    let child = SessionInfo {
        id: acp::SessionId::new("empty-frozen-launch-child"),
        cwd: temp.path().to_string_lossy().into_owned(),
    };
    let result = bootstrap_initial_context(
        &request,
        None,
        &ctx,
        &child,
        temp.path(),
        "test-model",
        128_000,
    )
    .await;
    let BootstrapInitialContext::Ready(initial) = result else {
        panic!("an empty launch conversation must remain usable");
    };
    assert_eq!(initial.source, InitialContextSource::New);
    assert!(initial.conversation.is_empty());
}

/// Purpose: Reject corrupt launch context instead of inheriting live parent state.
/// Input: Invalid serialized conversation data.
/// Output: Bootstrap aborts explicitly.
#[tokio::test]
async fn frozen_launch_bootstrap_rejects_invalid_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = bootstrap_test_request(true);
    request.runtime_overrides.fork_snapshot = Some(Arc::new(SubagentForkSnapshot {
        workspace_path: temp.path().to_path_buf(),
        conversation: serde_json::json!("not a conversation"),
    }));
    let ctx = ctx_with_toggle(HashMap::new());
    let child = SessionInfo {
        id: acp::SessionId::new("invalid-frozen-launch-child"),
        cwd: temp.path().to_string_lossy().into_owned(),
    };
    let result = bootstrap_initial_context(
        &request,
        None,
        &ctx,
        &child,
        temp.path(),
        "test-model",
        128_000,
    )
    .await;
    assert!(matches!(result, BootstrapInitialContext::ResumeAbort(_)));
}
