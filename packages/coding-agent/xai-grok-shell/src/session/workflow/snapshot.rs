//! Purpose: Freeze the workspace and conversation for one n-shot run.
//! Input: The parent workspace and chat state, or a persisted launch snapshot.
//! Output: Shared launch inputs that remain fixed across queued shots and resume.

use std::path::Path;
use std::sync::Arc;

use xai_grok_tools::implementations::grok_build::task::types::SubagentForkSnapshot;

const SNAPSHOT_FILE: &str = "launch-snapshot.json";

/// Purpose: Freeze the parent conversation and workspace before any shot can start.
/// Input: The source workspace and the parent chat state.
/// Output: A shared snapshot, or an error without a live-state fallback.
pub(crate) async fn capture(
    source: &Path,
    chat_state: &xai_chat_state::ChatStateHandle,
) -> anyhow::Result<Arc<SubagentForkSnapshot>> {
    let conversation = serde_json::to_value(chat_state.get_conversation().await)?;
    let source = source.to_path_buf();
    let id = uuid::Uuid::now_v7().to_string();
    let base = super::super::worktree::worktree_base_dir_for_source(&source)?;
    crate::agent::subagent::ensure_real_dir(&base).map_err(anyhow::Error::msg)?;
    let dest = base.join(format!("n-shot-snapshot-{id}"));
    let workspace_path = tokio::task::spawn_blocking(move || {
        let mut builder = xai_fast_worktree::WorktreeBuilder::new(&source, &dest)
            .working_tree_mode(xai_fast_worktree::WorkingTreeMode::PreserveWorkingTree)
            .worktree_kind(xai_fast_worktree::WorktreeKind::Subagent)
            .session_id(id);
        if let Some(delegate) = super::super::worktree::btrfs_delegate_from_env() {
            builder = builder.btrfs_delegate(delegate);
        }
        match builder.create() {
            Ok(report) => Ok(report.worktree_path),
            Err(error) => {
                if dest.exists()
                    && let Err(cleanup_error) = xai_fast_worktree::remove_worktree(&dest)
                {
                    tracing::warn!(%cleanup_error, "failed to remove incomplete n-shot snapshot");
                }
                Err(error)
            }
        }
    })
    .await??;
    Ok(Arc::new(SubagentForkSnapshot {
        workspace_path,
        conversation,
    }))
}

/// Purpose: Persist launch inputs before the workflow can dispatch a shot.
/// Input: The run directory and its immutable snapshot.
/// Output: A snapshot file, or an error before execution.
pub(crate) fn persist(run_dir: &Path, snapshot: &SubagentForkSnapshot) -> anyhow::Result<()> {
    use std::io::Write;

    std::fs::create_dir_all(run_dir)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(run_dir.join(SNAPSHOT_FILE))?;
    file.write_all(&serde_json::to_vec(snapshot)?)?;
    file.sync_all()?;
    Ok(())
}

/// Purpose: Restore the original launch inputs without a new parent-state capture.
/// Input: The persisted run directory.
/// Output: The original snapshot, or an explicit error when it is unavailable.
pub(crate) fn load(run_dir: &Path) -> anyhow::Result<Arc<SubagentForkSnapshot>> {
    let snapshot: SubagentForkSnapshot =
        serde_json::from_slice(&std::fs::read(run_dir.join(SNAPSHOT_FILE))?)?;
    anyhow::ensure!(
        snapshot.workspace_path.is_absolute() && snapshot.workspace_path.is_dir(),
        "the original n-shot launch workspace is unavailable; start a new run"
    );
    let _: Vec<xai_grok_sampling_types::conversation::ConversationItem> =
        serde_json::from_value(snapshot.conversation.clone())?;
    Ok(Arc::new(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::conversation::ConversationItem;

    /// Purpose: Supply a live parent actor without a model or persistence service.
    /// Input: None.
    /// Output: A chat handle with a synthetic launch conversation.
    fn parent_chat_state() -> xai_chat_state::ChatStateHandle {
        let (persistence, _persist_rx) = xai_chat_state::MockChatPersistence::new();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = xai_grok_sampling_types::SamplingConfig {
            base_url: "https://api.test/v1".into(),
            model: "test-model".into(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: Default::default(),
            adapter_kind: Default::default(),
            request_compat: None,
            endpoint_path: None,
            extra_headers: Default::default(),
            query_params: Default::default(),
            env_http_headers: Default::default(),
            context_window: std::num::NonZeroU64::new(128_000).unwrap(),
            reasoning_effort: None,
            stream_tool_calls: None,
        };
        xai_chat_state::ChatStateActor::spawn(
            vec![
                ConversationItem::system("system"),
                ConversationItem::user("original launch conversation"),
                ConversationItem::assistant("ready"),
            ],
            config,
            Box::new(persistence),
            event_tx,
            tokio_util::sync::CancellationToken::new(),
        )
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Purpose: Verify queued shots and resumed runs use the original launch inputs.
    /// Input: A repository with tracked, untracked, and ignored files, plus a live chat actor.
    /// Output: Both clones and the restored snapshot retain the original content.
    #[tokio::test]
    async fn launch_snapshot_freezes_workspace_and_conversation_and_survives_resume() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        std::fs::write(repo.join("tracked"), "base").unwrap();
        std::fs::write(repo.join(".gitignore"), "dependency\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        std::fs::write(repo.join("tracked"), "launch tracked").unwrap();
        std::fs::write(repo.join("untracked"), "launch untracked").unwrap();
        std::fs::write(repo.join("dependency"), "launch dependency").unwrap();
        let chat = parent_chat_state();
        let snapshot = capture(&repo, &chat).await.unwrap();
        assert!(
            !snapshot.workspace_path.join("dependency").exists(),
            "match normal subagents: do not copy ignored build trees"
        );
        let run_dir = temp.path().join("run");
        persist(&run_dir, &snapshot).unwrap();
        assert!(
            persist(&run_dir, &snapshot).is_err(),
            "launch inputs are immutable"
        );

        let clone_shot = |name: &str| {
            xai_fast_worktree::WorktreeBuilder::new(
                &snapshot.workspace_path,
                temp.path().join(name),
            )
            .working_tree_mode(xai_fast_worktree::WorkingTreeMode::PreserveWorkingTree)
            .create()
            .unwrap()
            .worktree_path
        };
        let first = clone_shot("first");
        std::fs::write(repo.join("tracked"), "later tracked").unwrap();
        std::fs::write(repo.join("untracked"), "later untracked").unwrap();
        std::fs::write(repo.join("dependency"), "later dependency").unwrap();
        chat.replace_conversation(vec![ConversationItem::user("later conversation")]);
        let _ = chat.get_conversation().await;
        let second = clone_shot("second");
        for child in [&first, &second] {
            assert!(!child.join("dependency").exists());
            for (file, expected) in [
                ("tracked", "launch tracked"),
                ("untracked", "launch untracked"),
            ] {
                assert_eq!(std::fs::read_to_string(child.join(file)).unwrap(), expected);
            }
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(child)
                .args(["ls-files", "--others", "--exclude-standard"])
                .output()
                .unwrap();
            assert!(String::from_utf8_lossy(&output.stdout).contains("untracked"));
        }
        let restored = load(&run_dir).unwrap();
        assert_eq!(restored.workspace_path, snapshot.workspace_path);
        assert_eq!(restored.conversation, snapshot.conversation);
        let text = restored.conversation.to_string();
        assert!(text.contains("original launch conversation"));
        assert!(!text.contains("later conversation"));
        for path in [&first, &second, &snapshot.workspace_path] {
            super::super::super::worktree::remove_subagent_worktree(path)
                .await
                .unwrap();
        }
        assert!(
            load(&run_dir).is_err(),
            "resume must not recapture a missing workspace"
        );
        assert!(load(&temp.path().join("missing-run")).is_err());
    }
}
