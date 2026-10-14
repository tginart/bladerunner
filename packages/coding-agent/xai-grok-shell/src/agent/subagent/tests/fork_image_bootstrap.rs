use super::*;
use crate::session::storage::StorageAdapter;
use xai_chat_state::compaction_image_context::{parse_image_files_paths, render_image_files_block};
use xai_chat_state::compaction_utils::CompactionStateContext;
use xai_grok_sampling_types::ConversationItem;

#[test]
fn normalized_fork_strips_runtime_assets_without_changing_query_literals() {
    let runtime = render_image_files_block(&["/parent/assets/runtime.png".into()]).unwrap();
    let literal = render_image_files_block(&["/parent/assets/literal.png".into()]).unwrap();
    let initial = forked_initial_context(vec![
        ConversationItem::system("system"),
        ConversationItem::user(format!(
            "{runtime}\n\n<user_query>example:\n{literal}</user_query>"
        )),
        ConversationItem::assistant("done"),
    ]);
    let text = initial.conversation.last().unwrap().text_content();
    assert!(!text.contains("/parent/assets/runtime.png"));
    assert!(text.contains(&literal));
    // Legacy unwrapped query text can contain literal envelopes as well.
    for query in [
        format!("please inspect this literal markup: {literal}"),
        format!("{runtime}\n\nplease inspect this literal markup: {literal}"),
    ] {
        let initial = forked_initial_context(vec![
            ConversationItem::system("system"),
            ConversationItem::user(query),
            ConversationItem::assistant("done"),
        ]);
        let text = initial.conversation.last().unwrap().text_content();
        assert!(text.contains(&literal));
        assert!(!text.contains("/parent/assets/runtime.png"));
    }
}

async fn image_fork_fixture(
    normalized: bool,
    block_target_assets: bool,
    from_disk: bool,
    incomplete_tail: bool,
) -> (
    tempfile::TempDir,
    InitialContext,
    Vec<String>,
    std::path::PathBuf,
) {
    let home = tempfile::tempdir().unwrap();
    let _home =
        xai_grok_test_support::EnvGuard::set("BLADERUNNER_HOME", home.path().to_str().unwrap());
    let _auth = xai_grok_test_support::EnvGuard::set(
        "GROK_AUTH_PATH",
        home.path().join("auth.json").to_str().unwrap(),
    );
    let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
    let unique = uuid::Uuid::new_v4();
    let parent = SessionInfo {
        id: acp::SessionId::new(format!("parent-images-{unique}")),
        cwd: "/original".into(),
    };
    let child = SessionInfo {
        id: acp::SessionId::new(format!("child-images-{unique}")),
        cwd: "/child".into(),
    };
    let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
        crate::util::grok_home::grok_home(),
    );
    storage
        .init_session(&parent, acp::ModelId::new("test-model"))
        .await
        .unwrap();
    storage
        .init_session(&child, acp::ModelId::new("test-model"))
        .await
        .unwrap();
    let parent_assets = crate::session::persistence::ensure_owner_only_session_dir(&parent)
        .unwrap()
        .join("assets");
    std::fs::create_dir(&parent_assets).unwrap();
    let mut paths: Vec<_> = ["earlier.png", "latest.png"]
        .into_iter()
        .map(|name| {
            let path = parent_assets.join(name);
            std::fs::write(&path, name).unwrap();
            path.to_string_lossy().into_owned()
        })
        .collect();
    let child_dir = crate::session::persistence::ensure_owner_only_session_dir(&child).unwrap();
    let child_assets = child_dir.join("assets");
    if block_target_assets {
        // An existing regular file cannot be used as the destination directory.
        std::fs::write(&child_assets, "obstruction").unwrap();
    }
    let chat = spawn_test_parent_chat_state("grok-4.5");
    let mut items = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user_meta(render_image_files_block(&paths[..1]).unwrap()),
        ConversationItem::user(format!(
            "{}\n\n<user_query>inspect latest</user_query>",
            render_image_files_block(&paths[1..]).unwrap(),
        )),
        ConversationItem::system_reminder("PARENT_REMINDER_MUST_STAY_FILTERED"),
        ConversationItem::assistant("ready to fork"),
    ];
    if incomplete_tail {
        let path = parent_assets.join("incomplete.png");
        std::fs::write(&path, "incomplete.png").unwrap();
        paths.push(path.to_string_lossy().into_owned());
        items.push(ConversationItem::user(format!(
            "{}\n\n<user_query>INCOMPLETE_INSTRUCTIONS_MUST_STAY_FILTERED</user_query>",
            render_image_files_block(&paths[2..]).unwrap(),
        )));
    }
    if from_disk {
        for item in &items {
            storage.append_chat_message(&parent, item).await.unwrap();
        }
    }
    chat.replace_conversation(items);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = if from_disk { None } else { Some(chat) };
    ctx.parent_session_id = parent.id.to_string();
    ctx.parent_cwd = PathBuf::from(&parent.cwd);
    // Exercise both authoritative session info and the live-only fallback lookup.
    ctx.parent_session_info = if normalized && !from_disk {
        None
    } else {
        Some(parent)
    };
    let request = bootstrap_test_request(true);
    let output = bootstrap_initial_context(
        &request,
        None,
        &ctx,
        &child,
        &child_dir,
        "test-model",
        if normalized { 1 } else { 256_000 },
    )
    .await;
    let BootstrapInitialContext::Ready(initial) = output else {
        panic!("fork must fail open rather than abort");
    };
    if !block_target_assets {
        std::fs::remove_dir_all(parent_assets).unwrap();
    }
    (home, initial, paths, child_assets)
}

#[tokio::test]
#[serial_test::serial]
async fn live_verbatim_and_normalized_forks_rebase_images_before_child_compaction() {
    for normalized in [false, true] {
        let (_home, initial, old_paths, child_assets) =
            image_fork_fixture(normalized, false, false, false).await;
        assert_eq!(initial.source, InitialContextSource::Forked);
        assert!(initial.copy_error.is_none());
        assert!(matches!(
            initial.conversation.first(),
            Some(ConversationItem::System(_))
        ));
        assert_eq!(initial.prefix_len, Some(initial.conversation.len()));
        assert!(
            !initial.verbatim_fork,
            "rebased paths cannot claim a byte-identical cached prefix"
        );
        let text = initial
            .conversation
            .iter()
            .map(ConversationItem::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.contains("<background_context>"), normalized);
        assert_eq!(
            fork_context_normalized(&initial.source, &initial.conversation),
            normalized,
            "image rebasing alone does not mean the fork was normalized"
        );
        if normalized {
            assert!(!text.contains("PARENT_REMINDER_MUST_STAY_FILTERED"));
            for path in &old_paths {
                assert!(
                    !text.contains(path),
                    "normalized background must not retain parent assets"
                );
            }
        }
        let mut conversation = initial.conversation;
        conversation.push(ConversationItem::user("continue after the fork"));
        let mut state = CompactionStateContext::build(&conversation, Default::default())
            .await
            .for_compaction();
        crate::session::helpers::session_compact::verify_compaction_images(
            &mut state.images,
            &child_assets,
        )
        .await;
        let mut kept = state.images.attached_paths;
        kept.extend(parse_image_files_paths(
            state
                .images
                .last_turn_image_files
                .as_deref()
                .unwrap_or_default(),
        ));
        assert_eq!(
            kept.len(),
            2,
            "earlier CompactionMeta and latest image must both survive"
        );
        assert_eq!(std::fs::read_dir(&child_assets).unwrap().count(), 2);
        let mut contents = Vec::new();
        for path in kept {
            assert!(!old_paths.contains(&path));
            assert_eq!(Path::new(&path).parent(), Some(child_assets.as_path()));
            contents.push(std::fs::read_to_string(path).unwrap());
        }
        contents.sort();
        assert_eq!(contents, ["earlier.png", "latest.png"]);
    }
}

#[tokio::test]
#[serial_test::serial]
async fn live_image_copy_failure_falls_open_with_recorded_error() {
    let (_home, initial, _paths, _assets) = image_fork_fixture(false, true, false, false).await;
    assert_eq!(initial.source, InitialContextSource::New);
    assert!(initial.copy_error.is_some());
    assert!(initial.conversation.is_empty());
    assert!(initial.prefix_len.is_none());
    assert!(!initial.verbatim_fork);
}

#[tokio::test]
#[serial_test::serial]
async fn disk_fork_keeps_copied_image_note_through_second_filter() {
    let (_home, initial, old_paths, child_assets) =
        image_fork_fixture(true, false, true, false).await;
    assert_eq!(initial.source, InitialContextSource::Forked);
    assert!(initial.copy_error.is_none());
    assert!(matches!(
        initial.conversation.first(),
        Some(ConversationItem::System(_))
    ));
    assert_eq!(initial.prefix_len, Some(initial.conversation.len()));
    let text = initial
        .conversation
        .iter()
        .map(ConversationItem::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("<background_context>"));
    assert!(!text.contains("PARENT_REMINDER_MUST_STAY_FILTERED"));
    let mut conversation = initial.conversation;
    conversation.push(ConversationItem::user("continue in child"));
    let mut state = CompactionStateContext::build(&conversation, Default::default())
        .await
        .for_compaction();
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut state.images,
        &child_assets,
    )
    .await;
    assert_eq!(state.images.attached_paths.len(), 2);
    assert_eq!(
        std::fs::read_dir(&child_assets).unwrap().count(),
        2,
        "same-session rebasing must retain the two copied files without duplicating them"
    );
    let mut contents = Vec::new();
    for path in state.images.attached_paths {
        assert!(!old_paths.contains(&path));
        assert_eq!(Path::new(&path).parent(), Some(child_assets.as_path()));
        contents.push(std::fs::read_to_string(path).unwrap());
    }
    contents.sort();
    assert_eq!(contents, ["earlier.png", "latest.png"]);
}

#[tokio::test]
#[serial_test::serial]
async fn disk_fork_preserves_incomplete_turn_images_without_incomplete_instructions() {
    let (_home, initial, _old_paths, child_assets) =
        image_fork_fixture(true, false, true, true).await;
    assert_eq!(initial.source, InitialContextSource::Forked);
    assert!(initial.copy_error.is_none());
    assert_eq!(initial.prefix_len, Some(initial.conversation.len()));
    let text = initial
        .conversation
        .iter()
        .map(ConversationItem::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("INCOMPLETE_INSTRUCTIONS_MUST_STAY_FILTERED"));
    assert!(!text.contains("PARENT_REMINDER_MUST_STAY_FILTERED"));
    let mut conversation = initial.conversation;
    conversation.push(ConversationItem::user("continue after disk fork"));
    let mut state = CompactionStateContext::build(&conversation, Default::default())
        .await
        .for_compaction();
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut state.images,
        &child_assets,
    )
    .await;
    assert_eq!(state.images.attached_paths.len(), 3);
    let mut contents = state
        .images
        .attached_paths
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>();
    contents.sort();
    assert_eq!(contents, ["earlier.png", "incomplete.png", "latest.png"]);
    assert_eq!(std::fs::read_dir(child_assets).unwrap().count(), 3);
}
