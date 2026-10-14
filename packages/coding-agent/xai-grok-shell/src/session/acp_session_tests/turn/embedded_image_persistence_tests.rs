//! Query-extracted images must become durable attachments before a later user turn.
use super::support::*;
use super::*;
use base64::Engine;
use xai_chat_state::compaction_image_context::parse_image_files_paths;
use xai_chat_state::compaction_utils::{
    CompactedHistoryInput, CompactionStateContext, build_compacted_history,
};
use xai_grok_sampling_types::ContentPart;
use xai_grok_test_support::{MockInferenceServer, ScriptedResponse};

fn png_data() -> String {
    // Varied pixels keep the valid PNG above the extractor's small-icon cutoff.
    let mut state = 0x12345678u32;
    let image = image::ImageBuffer::from_fn(64, 64, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgba([state as u8, (state >> 8) as u8, (state >> 16) as u8, 255])
    });
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn query_image_survives_later_turn(with_upload: bool) {
    let home = tempfile::tempdir().unwrap();
    let _home =
        xai_grok_test_support::EnvGuard::set("BLADERUNNER_HOME", home.path().to_str().unwrap());
    let _auth = xai_grok_test_support::EnvGuard::set(
        "GROK_AUTH_PATH",
        home.path().join("auth.json").to_str().unwrap(),
    );
    let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
    let server = MockInferenceServer::start().await.unwrap();
    for _ in 0..2 {
        server.enqueue_response(
            "/v1/responses",
            ScriptedResponse::sse(xai_grok_test_support::sse::responses_api_script_exact(
                "done", "test",
            )),
        );
    }
    let (sampler_event_tx, mut sampler_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sampler = xai_grok_sampler::SamplerActor::spawn(
        xai_grok_sampler::SamplerConfig {
            api_key: Some("test-key".into()),
            base_url: server.url(),
            model: "test".into(),
            api_backend: xai_grok_sampler::ApiBackend::Responses,
            context_window: 256_000,
            max_retries: Some(0),
            idle_timeout_secs: Some(10),
            ..Default::default()
        },
        xai_grok_sampler::RetryPolicy {
            max_retries: 0,
            rate_limit_retry_threshold: 0,
            ..Default::default()
        },
        sampler_event_tx,
    );
    let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(message) = gateway_rx.recv().await {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = message {
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(message) = persistence_rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = message {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.session_info.id = acp::SessionId::new(format!("query-images-{}", uuid::Uuid::new_v4()));
    actor.sampler_handle = sampler;
    let mut config = actor.chat_state_handle.get_sampling_config().await.unwrap();
    config.base_url = server.url();
    config.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
    actor.chat_state_handle.update_sampling_config(config);
    let mut credentials = actor.chat_state_handle.get_credentials().await;
    credentials.api_key = Some("test-key".into());
    actor.chat_state_handle.update_credentials(credentials);
    let actor = Arc::new(actor);
    let drainer = actor.clone();
    tokio::task::spawn_local(async move {
        while let Some(event) = sampler_event_rx.recv().await {
            drainer.handle_sampling_event(event).await;
        }
    });
    let data = png_data();
    assert_eq!(
        xai_grok_tools::util::base64_images::extract_base64_images(format!(
            "data:image/png;base64,{data}"
        ))
        .images
        .len(),
        1,
        "fixture must enter the embedded-image extraction branch",
    );
    let mut blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
        "Inspect this data:image/png;base64,{data}"
    )))];
    if with_upload {
        blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
            data,
            "image/png",
        )));
    }
    for (id, blocks) in [
        ("query-images-first", blocks),
        (
            "query-images-later",
            vec![acp::ContentBlock::Text(acp::TextContent::new("next task"))],
        ),
    ] {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            actor.handle_prompt(
                id,
                blocks,
                PromptMode::Agent,
                None,
                None,
                None,
                None,
                true,
                false,
                None,
                None,
                None,
            ),
        )
        .await
        .expect("mock turn must finish")
        .expect("mock turn must succeed");
    }
    let conversation = actor.chat_state_handle.get_conversation().await;
    let expected_images = if with_upload { 2 } else { 1 };
    let image_turn = conversation
        .iter()
        .find(|item| match item {
            ConversationItem::User(user) => user
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::Image { .. })),
            _ => false,
        })
        .expect("first real turn contains inline image parts");
    let image_count = match image_turn {
        ConversationItem::User(user) => user
            .content
            .iter()
            .filter(|part| matches!(part, ContentPart::Image { .. }))
            .count(),
        _ => unreachable!(),
    };
    assert_eq!(
        image_count, expected_images,
        "each image remains inline exactly once"
    );
    let paths = parse_image_files_paths(&image_turn.text_content());
    assert_eq!(
        paths.len(),
        expected_images,
        "ordinary and extracted images are persisted exactly once"
    );
    let session_dir =
        crate::session::persistence::ensure_owner_only_session_dir(&actor.session_info).unwrap();
    let assets = session_dir.join("assets");
    assert_eq!(std::fs::read_dir(&assets).unwrap().count(), expected_images);
    for path in &paths {
        assert_eq!(std::path::Path::new(path).parent(), Some(assets.as_path()));
        assert!(std::fs::symlink_metadata(path).unwrap().is_file());
    }
    let mut context = CompactionStateContext::build(&conversation, Default::default()).await;
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut context.images,
        &assets,
    )
    .await;
    assert!(
        context.images.last_turn_image_parts.is_empty(),
        "later real turn is text only"
    );
    assert_eq!(context.images.attached_paths, paths);
    let compacted = build_compacted_history(CompactedHistoryInput {
        system_message: ConversationItem::system("system"),
        user_message_prefix: "prefix".into(),
        agents_md_reminder: None,
        state_context: &context.for_compaction(),
        compaction_summary: "done".into(),
        system_reminder: None,
        summary_before_recent: false,
        transcript_hint: None,
        summary_count: 1,
    });
    let mut repeated = CompactionStateContext::build(&compacted, Default::default()).await;
    crate::session::helpers::session_compact::verify_compaction_images(
        &mut repeated.images,
        &assets,
    )
    .await;
    assert_eq!(
        repeated.images.attached_paths, paths,
        "references survive repeated compaction"
    );
    std::fs::remove_dir_all(session_dir).unwrap();
}

fn run_image_turn_test(with_upload: bool) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new()
                .block_on(&runtime, query_image_survives_later_turn(with_upload));
        })
        .unwrap()
        .join()
        .expect("image prompt test thread");
}

#[test]
#[serial_test::serial]
fn embedded_query_image_persists_after_later_real_turn() {
    run_image_turn_test(false);
}

#[test]
#[serial_test::serial]
fn mixed_upload_and_embedded_query_images_persist_once() {
    run_image_turn_test(true);
}
