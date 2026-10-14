use super::*;
use crate::views::review_chat::ReviewChatState;
use std::{cell::RefCell, rc::Rc};

#[test]
fn review_chat_survives_close_and_refresh_but_not_session_changes() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let load = |app: &mut AppView| {
        dispatch_task_result(
            TaskResult::ChangesLoaded {
                agent_id: id,
                session_id: app.agents[&id].session.session_id.clone().unwrap(),
                files: vec![],
                hunks: vec![],
                post_action: false,
            },
            app,
        );
    };
    load(&mut app);
    let shared = app.agents[&id].review_chat.clone().unwrap();
    shared.borrow_mut().open(None);
    app.agents.get_mut(&id).unwrap().active_modal = None;
    load(&mut app);
    assert!(Rc::ptr_eq(
        &shared,
        app.agents[&id].review_chat.as_ref().unwrap()
    ));
    assert!(app.agents[&id].active_modal.is_some());
    app.agents
        .get_mut(&id)
        .unwrap()
        .bind_session_id("another-session".into());
    assert!(app.agents[&id].review_chat.is_none());
    load(&mut app);
    assert!(!Rc::ptr_eq(
        &shared,
        app.agents[&id].review_chat.as_ref().unwrap()
    ));
}

#[test]
fn review_handoff_uses_normal_queue_and_preserves_the_main_composer() {
    for busy in [false, true] {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("unfinished main-chat prompt");
        if busy {
            agent.session.state = AgentState::TurnRunning;
        }
        let effects = dispatch(
            Action::ForwardReviewRequest("Requested edit with captured context".into()),
            &mut app,
        );
        assert!(effects.iter().any(|e| matches!(e, Effect::SendPrompt { text, .. } if text == "Requested edit with captured context")));
        assert_eq!(app.agents[&id].prompt.text(), "unfinished main-chat prompt");
        if busy {
            assert!(app.agents[&id].session.state.is_turn_running());
            assert_eq!(
                app.shared_prompt_queue("test-session").unwrap()[0].text,
                "Requested edit with captured context"
            );
        }
    }
}

#[test]
fn review_handoff_preserves_main_images_including_a_parked_turn() {
    use crate::app::agent_view::test_fixtures::simulate_task_output_wait;
    for mode in 0..3 {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        if mode > 0 {
            dispatch(Action::SendPrompt("first turn".into()), &mut app);
        }
        if mode == 2 {
            simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");
        }
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("unfinished main-chat prompt");
        let image = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
            data: vec![1, 2, 3],
            mime_type: "image/png".into(),
        });
        agent.prompt.insert_image(image).unwrap();
        let draft = agent.prompt.text().to_owned();
        let effects = dispatch(
            Action::ForwardReviewRequest("Review edit request".into()),
            &mut app,
        );
        assert!(effects.iter().any(|effect|
            matches!(effect, Effect::SendPrompt { text, .. } if text == "Review edit request")));
        let agent = app.agents.get_mut(&id).unwrap();
        assert_eq!(agent.prompt.text(), draft);
        assert_eq!(agent.prompt.images.len(), 1);
        assert_eq!(
            agent.prompt.drain_images().len(),
            1,
            "image chip must remain usable"
        );
    }
}

#[test]
fn review_handoff_retains_draft_and_context_until_send_is_accepted() {
    use crate::views::review_chat::ReviewChatOutcome;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use xai_grok_shell::extensions::review_chat::{AskResponse, ReviewAnchor};

    for busy in [false, true] {
        for queued_locally in [false, true] {
            let mut app = test_app_with_agent();
            let id = AgentId(0);
            let mut chat =
                ReviewChatState::new("test-session".into(), std::path::Path::new("/tmp"));
            chat.prompt.set_text("Explain this change");
            let request = chat
                .begin_question(ReviewAnchor {
                    path: "sample.py".into(),
                    hunk_id: "h1".into(),
                    patch: "+new".into(),
                    comparison: "baseline".into(),
                })
                .unwrap();
            chat.finish(Ok(AskResponse {
                child_id: request.child_id,
                success: true,
                output: "Explanation".into(),
                error: None,
            }));
            chat.open_draft();
            chat.draft.as_mut().unwrap().set_text("Improve this change");
            let shared = Rc::new(RefCell::new(chat));
            let agent = app.agents.get_mut(&id).unwrap();
            agent.review_chat = Some(shared.clone());
            if busy {
                agent.session.state = AgentState::TurnRunning;
            }
            // Leader mode keeps busy sends in the local prompt queue.
            app.leader_mode = queued_locally;
            app.reconnect_pending = true;
            let send = |app: &mut AppView| {
                let ReviewChatOutcome::Forward(text) = shared.borrow_mut().handle_key(
                    &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                    None,
                ) else {
                    panic!("expected handoff")
                };
                dispatch(Action::ForwardReviewRequest(text), app)
            };
            assert!(send(&mut app).is_empty());
            assert_eq!(
                shared.borrow().draft.as_ref().unwrap().text(),
                "Improve this change"
            );
            assert_eq!(
                shared.borrow().message.as_deref(),
                Some("Not sent. Retry when connected.")
            );
            app.reconnect_pending = false;
            let effects = send(&mut app);
            let forwarded = effects
                .iter()
                .find_map(|e| match e {
                    Effect::SendPrompt { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .or_else(|| {
                    app.agents[&id]
                        .session
                        .pending_prompts
                        .front()
                        .map(|p| p.text.as_str())
                })
                .unwrap();
            assert!(forwarded.starts_with("Improve this change"));
            assert!(forwarded.contains("sample.py"));
            assert!(forwarded.contains("Explanation"));
            assert!(shared.borrow().draft.is_none());
            assert_eq!(
                shared.borrow().message.as_deref(),
                Some("Sent to main chat")
            );
        }
    }
}

#[test]
fn changes_loads_from_a_previous_session_are_discarded() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .bind_session_id("session-b".into());
    dispatch_task_result(
        TaskResult::ChangesLoaded {
            agent_id: id,
            session_id: "test-session".into(),
            files: vec![],
            hunks: vec![],
            post_action: false,
        },
        &mut app,
    );
    assert!(app.agents[&id].review_chat.is_none());
    assert!(app.agents[&id].active_modal.is_none());
    dispatch_task_result(
        TaskResult::ChangesLoaded {
            agent_id: id,
            session_id: "session-b".into(),
            files: vec![],
            hunks: vec![],
            post_action: false,
        },
        &mut app,
    );
    let chat = app.agents[&id].review_chat.clone().unwrap();
    let mut state = crate::views::changes_modal::ChangesModalState::new(vec![], vec![]);
    state.message = Some("Session B".into());
    state.action_in_flight = true;
    app.agents.get_mut(&id).unwrap().active_modal =
        Some(crate::views::modal::ActiveModal::Changes {
            state: Box::new(state),
        });
    dispatch_task_result(
        TaskResult::ChangesFailed {
            agent_id: id,
            session_id: "test-session".into(),
            error: "Session A failed".into(),
        },
        &mut app,
    );
    assert_eq!(chat.borrow().parent_session_id, "session-b");
    let Some(crate::views::modal::ActiveModal::Changes { state }) = &app.agents[&id].active_modal
    else {
        panic!("session B modal must remain open");
    };
    assert_eq!(state.message.as_deref(), Some("Session B"));
    assert!(state.action_in_flight);
    // Also reject late loads once the view has been unbound entirely.
    app.agents.get_mut(&id).unwrap().unbind_session_id();
    dispatch_task_result(
        TaskResult::ChangesLoaded {
            agent_id: id,
            session_id: "session-b".into(),
            files: vec![],
            hunks: vec![],
            post_action: false,
        },
        &mut app,
    );
    assert!(app.agents[&id].review_chat.is_none());
}

#[test]
fn review_streams_are_consumed_without_entering_the_main_transcript() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let mut chat = ReviewChatState::new("test-session".into(), std::path::Path::new("/tmp"));
    chat.prompt.set_text("Why this change?");
    let request = chat
        .begin_question(xai_grok_shell::extensions::review_chat::ReviewAnchor {
            path: "sample.py".into(),
            hunk_id: "h1".into(),
            patch: "+new".into(),
            comparison: "baseline".into(),
        })
        .unwrap();
    let shared = Rc::new(RefCell::new(chat));
    app.agents.get_mut(&id).unwrap().review_chat = Some(shared.clone());
    // The reported child model stays accurate when the main model later changes.
    let models = &mut app.agents.get_mut(&id).unwrap().session.models;
    for (model_id, name) in [
        ("review-model", "Review Model"),
        ("next-model", "Next Model"),
    ] {
        let model_id = acp::ModelId::new(model_id);
        models
            .available
            .insert(model_id.clone(), acp::ModelInfo::new(model_id, name));
    }
    models.current = Some(acp::ModelId::new("review-model"));
    let spawned = xai_grok_shell::extensions::notification::SessionNotification {
        session_id: "test-session".into(),
        update: xai_grok_shell::extensions::notification::SessionUpdate::SubagentSpawned {
            subagent_id: request.child_id.clone(),
            parent_session_id: request.session_id.clone(),
            parent_prompt_id: None,
            child_session_id: request.child_id.clone(),
            subagent_type: xai_grok_shell::extensions::review_chat::AGENT_NAME.into(),
            description: "Review chat".into(),
            effective_context_source: Some("new".into()),
            context_normalized: false,
            capability_mode: Some("read-only".into()),
            persona: None,
            role: None,
            model: Some("review-model".into()),
            resumed_from: None,
            budget: None,
            workflow_run_id: None,
        },
        meta: None,
    };
    assert_eq!(
        crate::app::review_chat::route_lifecycle(&mut app, &spawned),
        Some(true)
    );
    let models = &mut app.agents.get_mut(&id).unwrap().session.models;
    models.current = Some(acp::ModelId::new("next-model"));
    shared.borrow_mut().sync_model_name(models);
    assert_eq!(shared.borrow().model_name, "Review Model");
    let before = app.agents[&id].scrollback.len();
    let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
        acp::ContentBlock::Text(acp::TextContent::new("Explanation")),
    ));
    assert_eq!(
        crate::app::review_chat::route_update(
            &mut app,
            &request.child_id.into(),
            &update,
            &Default::default()
        ),
        Some(true)
    );
    assert_eq!(shared.borrow().exchanges[0].answer, "Explanation");
    assert_eq!(app.agents[&id].scrollback.len(), before);
}

#[test]
fn review_question_refused_during_reconnect_can_be_retried_without_retyping() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let mut chat = ReviewChatState::new("test-session".into(), std::path::Path::new("/tmp"));
    let anchor = xai_grok_shell::extensions::review_chat::ReviewAnchor {
        path: "sample.py".into(),
        hunk_id: "h1".into(),
        patch: "+new".into(),
        comparison: "baseline".into(),
    };
    chat.prompt.set_text("Explain this selected change");
    let request = chat.begin_question(anchor.clone()).unwrap();
    let shared = Rc::new(RefCell::new(chat));
    app.agents.get_mut(&id).unwrap().review_chat = Some(shared.clone());
    app.reconnect_pending = true;
    assert!(dispatch(Action::AskReviewChat(request), &mut app).is_empty());
    assert!(!shared.borrow().busy);
    assert!(shared.borrow().exchanges.is_empty());
    assert_eq!(
        shared.borrow().prompt.text(),
        "Explain this selected change"
    );
    app.reconnect_pending = false;
    let retry = shared.borrow_mut().begin_question(anchor.clone()).unwrap();
    assert_eq!(retry.anchor, anchor);
    let effects = dispatch(Action::AskReviewChat(retry), &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::AskReviewChat { request, .. }]
        if request.question == "Explain this selected change")
    );
    assert!(shared.borrow().busy);
}

#[test]
fn review_session_teardown_cancels_only_active_children_on_the_original_parent() {
    for unbind in [false, true] {
        for busy in [false, true] {
            let mut app = test_app_with_agent();
            let id = AgentId(0);
            let mut chat =
                ReviewChatState::new("test-session".into(), std::path::Path::new("/tmp"));
            chat.busy = busy;
            chat.current_child_id = Some("review-child".into());
            app.agents.get_mut(&id).unwrap().review_chat = Some(Rc::new(RefCell::new(chat)));
            let agent = app.agents.get_mut(&id).unwrap();
            assert!(agent.bind_session_id("test-session".into()).is_empty());
            assert!(agent.review_chat.is_some());
            let effects = if unbind {
                agent.unbind_session_id()
            } else {
                agent.bind_session_id("replacement".into())
            };
            assert!(agent.review_chat.is_none());
            assert_eq!(effects.len(), usize::from(busy));
            if busy {
                assert!(
                    matches!(effects.as_slice(), [Effect::KillSubagent { session_id, subagent_id }]
                    if session_id.0.as_ref() == "test-session" && subagent_id == "review-child")
                );
            }
            assert!(agent.unbind_session_id().is_empty());
        }
    }
}

#[test]
fn review_session_creation_dispatch_delivers_cancellation_without_another_event() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let mut chat = ReviewChatState::new("test-session".into(), std::path::Path::new("/tmp"));
    chat.busy = true;
    chat.current_child_id = Some("review-child".into());
    app.agents.get_mut(&id).unwrap().review_chat = Some(Rc::new(RefCell::new(chat)));
    let effects = dispatch_task_result(
        TaskResult::SessionCreated {
            agent_id: id,
            session_id: "replacement".into(),
            models: None,
            scheduler_background_loops: None,
        },
        &mut app,
    );
    assert!(effects.iter().any(
        |e| matches!(e, Effect::KillSubagent { session_id, subagent_id }
        if session_id.0.as_ref() == "test-session" && subagent_id == "review-child")
    ));
    assert!(app.pending_effects.is_empty());
    assert!(app.agents[&id].review_chat.is_none());
}

#[test]
fn review_agent_removal_delivers_child_cancellation() {
    for through_picker in [false, true] {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let mut chat = ReviewChatState::new("test-session".into(), std::path::Path::new("/tmp"));
        chat.busy = true;
        chat.current_child_id = Some("review-child".into());
        app.agents.get_mut(&id).unwrap().review_chat = Some(Rc::new(RefCell::new(chat)));
        let effects = if through_picker {
            let mut peer = crate::app::agent_view::test_agent_view(Some("peer"), "/tmp".into());
            peer.session.id = AgentId(1);
            app.agents.insert(AgentId(1), peer);
            crate::app::dispatch::session::modal::dispatch_sessions_confirm_close(&mut app, id)
        } else {
            crate::app::dispatch::session::modal::remove_agent_and_cleanup(&mut app, id);
            std::mem::take(&mut app.pending_effects)
        };
        assert!(!app.agents.contains_key(&id));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::KillSubagent { session_id, subagent_id }
            if session_id.0.as_ref() == "test-session" && subagent_id == "review-child")));
        assert!(app.pending_effects.is_empty());
    }
}
