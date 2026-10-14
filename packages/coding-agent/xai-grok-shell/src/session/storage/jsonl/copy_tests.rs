use crate::sampling::ConversationItem;
use crate::session::info::Info;
use crate::session::persistence::{CHAT_FORMAT_VERSION, default_model_id};
use crate::session::storage::{
    CopySessionOptions, JsonlStorageAdapter, SessionUpdate, StorageAdapter,
};
use crate::tools::todo::TodoState;
use agent_client_protocol as acp;
use std::path::Path;
use tempfile::TempDir;

#[tokio::test]
async fn forks_require_complete_hash_matching_evidence_before_accepting_history() {
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_owned());
    let source = Info {
        id: acp::SessionId::new("pending-evidence-parent"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let mut request = xai_grok_sampling_types::ConversationRequest::from_items(vec![
        ConversationItem::system("pending system"),
        ConversationItem::user("hello"),
    ]);
    let tools_sha = request.tools_sha256();
    let system_sha = request.system_sha256().unwrap();
    let mut assistant = ConversationItem::assistant("done");
    if let ConversationItem::Assistant(item) = &mut assistant {
        item.record_request(Some(&tools_sha), Some(&system_sha), None);
    }
    request.items.push(assistant);
    for item in &request.items {
        adapter.append_chat_message(&source, item).await.unwrap();
    }
    let tools_path = adapter
        .session_dir(&source)
        .join(format!("request_tools/{tools_sha}.json"));
    let system_path = adapter
        .session_dir(&source)
        .join(format!("system_prompts/{system_sha}.txt"));
    for live in [true, false] {
        let target = Info {
            id: acp::SessionId::new(if live {
                "pending-live-child"
            } else {
                "pending-disk-child"
            }),
            cwd: "/project".into(),
        };
        // The assistant is visible, but its writer has not published either file.
        for path in [&tools_path, &system_path] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("{error}"),
            }
        }
        for stage in 0..4 {
            let result = if live {
                adapter
                    .rebase_fork_image_assets(&source, &target, request.items.clone(), vec![])
                    .await
                    .map(|_| ())
            } else {
                adapter
                    .copy_session_data(&source, &target, CopySessionOptions::default())
                    .await
                    .map(|_| ())
            };
            if stage < 3 {
                let error = result.unwrap_err();
                assert_eq!(
                    error.kind(),
                    if stage == 2 {
                        std::io::ErrorKind::InvalidData
                    } else {
                        std::io::ErrorKind::NotFound
                    },
                    "live: {live}, stage: {stage}"
                );
                assert!(
                    !adapter.chat_file(&target).exists(),
                    "incomplete evidence cannot commit inherited history"
                );
            } else {
                result.unwrap();
            }
            match stage {
                0 => {
                    std::fs::create_dir_all(tools_path.parent().unwrap()).unwrap();
                    std::fs::write(&tools_path, serde_json::to_vec(&request.tools).unwrap())
                        .unwrap();
                }
                1 => {
                    std::fs::create_dir_all(system_path.parent().unwrap()).unwrap();
                    std::fs::write(&system_path, b"corrupt system content").unwrap();
                }
                2 => std::fs::write(&system_path, b"pending system").unwrap(),
                _ => {}
            }
        }
        let directory = adapter.session_dir(&target);
        assert_eq!(
            std::fs::read(directory.join(format!("request_tools/{tools_sha}.json"))).unwrap(),
            b"[]"
        );
        assert_eq!(
            std::fs::read(directory.join(format!("system_prompts/{system_sha}.txt"))).unwrap(),
            b"pending system"
        );
    }
}

#[tokio::test]
async fn forks_keep_request_evidence_without_the_parent_session() {
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_owned());
    let source = Info {
        id: acp::SessionId::new("evidence-parent"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let request = xai_grok_sampling_types::ConversationRequest::from_items(vec![
        ConversationItem::system("Original /project system prompt"),
        ConversationItem::user("hi"),
    ]);
    let tools_sha = request.tools_sha256();
    let system_sha = request.system_sha256().unwrap();
    let mut assistant = ConversationItem::assistant("done");
    if let ConversationItem::Assistant(item) = &mut assistant {
        item.record_request(Some(&tools_sha), Some(&system_sha), None);
    }
    for item in request.items.iter().chain(std::iter::once(&assistant)) {
        adapter.append_chat_message(&source, item).await.unwrap();
    }
    let source_dir = adapter.session_dir(&source);
    let tools_bytes = serde_json::to_vec(&request.tools).unwrap();
    let archived_sha =
        xai_grok_sampling_types::ConversationRequest::from_items(vec![ConversationItem::system(
            "Earlier head",
        )])
        .system_sha256()
        .unwrap();
    let files = [
        (format!("request_tools/{tools_sha}.json"), tools_bytes),
        (
            format!("system_prompts/{system_sha}.txt"),
            b"Original /project system prompt".to_vec(),
        ),
        // Evidence may also belong to archived calls no longer in live history.
        (
            format!("system_prompts/{archived_sha}.txt"),
            b"Earlier head".to_vec(),
        ),
    ];
    for (relative, bytes) in &files {
        let path = source_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let mut targets = Vec::new();
    for (id, cwd, options) in [
        ("ordinary-child", "/project", CopySessionOptions::default()),
        (
            "worktree-child",
            "/project-worktree",
            CopySessionOptions {
                skip_cwd_transform: true,
                ..Default::default()
            },
        ),
        (
            "subagent-child",
            "/project",
            CopySessionOptions {
                fork_filter: true,
                ..Default::default()
            },
        ),
    ] {
        let target = Info {
            id: acp::SessionId::new(id),
            cwd: cwd.into(),
        };
        adapter
            .copy_session_data(&source, &target, options)
            .await
            .unwrap();
        targets.push(target);
    }
    std::fs::remove_dir_all(source_dir).unwrap();
    for target in targets {
        let chat = std::fs::read_to_string(adapter.chat_file(&target)).unwrap();
        let inherited = chat
            .lines()
            .map(|line| serde_json::from_str::<ConversationItem>(line).unwrap())
            .find_map(|item| match item {
                ConversationItem::Assistant(item) => Some(item),
                _ => None,
            })
            .expect("inherited assistant");
        assert_eq!(inherited.tools_sha256.as_deref(), Some(tools_sha.as_str()));
        assert_eq!(
            inherited.system_sha256.as_deref(),
            Some(system_sha.as_str())
        );
        for (relative, bytes) in &files {
            assert_eq!(
                std::fs::read(adapter.session_dir(&target).join(relative)).unwrap(),
                *bytes
            );
        }
    }
}

#[tokio::test]
async fn live_fork_caps_runtime_references_without_rewriting_query_literals() {
    use xai_chat_state::compaction_image_context::{
        parse_image_files_paths, render_image_files_block,
    };
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_owned());
    let source = Info {
        id: acp::SessionId::new("cap-source"),
        cwd: "/project".into(),
    };
    let target = Info {
        id: acp::SessionId::new("cap-child"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let assets = adapter.session_dir(&source).join("assets");
    std::fs::create_dir(&assets).unwrap();
    let paths: Vec<_> = (0..35)
        .map(|index| {
            let path = assets.join(format!("{index}.png"));
            std::fs::write(&path, index.to_string()).unwrap();
            path.to_string_lossy().into_owned()
        })
        .collect();
    let literal = render_image_files_block(&paths[..1]).unwrap();
    let mut conversation: Vec<_> = paths
        .iter()
        .map(|path| {
            ConversationItem::user(format!(
                "{}\n\n<user_query>inspect</user_query>",
                render_image_files_block(std::slice::from_ref(path)).unwrap()
            ))
        })
        .collect();
    conversation.push(ConversationItem::user(format!(
        "<user_query>literal example:\n{literal}</user_query>"
    )));
    let child = adapter
        .rebase_fork_image_assets(&source, &target, conversation, paths.clone())
        .await
        .unwrap();
    let inherited: Vec<_> = child
        .iter()
        .flat_map(|item| parse_image_files_paths(&item.text_content()))
        .collect();
    assert_eq!(
        inherited.len(),
        32,
        "uncopied runtime references must disappear"
    );
    assert!(child.last().unwrap().text_content().contains(&literal));
    std::fs::remove_dir_all(assets).unwrap();
    let contents: Vec<_> = inherited
        .iter()
        .map(|path| {
            assert_eq!(
                Path::new(path).parent(),
                Some(adapter.session_dir(&target).join("assets").as_path())
            );
            std::fs::read_to_string(path)
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .collect();
    assert_eq!(contents, (3..35).collect::<Vec<_>>());
}

#[tokio::test]
async fn copied_replay_preserves_raw_literal_image_envelopes() {
    use xai_chat_state::compaction_image_context::render_image_files_block;
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_owned());
    let source = Info {
        id: acp::SessionId::new("literal-source"),
        cwd: "/project".into(),
    };
    let target = Info {
        id: acp::SessionId::new("literal-child"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let assets = adapter.session_dir(&source).join("assets");
    std::fs::create_dir(&assets).unwrap();
    let path = assets.join("image.png").to_string_lossy().into_owned();
    std::fs::write(&path, "image").unwrap();
    let block = render_image_files_block(&[path]).unwrap();
    let literal = format!("Please inspect this literal markup:\n{block}");
    adapter
        .append_chat_message(
            &source,
            &ConversationItem::user(format!("{block}\n\n<user_query>{literal}</user_query>")),
        )
        .await
        .unwrap();
    adapter
        .append_update(&source, &fork_user_chunk(source.id.0.as_ref(), &literal, 0))
        .await
        .unwrap();
    adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .unwrap();
    let lines = std::fs::read_to_string(adapter.updates_file(&target)).unwrap();
    let copied = crate::session::storage::SessionUpdateEnvelope::from_str(lines.trim()).unwrap();
    let SessionUpdate::Acp(notification) = copied else {
        panic!("ACP user update");
    };
    let acp::SessionUpdate::UserMessageChunk(chunk) = notification.update else {
        panic!("user text");
    };
    let acp::ContentBlock::Text(text) = chunk.content else {
        panic!("text chunk");
    };
    assert_eq!(text.text, literal, "raw user input must be byte-identical");
}

fn fork_user_chunk(session_id: &str, text: &str, prompt_index: usize) -> SessionUpdate {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
        text.to_string(),
    )))
    .meta(
        serde_json::json!({ "promptIndex": prompt_index })
            .as_object()
            .cloned(),
    );
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        acp::SessionId::new(session_id),
        acp::SessionUpdate::UserMessageChunk(chunk),
    )))
}

fn fork_agent_chunk(session_id: &str, text: &str) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        acp::SessionId::new(session_id),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text.to_string()),
        ))),
    )))
}

fn fork_rewind_marker(session_id: &str, target_prompt_index: usize) -> SessionUpdate {
    use crate::extensions::notification::{
        SessionNotification as XaiSessionNotification, SessionUpdate as XaiSessionUpdateType,
    };
    SessionUpdate::Xai(Box::new(XaiSessionNotification {
        session_id: acp::SessionId::new(session_id),
        update: XaiSessionUpdateType::RewindMarker {
            target_prompt_index,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        meta: None,
    }))
}

fn chat_user(text: &str, prompt_index: usize) -> ConversationItem {
    let mut item = ConversationItem::user(text);
    item.set_prompt_index(prompt_index);
    item
}

/// Fork truncation targets the live branch (dead-branch runs from a
/// prior rewind overlap its stamps, since indices are branch-local) and keeps
/// prompt N inclusive in both the updates and chat (model-context) files.
#[tokio::test]
async fn copy_session_data_fork_truncates_live_branch_inclusive() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-rewound";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    // Prompt 1 was rewound and retried: P1-dead/A1-dead is the dead branch.
    for update in [
        fork_user_chunk(sid, "P0", 0),
        fork_agent_chunk(sid, "A0"),
        fork_user_chunk(sid, "P1-dead", 1),
        fork_agent_chunk(sid, "A1-dead"),
        fork_rewind_marker(sid, 1),
        fork_user_chunk(sid, "P1b", 1),
        fork_agent_chunk(sid, "A1b"),
        fork_user_chunk(sid, "P2", 2),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    for item in [
        chat_user("P0", 0),
        ConversationItem::assistant("A0"),
        chat_user("P1b", 1),
        ConversationItem::assistant("A1b"),
        chat_user("P2", 2),
    ] {
        adapter
            .append_chat_message(&source_info, &item)
            .await
            .unwrap();
    }

    let fork_at = |target: usize, fork_id: &str| {
        let target_info = Info {
            id: acp::SessionId::new(fork_id),
            cwd: "/src".to_string(),
        };
        let options = CopySessionOptions {
            target_prompt_index: Some(target),
            ..Default::default()
        };
        (target_info, options)
    };

    // Fork at live prompt 1: keeps P0, A0, P1b, A1b in both files. A raw
    // run count would cut inside the dead branch instead.
    let (target_info, options) = fork_at(1, "fork-at-1");
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 4);
    assert_eq!(result.chat_messages_copied, 4);
    let loaded = adapter.load_session(&target_info).await.unwrap();
    let last = loaded.updates.last().unwrap();
    assert!(
        matches!(
            last,
            SessionUpdate::Acp(n) if matches!(
                &n.update,
                acp::SessionUpdate::AgentMessageChunk(c)
                    if matches!(&c.content, acp::ContentBlock::Text(t) if t.text == "A1b")
            )
        ),
        "fork must end at the live branch's A1b, got {last:?}"
    );

    // Prompt 0 is kept inclusive; an exclusive cut would copy an empty
    // model context here.
    let (target_info, options) = fork_at(0, "fork-at-0");
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 2, "P0 + A0");
    assert_eq!(result.chat_messages_copied, 2, "P0 + A0 in model context");
}

/// Without a `target_prompt_index`, every line streams through: rewind
/// markers and dead branches survive a plain fork. A regression that routes
/// the default path through the rewind filter would strip them silently.
#[tokio::test]
async fn copy_session_data_without_prompt_target_preserves_dead_branches() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-dead-branch";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    for update in [
        fork_user_chunk(sid, "P0", 0),
        fork_agent_chunk(sid, "A0"),
        fork_user_chunk(sid, "P1-dead", 1),
        fork_rewind_marker(sid, 1),
        fork_user_chunk(sid, "P1b", 1),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }

    let target_info = Info {
        id: acp::SessionId::new("fork-plain"),
        cwd: "/src".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        result.updates_copied, 5,
        "dead branch and rewind marker must survive a plain fork"
    );
}

/// The streaming fork copy skips torn or undecodable lines like the load
/// path does, both with and without a prompt-index cut.
#[tokio::test]
async fn copy_session_data_skips_torn_updates_lines() {
    use std::io::Write as _;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-torn";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    for update in [fork_user_chunk(sid, "P0", 0), fork_agent_chunk(sid, "A0")] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    // A torn append (truncated JSON) and an undecodable line.
    let updates_path = adapter.updates_file_path(&source_info).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&updates_path)
        .unwrap();
    file.write_all(b"{\"method\":\"session/update\",\"params\":{tor\n")
        .unwrap();
    file.write_all(&[0xFF, 0xFE, b'\n']).unwrap();
    drop(file);
    adapter
        .append_update(&source_info, &fork_user_chunk(sid, "P1", 1))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-torn"),
        cwd: "/src".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 3, "P0 + A0 + P1, torn lines dropped");
    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.updates.len(), 3);

    let target_info = Info {
        id: acp::SessionId::new("fork-torn-at-0"),
        cwd: "/src".to_string(),
    };
    let options = CopySessionOptions {
        target_prompt_index: Some(0),
        ..Default::default()
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 2, "P0 + A0; torn tail and P1 cut");
}

/// A torn line inside a multi-chunk user run ends the run during the prompt
/// cut, so the second chunk opens a new counted turn, matching replay's
/// raw-line semantics. Pins the boundary so a classifier change is deliberate.
#[tokio::test]
async fn torn_line_inside_user_run_splits_the_run_for_prompt_cut() {
    use std::io::Write as _;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-torn-mid-run";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    for update in [
        fork_user_chunk(sid, "P0", 0),
        fork_agent_chunk(sid, "A0"),
        fork_user_chunk(sid, "P1a", 1),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    let updates_path = adapter.updates_file_path(&source_info).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&updates_path)
        .unwrap();
    file.write_all(b"{torn mid-run\n").unwrap();
    drop(file);
    for update in [
        fork_user_chunk(sid, "P1b", 1),
        fork_agent_chunk(sid, "A1"),
        fork_user_chunk(sid, "P2", 2),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }

    let target_info = Info {
        id: acp::SessionId::new("fork-torn-mid-run"),
        cwd: "/src".to_string(),
    };
    let options = CopySessionOptions {
        target_prompt_index: Some(1),
        ..Default::default()
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    // P1b re-counts as a turn after the torn split, so the cut lands before
    // it: P0, A0, P1a survive. The contiguous-run cut would have kept 5.
    assert_eq!(result.updates_copied, 3, "P0 + A0 + P1a");
}

fn create_test_chat_messages() -> Vec<ConversationItem> {
    vec![
        ConversationItem::user("Hello world"),
        ConversationItem::user("How are you?"),
        ConversationItem::user("Test message"),
    ]
}

fn create_test_notification() -> acp::SessionNotification {
    acp::SessionNotification::new(
        acp::SessionId::new("test-session-123"),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new("Test response".to_string()),
        ))),
    )
}

fn create_test_plan_state() -> TodoState {
    TodoState::default()
}

#[tokio::test]
async fn copy_session_data_copies_compaction_segments_when_enabled() {
    use crate::extensions::notification::CompactionSegmentFile;
    use xai_grok_sampling_types::ConversationItem;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("seg-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    for msg in &create_test_chat_messages() {
        adapter
            .append_chat_message(&source_info, msg)
            .await
            .unwrap();
    }

    // Two compaction segments → compaction/{segment_000.md, segment_001.md, INDEX.md}.
    let seg = |s: &str| CompactionSegmentFile {
        items: vec![ConversationItem::user("a"), ConversationItem::user("b")],
        summary: s.to_string(),
        detail: xai_chat_state::CompactionDetail::Verbose,
        timestamp: "2026-01-01T00:00:00Z".to_string(),
    };
    adapter
        .write_compaction_segment(&source_info, &seg("first"))
        .await
        .unwrap();
    adapter
        .write_compaction_segment(&source_info, &seg("second"))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("seg-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                copy_compaction_segments: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.compaction_segments_copied, 3); // 2 segments + INDEX.md

    let dst = adapter
        .session_dir(&target_info)
        .join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
    assert!(dst.join("segment_000.md").is_file());
    assert!(dst.join("segment_001.md").is_file());
    assert!(dst.join("INDEX.md").is_file());
    assert!(
        std::fs::read_to_string(dst.join("segment_000.md"))
            .unwrap()
            .contains("# HISTORICAL -- DO NOT EDIT")
    );

    let target2 = Info {
        id: acp::SessionId::new("seg-dst-default"),
        cwd: "/target2/workspace".to_string(),
    };
    let result2 = adapter
        .copy_session_data(&source_info, &target2, CopySessionOptions::default())
        .await
        .unwrap();
    assert_eq!(result2.compaction_segments_copied, 0);
    assert!(
        !adapter
            .session_dir(&target2)
            .join(xai_chat_state::compaction_transcript::COMPACTION_DIR)
            .exists()
    );
}

/// A `compaction_checkpoint` record pointing at `compaction_checkpoints/{id}.json`.
fn checkpoint_record(id: &str) -> SessionUpdate {
    checkpoint_record_with_path(id, &format!("compaction_checkpoints/{id}.json"))
}

/// A `compaction_checkpoint` record with an arbitrary `checkpoint_file` path.
fn checkpoint_record_with_path(id: &str, checkpoint_file: &str) -> SessionUpdate {
    use crate::extensions::notification::{
        CompactionCheckpointInfo, SessionNotification as XaiSessionNotification,
        SessionUpdate as XaiSessionUpdateType,
    };
    SessionUpdate::Xai(Box::new(XaiSessionNotification {
        session_id: acp::SessionId::new("ckpt-src"),
        update: XaiSessionUpdateType::CompactionCheckpoint(Box::new(CompactionCheckpointInfo {
            checkpoint_id: id.to_string(),
            prompt_index_at_compaction: 1,
            checkpoint_file: checkpoint_file.to_string(),
            auto_continue: None,
            schema_version: 1,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })),
        meta: None,
    }))
}

/// A user message chunk stamped with `_meta.promptIndex` so
/// `truncate_for_prompt_by` counts it as a turn.
fn prompt_user_chunk(text: &str, prompt_index: usize) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        acp::SessionId::new("ckpt-src"),
        acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                text.to_string(),
            )))
            .meta(
                serde_json::json!({ "promptIndex": prompt_index })
                    .as_object()
                    .cloned(),
            ),
        ),
    )))
}

async fn write_checkpoint_file(adapter: &JsonlStorageAdapter, info: &Info, id: &str) {
    use crate::extensions::notification::CompactionCheckpointFile;
    adapter
        .write_compaction_checkpoint(
            info,
            &CompactionCheckpointFile {
                checkpoint_id: id.to_string(),
                prompt_index_at_compaction: 1,
                compacted_history: vec![],
                schema_version: 1,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                original_user_info: None,
                reread_file_paths: vec![],
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn copied_checkpoints_require_their_own_hash_matching_evidence() {
    use crate::extensions::notification::CompactionCheckpointFile;
    use xai_grok_sampling_types::{RequestEvidence, ToolSpec};

    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_owned());
    let source = Info {
        id: acp::SessionId::new("checkpoint-evidence-parent"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let current = RequestEvidence::new(&[], Some("current system".into()));
    let archived = RequestEvidence::new(
        &[ToolSpec {
            name: "archived_tool".into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
        }],
        Some("earlier system".into()),
    );
    let mut history = Vec::new();
    for evidence in [&current, &archived] {
        let mut assistant = ConversationItem::assistant("done");
        if let ConversationItem::Assistant(item) = &mut assistant {
            item.record_request(
                Some(&evidence.tools.0),
                Some(&evidence.system.as_ref().unwrap().0),
                None,
            );
        }
        history.push(assistant);
        for (relative, bytes) in [
            (
                format!("request_tools/{}.json", evidence.tools.0),
                evidence.tools.1.clone(),
            ),
            (
                format!("system_prompts/{}.txt", evidence.system.as_ref().unwrap().0),
                evidence.system.as_ref().unwrap().1.as_bytes().to_vec(),
            ),
        ] {
            let path = adapter.session_dir(&source).join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }
    // The current chat is intact; only the older checkpoint uses archived evidence.
    adapter
        .append_chat_message(&source, &history[0])
        .await
        .unwrap();
    adapter
        .append_update(&source, &checkpoint_record("older"))
        .await
        .unwrap();
    adapter
        .write_compaction_checkpoint(
            &source,
            &CompactionCheckpointFile {
                checkpoint_id: "older".into(),
                prompt_index_at_compaction: 1,
                compacted_history: vec![
                    history[1].clone(),
                    ConversationItem::assistant("legacy assistant without evidence hashes"),
                ],
                schema_version: 1,
                created_at: "2026-01-01T00:00:00Z".into(),
                original_user_info: None,
                reread_file_paths: vec![],
            },
        )
        .await
        .unwrap();

    for (kind, sha, extension, bytes) in [
        (
            "request_tools",
            &archived.tools.0,
            "json",
            archived.tools.1.as_slice(),
        ),
        (
            "system_prompts",
            &archived.system.as_ref().unwrap().0,
            "txt",
            archived.system.as_ref().unwrap().1.as_bytes(),
        ),
    ] {
        let path = adapter
            .session_dir(&source)
            .join(format!("{kind}/{sha}.{extension}"));
        for corrupt in [false, true] {
            if corrupt {
                std::fs::write(&path, "corrupt evidence").unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            let target = Info {
                id: acp::SessionId::new(format!("checkpoint-evidence-{kind}-{corrupt}")),
                cwd: "/project".into(),
            };
            let error = adapter
                .copy_session_data(&source, &target, Default::default())
                .await
                .unwrap_err();
            assert_eq!(
                error.kind(),
                if corrupt {
                    std::io::ErrorKind::InvalidData
                } else {
                    std::io::ErrorKind::NotFound
                }
            );
            assert!(
                !adapter
                    .session_dir(&target)
                    .join("compaction_checkpoints/older.json")
                    .exists()
            );

            // A filtered copy retains no checkpoint and need not validate its history.
            let filtered = Info {
                id: acp::SessionId::new(format!("filtered-checkpoint-{kind}-{corrupt}")),
                cwd: "/project".into(),
            };
            let result = adapter
                .copy_session_data(
                    &source,
                    &filtered,
                    CopySessionOptions {
                        fork_filter: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(result.compaction_checkpoints_copied, 0);
            std::fs::write(&path, bytes).unwrap();
        }
    }
    let checkpoint_path = adapter
        .session_dir(&source)
        .join("compaction_checkpoints/older.json");
    let original_checkpoint = std::fs::read(&checkpoint_path).unwrap();
    std::fs::write(&checkpoint_path, "unreadable checkpoint history").unwrap();
    let unreadable_target = Info {
        id: acp::SessionId::new("unreadable-checkpoint-child"),
        cwd: "/project".into(),
    };
    let error = adapter
        .copy_session_data(&source, &unreadable_target, Default::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    std::fs::write(&checkpoint_path, original_checkpoint).unwrap();

    let target = Info {
        id: acp::SessionId::new("checkpoint-evidence-recovered"),
        cwd: "/project".into(),
    };
    let result = adapter
        .copy_session_data(&source, &target, Default::default())
        .await
        .unwrap();
    assert_eq!(result.compaction_checkpoints_copied, 1);
    std::fs::remove_dir_all(adapter.session_dir(&source)).unwrap();
    let checkpoint = adapter
        .read_compaction_checkpoint(&target, "compaction_checkpoints/older.json")
        .await
        .unwrap();
    adapter
        .validate_request_evidence_sync(&target, &checkpoint.compacted_history)
        .unwrap();
}

#[tokio::test]
async fn copy_session_data_copies_referenced_compaction_checkpoints() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    // Two records referencing the same file (e.g. a chained fork) must
    // still produce one copy.
    for _ in 0..2 {
        adapter
            .append_update(&source_info, &checkpoint_record("ckpt-a"))
            .await
            .unwrap();
    }
    write_checkpoint_file(&adapter, &source_info, "ckpt-a").await;

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 1);
    assert_eq!(
        result.updates_copied, 2,
        "checkpoint records must be copied"
    );
    let rel = "compaction_checkpoints/ckpt-a.json";
    let copied = std::fs::read(adapter.session_dir(&target_info).join(rel)).unwrap();
    let original = std::fs::read(adapter.session_dir(&source_info).join(rel)).unwrap();
    assert_eq!(copied, original, "checkpoint file must be copied verbatim");
}

#[tokio::test]
async fn fork_filter_copy_skips_compaction_checkpoints() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-a"))
        .await
        .unwrap();
    write_checkpoint_file(&adapter, &source_info, "ckpt-a").await;

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    // fork_filter clears the copied updates, so no record survives and no
    // checkpoint file should come along.
    let result = adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                fork_filter: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert_eq!(
        result.updates_copied, 0,
        "fork_filter clears the transcript"
    );
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints")
            .exists()
    );
}

#[tokio::test]
async fn target_prompt_index_truncation_gates_checkpoint_copy() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    for update in [
        prompt_user_chunk("P0", 0),
        checkpoint_record("ckpt-early"),
        prompt_user_chunk("P1", 1),
        prompt_user_chunk("P2", 2),
        checkpoint_record("ckpt-late"),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    write_checkpoint_file(&adapter, &source_info, "ckpt-early").await;
    write_checkpoint_file(&adapter, &source_info, "ckpt-late").await;

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    // Truncating to prompt 0 keeps [P0, ckpt-early] and drops the rest.
    let result = adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                target_prompt_index: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 1);
    let dst = adapter
        .session_dir(&target_info)
        .join("compaction_checkpoints");
    assert!(
        dst.join("ckpt-early.json").is_file(),
        "record before the cut keeps its checkpoint file"
    );
    assert!(
        !dst.join("ckpt-late.json").exists(),
        "record after the cut must not pull its checkpoint file"
    );
}

#[tokio::test]
async fn dangling_checkpoint_record_copies_without_file() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    // Record present but its file was never written (already-broken source).
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-gone"))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert_eq!(result.updates_copied, 1, "the record itself still copies");
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints/ckpt-gone.json")
            .exists()
    );
}

#[tokio::test]
async fn checkpoint_record_with_non_checkpoint_path_is_not_copied() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    // A doctored record addressing another session file: copying it would
    // clobber the target's rewritten updates.jsonl with raw source bytes.
    adapter
        .append_update(
            &source_info,
            &checkpoint_record_with_path("ckpt-evil", "updates.jsonl"),
        )
        .await
        .unwrap();
    // Real checkpoint dir present so the path-shape guard (not the
    // missing-dir guard) is what rejects the record.
    std::fs::create_dir_all(
        adapter
            .session_dir(&source_info)
            .join("compaction_checkpoints"),
    )
    .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    // The target updates must keep the transformed record (session id
    // rewritten to the fork), not the source file's raw bytes.
    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.updates.len(), 1);
    match &loaded.updates[0] {
        SessionUpdate::Xai(notification) => {
            assert_eq!(notification.session_id.0.as_ref(), "ckpt-dst");
        }
        other => panic!("Expected Xai update, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_checkpoint_file_is_not_copied() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-a"))
        .await
        .unwrap();
    // Plant a symlink where the checkpoint file should be: the copy must
    // not follow it out of the session directory.
    let ckpt_dir = adapter
        .session_dir(&source_info)
        .join("compaction_checkpoints");
    std::fs::create_dir_all(&ckpt_dir).unwrap();
    let outside = temp_dir.path().join("outside.json");
    std::fs::write(&outside, b"outside bytes").unwrap();
    std::os::unix::fs::symlink(&outside, ckpt_dir.join("ckpt-a.json")).unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints/ckpt-a.json")
            .exists()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_checkpoint_dir_is_not_copied() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-a"))
        .await
        .unwrap();
    // Plant the whole compaction_checkpoints dir as a symlink to an
    // outside dir holding a matching .json: nothing may be copied.
    let outside_dir = temp_dir.path().join("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("ckpt-a.json"), b"outside bytes").unwrap();
    std::os::unix::fs::symlink(
        &outside_dir,
        adapter
            .session_dir(&source_info)
            .join("compaction_checkpoints"),
    )
    .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints")
            .exists()
    );
}

#[tokio::test]
async fn copy_session_data_basic() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-session-123"),
        cwd: "/source/workspace".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let messages = create_test_chat_messages();
    for msg in &messages {
        adapter
            .append_chat_message(&source_info, msg)
            .await
            .unwrap();
    }

    let notification = create_test_notification();
    adapter
        .append_update(&source_info, &SessionUpdate::Acp(Box::new(notification)))
        .await
        .unwrap();

    let plan_state = create_test_plan_state();
    adapter
        .write_plan_state(&source_info, &plan_state)
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-source-session-123-abcd1234"),
        cwd: "/target/workspace".to_string(),
    };

    let options = CopySessionOptions {
        parent_session_id: Some("source-session-123".to_string()),
        new_model_id: None,
        target_prompt_index: None,
        ..Default::default()
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    assert_eq!(result.chat_messages_copied, 3);
    assert_eq!(result.updates_copied, 1);
    assert!(result.plan_state_copied);

    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.summary.info.id, target_info.id);
    assert_eq!(loaded.summary.info.cwd, "/target/workspace");
    assert_eq!(loaded.summary.session_kind.as_deref(), Some("fork"));
    assert_eq!(
        loaded.summary.parent_session_id,
        Some("source-session-123".to_string())
    );
    assert!(loaded.summary.forked_at.is_some());
    assert_eq!(loaded.chat_history.len(), 3);
    assert_eq!(loaded.updates.len(), 1);
    match &loaded.updates[0] {
        SessionUpdate::Acp(notification) => {
            assert_eq!(
                notification.session_id.0.as_ref(),
                "fork-source-session-123-abcd1234"
            );
        }
        _ => panic!("Expected ACP update"),
    }
    assert!(loaded.plan_state.is_some());
}

#[tokio::test]
async fn copy_session_data_without_plan() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-no-plan"),
        cwd: "/source/workspace".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    adapter
        .append_chat_message(&source_info, &ConversationItem::user("Hello"))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-source-no-plan-12345678"),
        cwd: "/target/workspace".to_string(),
    };

    let result = adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await
        .unwrap();

    assert_eq!(result.chat_messages_copied, 1);
    assert_eq!(result.updates_copied, 0);
    assert!(!result.plan_state_copied);

    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert!(loaded.plan_state.is_none());
}

#[tokio::test]
async fn copy_session_data_transforms_xai_updates() {
    use crate::extensions::notification::{
        DiffContent, SessionNotification as XaiSessionNotification,
        SessionUpdate as XaiSessionUpdateType,
    };

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-xai"),
        cwd: "/source".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let xai_notification = XaiSessionNotification {
        session_id: acp::SessionId::new("source-xai"),
        update: XaiSessionUpdateType::DiffReview {
            content: vec![DiffContent {
                diff: acp::Diff::new(std::path::PathBuf::from("/test/file.rs"), "new".to_string())
                    .old_text(Some("old".to_string())),
            }],
        },
        meta: None,
    };
    adapter
        .append_update(
            &source_info,
            &SessionUpdate::Xai(Box::new(xai_notification)),
        )
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-source-xai-abcd1234"),
        cwd: "/target".to_string(),
    };

    adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await
        .unwrap();

    let loaded = adapter.load_session(&target_info).await.unwrap();
    match &loaded.updates[0] {
        SessionUpdate::Xai(notification) => {
            assert_eq!(
                notification.session_id.0.as_ref(),
                "fork-source-xai-abcd1234"
            );
        }
        _ => panic!("Expected xAI update"),
    }
}

#[tokio::test]
async fn copy_session_data_source_not_found() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("nonexistent"),
        cwd: "/nonexistent".to_string(),
    };

    let target_info = Info {
        id: acp::SessionId::new("fork-nonexistent-abcd1234"),
        cwd: "/target".to_string(),
    };

    let result = adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn copy_session_data_with_model_override() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-model-test"),
        cwd: "/source".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-model-test"),
        cwd: "/target".to_string(),
    };

    let options = CopySessionOptions {
        parent_session_id: Some("source-model-test".to_string()),
        new_model_id: Some("grok-3".to_string()),
        target_prompt_index: None,
        ..Default::default()
    };
    adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.summary.current_model_id.0.as_ref(), "grok-3");
    assert_eq!(
        loaded.summary.parent_session_id,
        Some("source-model-test".to_string())
    );
}

#[tokio::test]
async fn copy_session_data_skips_tool_state_directory() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-dir-tool-state"),
        cwd: "/source/project".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    adapter
        .append_chat_message(&source_info, &ConversationItem::user("Hello"))
        .await
        .unwrap();

    let source_dir = adapter.session_dir(&source_info);
    std::fs::create_dir_all(source_dir.join("tool_state.json").join("terminal")).unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-dir-tool-state"),
        cwd: "/target/worktree".to_string(),
    };

    let result = adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await
        .unwrap();

    assert!(!result.tool_state_copied);
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("tool_state.json")
            .is_file()
    );
}

#[tokio::test]
async fn copy_fork_provenance_persisted_in_summary() {
    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source_info = Info {
        id: acp::SessionId::new("src-prov"),
        cwd: "/src".to_string(),
    };
    let target_info = Info {
        id: acp::SessionId::new("tgt-prov"),
        cwd: "/tgt".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let options = CopySessionOptions {
        parent_session_id: Some("src-prov".to_string()),
        session_kind: Some("subagent_fork".to_string()),
        fork_context_source: Some("forked".to_string()),
        fork_parent_prompt_id: Some("prompt-42".to_string()),
        ..Default::default()
    };
    adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    let data = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(data.summary.session_kind.as_deref(), Some("subagent_fork"));
    assert_eq!(data.summary.fork_context_source.as_deref(), Some("forked"));
    assert_eq!(
        data.summary.fork_parent_prompt_id.as_deref(),
        Some("prompt-42")
    );
}

#[tokio::test]
async fn copy_session_data_inherits_source_summary_fields() {
    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source_info = Info {
        id: acp::SessionId::new("src-inherit"),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .update_git_head(
            &source_info,
            Some("abc123".into()),
            Some("feature-branch".into()),
        )
        .await
        .unwrap();
    // Set the profile on disk so the assertion is independent of the
    // process-global configured profile.
    let mut src_summary = adapter.read_summary_sync(&source_info).unwrap();
    src_summary.sandbox_profile = Some("workspace".to_string());
    adapter
        .write_summary_sync(&source_info, &src_summary)
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("tgt-inherit"),
        cwd: "/tgt".to_string(),
    };
    adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    let loaded = adapter.load_summary(&target_info).await.unwrap();
    assert_eq!(loaded.head_commit.as_deref(), Some("abc123"));
    assert_eq!(loaded.head_branch.as_deref(), Some("feature-branch"));
    assert_eq!(loaded.sandbox_profile.as_deref(), Some("workspace"));
}

async fn assert_copy_clears_pending_relocation(fork_filter: bool) {
    use crate::session::persistence::PendingCwdSwitchReminder;

    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new(format!("pending-source-{fork_filter}")),
        cwd: "/src".into(),
    };
    let target = Info {
        id: acp::SessionId::new(format!("pending-target-{fork_filter}")),
        cwd: "/target".into(),
    };
    let mut summary = adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    summary.cwd_generation = 3;
    summary.previous_cwd = Some("/older".into());
    summary.pending_cwd_switch_reminder = Some(PendingCwdSwitchReminder {
        cwd_generation: 3,
        previous_cwd: "/src".into(),
        destination_cwd: "/destination".into(),
        content: "switch".into(),
        destination_project_instructions: None,
    });
    adapter.write_summary_sync(&source, &summary).unwrap();
    adapter
        .append_chat_message(
            &source,
            &ConversationItem::working_directory_switch("switch", 3),
        )
        .await
        .unwrap();

    adapter
        .copy_session_data(
            &source,
            &target,
            CopySessionOptions {
                fork_filter,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let copied = adapter.read_summary_sync(&target).unwrap();
    assert_eq!(copied.cwd_generation, 3);
    assert_eq!(copied.previous_cwd.as_deref(), Some("/older"));
    assert!(copied.pending_cwd_switch_reminder.is_none());
    let expected_generation = if fork_filter { 0 } else { 3 };
    assert_eq!(
        copied.cwd_switch_bookkeeping_generation,
        expected_generation
    );
    if !fork_filter {
        let before = copied.num_chat_messages;
        assert!(matches!(
            adapter
                .append_cwd_switch_commit_aware(
                    &target,
                    &ConversationItem::working_directory_switch("switch", 3),
                )
                .await
                .unwrap(),
            xai_chat_state::StrictAppendAck::AlreadyPresent(item)
                if item.text_content() == "switch"
        ));
        let retried = adapter.read_summary_sync(&target).unwrap();
        assert_eq!(retried.num_chat_messages, before);
        assert_eq!(
            adapter
                .read_chat_history_sync(adapter.chat_file(&target), CHAT_FORMAT_VERSION)
                .unwrap()
                .iter()
                .filter(|item| item.working_directory_switch_generation() == Some(3))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn unfiltered_copy_clears_pending_relocation() {
    assert_copy_clears_pending_relocation(false).await;
}

#[tokio::test]
async fn filtered_copy_clears_pending_relocation() {
    assert_copy_clears_pending_relocation(true).await;
}

/// Each sidecar flag gates exactly its own file: one fork per flag disables
/// only that flag and asserts only its file is missing, so a transposed flag
/// or path in the `copy_sidecar_file` call sites fails. A defaults fork then
/// proves all five copy with their contents intact.
#[tokio::test]
async fn sidecar_flags_gate_their_files_independently() {
    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("src-sidecars"),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    std::fs::write(adapter.plan_file(&source), b"plan").unwrap();
    std::fs::write(adapter.signals_file(&source), b"signals").unwrap();
    std::fs::write(adapter.plan_mode_state_file(&source), b"plan-mode").unwrap();
    std::fs::write(
        adapter.session_dir(&source).join("tool_state.json"),
        b"{\"todo\":[]}",
    )
    .unwrap();
    std::fs::write(adapter.announcement_state_file(&source), b"announcements").unwrap();

    type DisableFlag = fn(&mut CopySessionOptions);
    let cases: [(&str, DisableFlag); 5] = [
        ("plan", |o| o.copy_plan_state = false),
        ("signals", |o| o.copy_signals = false),
        ("plan_mode", |o| o.copy_plan_mode_state = false),
        ("tool_state", |o| o.copy_tool_state = false),
        ("announcement", |o| o.copy_announcement_state = false),
    ];
    for (off, (name, disable)) in cases.iter().enumerate() {
        let target = Info {
            id: acp::SessionId::new(format!("tgt-sidecar-off-{name}")),
            cwd: "/tgt".to_string(),
        };
        let mut options = CopySessionOptions::default();
        disable(&mut options);
        let result = adapter
            .copy_session_data(&source, &target, options)
            .await
            .unwrap();
        let copied = [
            result.plan_state_copied,
            result.signals_copied,
            result.plan_mode_state_copied,
            result.tool_state_copied,
            result.announcement_state_copied,
        ];
        let present = [
            adapter.plan_file(&target).exists(),
            adapter.signals_file(&target).exists(),
            adapter.plan_mode_state_file(&target).exists(),
            adapter
                .session_dir(&target)
                .join("tool_state.json")
                .exists(),
            adapter.announcement_state_file(&target).exists(),
        ];
        for (i, (copied, present)) in copied.into_iter().zip(present).enumerate() {
            let expected = i != off;
            assert_eq!(copied, expected, "{name} off: sidecar {i} copied flag");
            assert_eq!(present, expected, "{name} off: sidecar {i} file present");
        }
    }

    let target_on = Info {
        id: acp::SessionId::new("tgt-sidecars-on"),
        cwd: "/tgt".to_string(),
    };
    let result = adapter
        .copy_session_data(&source, &target_on, CopySessionOptions::default())
        .await
        .unwrap();
    assert!(result.plan_state_copied);
    assert!(result.signals_copied);
    assert!(result.plan_mode_state_copied);
    assert!(result.tool_state_copied);
    assert!(result.announcement_state_copied);
    assert_eq!(
        std::fs::read(adapter.plan_file(&target_on)).unwrap(),
        b"plan"
    );
    assert_eq!(
        std::fs::read(adapter.signals_file(&target_on)).unwrap(),
        b"signals"
    );
    assert_eq!(
        std::fs::read(adapter.plan_mode_state_file(&target_on)).unwrap(),
        b"plan-mode"
    );
    assert_eq!(
        std::fs::read(adapter.session_dir(&target_on).join("tool_state.json")).unwrap(),
        b"{\"todo\":[]}"
    );
    assert_eq!(
        std::fs::read(adapter.announcement_state_file(&target_on)).unwrap(),
        b"announcements"
    );
}

/// Boundary matrix for the capped line reader: exactly-cap content is kept,
/// cap-plus-one is discarded without consuming an index (so the two copy
/// passes stay aligned), a drain spanning several read chunks terminates, and
/// an unterminated within-cap tail is kept.
#[test]
fn capped_line_reader_discards_overlong_lines_without_shifting_indexes() {
    fn collect(input: &[u8], cap: usize) -> Vec<(usize, Vec<u8>)> {
        let mut seen = Vec::new();
        super::for_each_jsonl_line_capped(std::io::Cursor::new(input), cap, |index, line| {
            seen.push((index, line.to_vec()));
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
        seen
    }

    // Exactly cap content bytes: kept.
    assert_eq!(collect(b"abcd\n", 4), vec![(0, b"abcd".to_vec())]);
    // One over cap: discarded; the next line takes the next index, not a
    // shifted one.
    assert_eq!(
        collect(b"aa\nxxxxx\nbb\n", 4),
        vec![(0, b"aa".to_vec()), (1, b"bb".to_vec())]
    );
    // Overlong spanning several drain chunks still finds the line end.
    assert_eq!(
        collect(b"xxxxxxxxxxxxxxxxxxxxx\ncc\n", 4),
        vec![(0, b"cc".to_vec())]
    );
    // Overlong unterminated at EOF: drain hits EOF and stops cleanly.
    assert_eq!(collect(b"aa\nxxxxxxxx", 4), vec![(0, b"aa".to_vec())]);
    // Unterminated within-cap tail is kept, matching the uncapped reader.
    assert_eq!(
        collect(b"aa\nbb", 4),
        vec![(0, b"aa".to_vec()), (1, b"bb".to_vec())]
    );
}

#[tokio::test]
async fn fork_copies_image_assets_and_rewrites_chat_replay_and_checkpoint_paths() {
    use crate::extensions::notification::CompactionCheckpointFile;
    use xai_chat_state::compaction_image_context::{
        parse_image_files_paths, render_image_files_block,
    };
    use xai_chat_state::compaction_utils::{
        CompactedHistoryInput, CompactionStateContext, build_compacted_history,
    };
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("image-source"),
        // A custom state directory may itself live under the original cwd.
        cwd: root.path().to_string_lossy().into_owned(),
    };
    let target = Info {
        id: acp::SessionId::new("image-fork"),
        cwd: "/fork".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let assets = adapter.session_dir(&source).join("assets");
    std::fs::create_dir(&assets).unwrap();
    let old_path = assets.join("old.png").to_string_lossy().into_owned();
    let checkpoint_path = assets.join("checkpoint.png").to_string_lossy().into_owned();
    std::fs::write(&old_path, "old image").unwrap();
    std::fs::write(&checkpoint_path, "checkpoint image").unwrap();
    std::fs::write(assets.join("unreferenced.png"), "not inherited").unwrap();
    let image_text = format!(
        "{}\n\n<user_query>\ninspect\n</user_query>",
        render_image_files_block(std::slice::from_ref(&old_path)).unwrap()
    );
    for item in [
        ConversationItem::user(&image_text),
        ConversationItem::user("next task"),
    ] {
        adapter.append_chat_message(&source, &item).await.unwrap();
    }
    let replay_text = format!(
        "{}\n\n{}",
        render_image_files_block(std::slice::from_ref(&old_path)).unwrap(),
        xai_interjection_core::format_interjection("inspect".into()),
    );
    let mut interjection_update = fork_user_chunk(source.id.0.as_ref(), &replay_text, 0);
    if let SessionUpdate::Acp(notification) = &mut interjection_update
        && let acp::SessionUpdate::UserMessageChunk(chunk) = &mut notification.update
    {
        chunk.meta.as_mut().unwrap().remove("promptIndex");
    }
    adapter
        .append_update(&source, &interjection_update)
        .await
        .unwrap();
    adapter
        .append_update(&source, &checkpoint_record("image-checkpoint"))
        .await
        .unwrap();
    adapter
        .write_compaction_checkpoint(
            &source,
            &CompactionCheckpointFile {
                checkpoint_id: "image-checkpoint".into(),
                prompt_index_at_compaction: 1,
                compacted_history: vec![ConversationItem::user_meta(
                    render_image_files_block(std::slice::from_ref(&checkpoint_path)).unwrap(),
                )],
                schema_version: 1,
                created_at: "2026-01-01T00:00:00Z".into(),
                original_user_info: None,
                reread_file_paths: vec![checkpoint_path.clone()],
            },
        )
        .await
        .unwrap();
    let relative = "compaction_checkpoints/image-checkpoint.json";
    let checkpoint_file = adapter.session_dir(&source).join(relative);
    let mut raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&checkpoint_file).unwrap()).unwrap();
    raw["future_metadata"] = serde_json::json!({"preserved": true});
    std::fs::write(&checkpoint_file, serde_json::to_vec(&raw).unwrap()).unwrap();
    adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .unwrap();
    let target_assets = adapter.session_dir(&target).join("assets");
    assert_eq!(
        std::fs::read_dir(&target_assets).unwrap().count(),
        2,
        "copy only referenced assets"
    );
    // The child must no longer depend on its parent's files.
    std::fs::remove_dir_all(&assets).unwrap();
    let chat = adapter
        .read_chat_history_sync(adapter.chat_file(&target), CHAT_FORMAT_VERSION)
        .unwrap();
    let inherited = parse_image_files_paths(&chat[0].text_content());
    assert_eq!(inherited.len(), 1);
    assert_eq!(
        std::path::Path::new(&inherited[0]).parent(),
        Some(target_assets.as_path())
    );
    assert_eq!(std::fs::read_to_string(&inherited[0]).unwrap(), "old image");
    let replay = std::fs::read_to_string(adapter.updates_file(&target)).unwrap();
    assert!(replay.contains(&inherited[0]));
    assert!(!replay.contains(&old_path));
    let checkpoint = adapter
        .read_compaction_checkpoint(&target, relative)
        .await
        .unwrap();
    assert_eq!(checkpoint.schema_version, 1);
    let checkpoint_images =
        parse_image_files_paths(&checkpoint.compacted_history[0].text_content());
    assert_eq!(checkpoint_images, checkpoint.reread_file_paths);
    assert_eq!(
        std::fs::read_to_string(&checkpoint_images[0]).unwrap(),
        "checkpoint image"
    );
    let copied_raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(adapter.session_dir(&target).join(relative)).unwrap(),
    )
    .unwrap();
    assert_eq!(copied_raw["future_metadata"], raw["future_metadata"]);
    let mut state = CompactionStateContext::build(&chat, Default::default())
        .await
        .for_compaction();
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut state.images,
        &target_assets,
    )
    .await;
    assert_eq!(state.images.attached_paths, inherited);
    let history = build_compacted_history(CompactedHistoryInput {
        system_message: ConversationItem::system("system"),
        user_message_prefix: "prefix".into(),
        agents_md_reminder: None,
        state_context: &state,
        compaction_summary: "summary".into(),
        system_reminder: None,
        summary_before_recent: false,
        transcript_hint: None,
        summary_count: 1,
    });
    let mut again = CompactionStateContext::build(&history, Default::default())
        .await
        .for_compaction();
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut again.images,
        &target_assets,
    )
    .await;
    assert_eq!(again.images.attached_paths, inherited);
}

#[cfg(unix)]
#[tokio::test]
async fn fork_asset_copy_excludes_symlinks_nested_files_and_traversal() {
    use xai_chat_state::compaction_image_context::{
        parse_image_files_paths, render_image_files_block,
    };
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("unsafe-image-source"),
        cwd: "/project".into(),
    };
    let target = Info {
        id: acp::SessionId::new("safe-image-fork"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let assets = adapter.session_dir(&source).join("assets");
    std::fs::create_dir(&assets).unwrap();
    std::fs::create_dir(assets.join("nested")).unwrap();
    let good = assets.join("good.png");
    std::fs::write(&good, "good").unwrap();
    std::fs::write(assets.join("nested/hidden.png"), "nested").unwrap();
    let outside = root.path().join("outside.png");
    std::fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, assets.join("link.png")).unwrap();
    let paths = [
        good.clone(),
        assets.join("link.png"),
        assets.join("nested/hidden.png"),
        assets.join("../assets/good.png"),
        assets.join("missing.png"),
        outside.clone(),
    ]
    .map(|p| p.to_string_lossy().into_owned());
    adapter
        .append_chat_message(
            &source,
            &ConversationItem::user(format!(
                "{}\n\n<user_query>inspect</user_query>",
                render_image_files_block(&paths).unwrap()
            )),
        )
        .await
        .unwrap();
    adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .unwrap();
    let child_assets = adapter.session_dir(&target).join("assets");
    assert_eq!(std::fs::read_dir(&child_assets).unwrap().count(), 1);
    let chat = adapter
        .read_chat_history_sync(adapter.chat_file(&target), CHAT_FORMAT_VERSION)
        .unwrap();
    let mut state =
        xai_chat_state::compaction_utils::CompactionStateContext::build(&chat, Default::default())
            .await;
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut state.images,
        &child_assets,
    )
    .await;
    let verified = parse_image_files_paths(state.images.last_turn_image_files.as_deref().unwrap());
    assert_eq!(verified.len(), 1);
    assert_eq!(std::fs::read_to_string(&verified[0]).unwrap(), "good");

    std::fs::remove_dir_all(&assets).unwrap();
    std::os::unix::fs::symlink(root.path(), &assets).unwrap();
    let second = Info {
        id: acp::SessionId::new("symlink-assets-fork"),
        cwd: "/project".into(),
    };
    adapter
        .copy_session_data(&source, &second, CopySessionOptions::default())
        .await
        .unwrap();
    assert!(!adapter.session_dir(&second).join("assets").exists());
}

#[tokio::test]
async fn filtered_fork_preserves_verified_image_refs_without_synthetic_instructions() {
    use xai_chat_state::compaction_image_context::{
        parse_image_files_paths, render_image_files_block,
    };
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_owned());
    let source = Info {
        id: acp::SessionId::new("filtered-image-source"),
        cwd: "/project".into(),
    };
    let target = Info {
        id: acp::SessionId::new("filtered-image-child"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let assets = adapter.session_dir(&source).join("assets");
    std::fs::create_dir(&assets).unwrap();
    let older = assets.join("older.png").to_string_lossy().into_owned();
    std::fs::write(&older, "old image").unwrap();
    let pending = assets.join("pending.png").to_string_lossy().into_owned();
    std::fs::write(&pending, "newest image").unwrap();
    let note = format!(
        "{}\nDO NOT INHERIT THIS SYNTHETIC INSTRUCTION",
        render_image_files_block(&[older]).unwrap()
    );
    for item in [
        ConversationItem::system("system"),
        ConversationItem::user_meta(note),
        ConversationItem::user("current task"),
        ConversationItem::assistant("finished"),
        ConversationItem::user(format!(
            "{}\n\n<user_query>INCOMPLETE_REQUEST</user_query>",
            render_image_files_block(&[pending]).unwrap()
        )),
    ] {
        adapter.append_chat_message(&source, &item).await.unwrap();
    }
    adapter
        .copy_session_data(
            &source,
            &target,
            CopySessionOptions {
                fork_filter: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    std::fs::remove_dir_all(&assets).unwrap();
    let chat = adapter
        .read_chat_history_sync(adapter.chat_file(&target), CHAT_FORMAT_VERSION)
        .unwrap();
    assert!(matches!(chat.first(), Some(ConversationItem::System(_))));
    assert!(
        !chat
            .iter()
            .any(|item| item.text_content().contains("DO NOT INHERIT"))
    );
    let paths: Vec<_> = chat
        .iter()
        .flat_map(|item| parse_image_files_paths(&item.text_content()))
        .collect();
    assert_eq!(paths.len(), 2);
    assert!(
        !chat
            .iter()
            .any(|item| item.text_content().contains("INCOMPLETE_REQUEST"))
    );
    assert_eq!(
        std::path::Path::new(&paths[0]).parent(),
        Some(adapter.session_dir(&target).join("assets").as_path())
    );
    assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), "old image");
    assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), "newest image");
    let mut state =
        xai_chat_state::compaction_utils::CompactionStateContext::build(&chat, Default::default())
            .await;
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut state.images,
        &adapter.session_dir(&target).join("assets"),
    )
    .await;
    assert_eq!(state.images.attached_paths, paths);
}

#[cfg(unix)]
#[tokio::test]
async fn fork_asset_copy_rejects_target_assets_symlink_without_touching_external_files() {
    use xai_chat_state::compaction_image_context::render_image_files_block;
    let root = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(root.path().to_owned());
    let source = Info {
        id: acp::SessionId::new("target-link-source"),
        cwd: "/project".into(),
    };
    let target = Info {
        id: acp::SessionId::new("target-link-child"),
        cwd: "/project".into(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    adapter
        .init_session(&target, default_model_id())
        .await
        .unwrap();
    let source_assets = adapter.session_dir(&source).join("assets");
    std::fs::create_dir(&source_assets).unwrap();
    let path = source_assets
        .join("real.png")
        .to_string_lossy()
        .into_owned();
    std::fs::write(&path, "image").unwrap();
    adapter
        .append_chat_message(
            &source,
            &ConversationItem::user(render_image_files_block(&[path]).unwrap()),
        )
        .await
        .unwrap();
    let outside = TempDir::new().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(outside.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink(outside.path(), adapter.session_dir(&target).join("assets"))
        .unwrap();
    let error = adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    assert_eq!(
        std::fs::metadata(outside.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
}
