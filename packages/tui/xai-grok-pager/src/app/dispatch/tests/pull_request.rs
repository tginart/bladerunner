//! Exercise the reviewer's path from a slash command to a scoped AI reply.
use super::*;
use crate::app::app_view::InputOutcome;
use crate::views::{
    modal::ActiveModal,
    pull_request::github::{Item, Snapshot},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn snapshot() -> Snapshot {
    Snapshot {
        badge: "PR #8 · OPEN".into(),
        subject: "Fix resume".into(),
        stats: "1 file · +2 −0".into(),
        checks: "Checks · 3 passed".into(),
        url: "https://github.example/test/repo/pull/8".into(),
        title: "Fix resume".into(),
        head: "abc123".into(),
        overview: Item {
            label: "Overview".into(),
            location: "Description".into(),
            parts: vec!["Resume safely".into()],
        },
        files: vec![Item {
            label: "session.rs".into(),
            location: "session.rs · modified".into(),
            parts: vec!["@@ -1 +1 @@\n+first".into(), "@@ -9 +9 @@\n+second".into()],
        }],
        discussions: vec![],
    }
}

#[test]
fn selected_hunk_reaches_review_chat_and_answer_survives_closing_viewer() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch_send_prompt(&mut app, "/pr #8".into());
    let [
        Effect::FetchPullRequest {
            target, request_id, ..
        },
    ] = effects.as_slice()
    else {
        panic!("slash command must open a viewer: {effects:?}");
    };
    assert_eq!(target, "8");
    dispatch_task_result(
        TaskResult::PullRequestLoaded {
            agent_id: id,
            request_id: *request_id,
            result: Ok(snapshot()),
        },
        &mut app,
    );
    let agent = app.agents.get_mut(&id).unwrap();
    assert!(agent.active_modal.is_none(), "open in the corner");
    dispatch_send_prompt(&mut app, "/pr open".into());
    let agent = app.agents.get_mut(&id).unwrap();
    assert!(matches!(agent.active_modal, Some(ActiveModal::PullRequest)));
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
    agent.handle_modal_key(&press(KeyCode::Right));
    agent.handle_modal_key(&press(KeyCode::Char(']')));
    agent.handle_modal_key(&press(KeyCode::Char('?')));
    agent.handle_modal_paste(
        "Why is this needed?",
        &crate::actions::ActionRegistry::defaults(),
    );
    let InputOutcome::Action(Action::AskReviewChat(request)) =
        agent.handle_modal_key(&press(KeyCode::Enter))
    else {
        panic!("missing review question")
    };
    assert_eq!(request.question, "Why is this needed?");
    assert!(request.anchor.comparison.contains("abc123"));
    assert!(request.anchor.comparison.contains("/pull/8"));
    assert!(request.anchor.patch.contains("+second"));
    assert!(!request.anchor.patch.contains("+first"));
    assert!(matches!(
        dispatch(Action::AskReviewChat(request.clone()), &mut app).as_slice(),
        [Effect::AskReviewChat { .. }]
    ));
    let agent = app.agents.get_mut(&id).unwrap();
    agent.handle_modal_key(&press(KeyCode::Esc));
    agent.handle_modal_key(&press(KeyCode::Esc));
    assert!(agent.active_modal.is_none());
    dispatch_task_result(
        TaskResult::ReviewChatAnswered {
            agent_id: id,
            parent_session_id: request.session_id,
            child_id: request.child_id.clone(),
            response: Ok(xai_grok_shell::extensions::review_chat::AskResponse {
                child_id: request.child_id,
                success: true,
                output: "Scoped answer".into(),
                error: None,
            }),
        },
        &mut app,
    );
    let agent = &app.agents[&id];
    assert!(agent.review_chat.is_none());
    assert_eq!(
        agent.pr_chats.values().next().unwrap().borrow().exchanges[0].answer,
        "Scoped answer"
    );
}

#[test]
fn cancelled_handoff_keeps_the_next_question_on_the_selected_hunk() {
    use crate::views::pull_request::{Outcome, Viewer};
    use crate::views::review_chat::{ReviewChatOutcome, ReviewChatState};
    use std::{cell::RefCell, path::Path, rc::Rc};
    use xai_grok_shell::extensions::review_chat::AskResponse;

    let chat = Rc::new(RefCell::new(ReviewChatState::new(
        "parent".into(),
        Path::new("."),
    )));
    let mut viewer = Viewer::loading();
    viewer.chat = Some(chat.clone());
    viewer.data = Some(snapshot());
    viewer.enter_fullscreen();
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
    viewer.key(&press(KeyCode::Right));
    viewer.key(&press(KeyCode::Char('?')));
    chat.borrow_mut().paste("Explain the first hunk");
    let Outcome::Chat(ReviewChatOutcome::Ask(first)) = viewer.key(&press(KeyCode::Enter)) else {
        panic!("expected a review question");
    };
    assert!(first.anchor.patch.contains("+first"));
    chat.borrow_mut().finish(Ok(AskResponse {
        child_id: first.child_id,
        success: true,
        output: "First hunk explained".into(),
        error: None,
    }));
    viewer.key(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    viewer.key(&press(KeyCode::Tab));
    viewer.key(&press(KeyCode::Char(']')));
    viewer.key(&press(KeyCode::Char('?')));
    chat.borrow_mut().paste("Share this answer");
    let Outcome::Chat(ReviewChatOutcome::Forward(handoff)) =
        viewer.key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    else {
        panic!("expected the draft to retain its source");
    };
    assert!(handoff.contains("+first"));
    assert!(!handoff.contains("+second"));
    viewer.key(&press(KeyCode::Esc));
    viewer.key(&press(KeyCode::Char('?')));
    chat.borrow_mut().paste("Explain this hunk too");
    let Outcome::Chat(ReviewChatOutcome::Ask(next)) = viewer.key(&press(KeyCode::Enter)) else {
        panic!("expected a follow-up question");
    };
    assert!(next.anchor.patch.contains("+second"));
    assert!(!next.anchor.patch.contains("+first"));
}

#[test]
fn old_fetch_cannot_replace_newer_or_dismissed_viewer() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let first = dispatch(Action::OpenPullRequest("8".into()), &mut app);
    let [Effect::FetchPullRequest { request_id, .. }] = first.as_slice() else {
        panic!()
    };
    let complete = || TaskResult::PullRequestLoaded {
        agent_id: id,
        request_id: *request_id,
        result: Ok(snapshot()),
    };
    dispatch(Action::OpenPullRequest("9".into()), &mut app);
    dispatch_task_result(complete(), &mut app);
    assert!(
        app.agents[&id]
            .pull_request
            .as_ref()
            .unwrap()
            .data
            .is_none()
    );
    app.agents.get_mut(&id).unwrap().active_modal = None;
    dispatch_task_result(complete(), &mut app);
    assert!(app.agents[&id].active_modal.is_none());
}

#[test]
fn file_view_renders_selected_hunk_at_wide_and_narrow_widths() {
    use ratatui::{buffer::Buffer, layout::Rect};
    let mut viewer = crate::views::pull_request::Viewer::loading();
    viewer.data = Some(snapshot());
    viewer.key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    viewer.key(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
    for width in [130, 64] {
        let area = Rect::new(0, 0, width, 26);
        let mut buf = Buffer::empty(area);
        viewer.render(&mut buf, area, &crate::theme::Theme::current());
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("session.rs"));
        assert!(text.contains("+second"));
        assert!(!text.contains("+first"));
        assert!(text.contains("? Ask AI"));
    }
}

/// Input: A displayed label in the normal agent frame.
/// Output: The mouse action produced by the same input handler as the terminal.
fn click_label(app: &mut AppView, label: &str) -> Action {
    use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{buffer::Buffer, layout::Rect};
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    let area = Rect::new(0, 0, 132, 38);
    let mut buf = Buffer::empty(area);
    let registry = crate::actions::ActionRegistry::defaults();
    agent.draw(
        area,
        &mut buf,
        &registry,
        &mut crate::scrollback::render::ScratchBuffer::new(),
        None,
        false,
        crate::app::agent_view::BannerSlotParams::none(),
        &crate::app::bundle::BundleState::default(),
        false,
        false,
        &mut vec![],
        crate::app::agent_view::AppRenderParams::default(),
    );
    let rows: Vec<String> = (0..area.height)
        .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect();
    for (row, line) in rows.iter().enumerate() {
        if let Some(byte) = line.find(label) {
            let click = Event::Mouse(MouseEvent {
                column: line[..byte].chars().count() as u16,
                row: row as u16,
                kind: MouseEventKind::Down(MouseButton::Left),
                modifiers: KeyModifiers::NONE,
            });
            let InputOutcome::Action(action) = agent.handle_input(&click, &registry) else {
                panic!("inactive {label}")
            };
            return action;
        }
    }
    panic!("Missing {label} in frame:\n{}", rows.join("\n"));
}

#[test]
fn corner_controls_leave_the_main_draft_intact_and_restore_the_same_pr() {
    use crate::views::pull_request::summary::Display;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch_send_prompt(&mut app, "/pr 8".into());
    let [Effect::FetchPullRequest { request_id, .. }] = effects.as_slice() else {
        panic!()
    };
    dispatch_task_result(
        TaskResult::PullRequestLoaded {
            agent_id: id,
            request_id: *request_id,
            result: Ok(snapshot()),
        },
        &mut app,
    );
    let agent = app.agents.get_mut(&id).unwrap();
    agent.prompt.set_text("draft for main");
    assert!(agent.active_modal.is_none());
    for label in ["[-]", "[+]", "[x]"] {
        let action = click_label(&mut app, label);
        assert!(dispatch(action, &mut app).is_empty());
    }
    assert!(
        app.agents[&id]
            .pull_request
            .as_ref()
            .unwrap()
            .summary
            .display
            == Display::Closed
    );
    assert_eq!(app.agents[&id].prompt.text(), "draft for main");
    assert!(dispatch(Action::OpenPullRequest(String::new()), &mut app).is_empty());
    let action = click_label(&mut app, "[Fullscreen]");
    dispatch(action, &mut app);
    assert!(matches!(
        app.agents[&id].active_modal,
        Some(ActiveModal::PullRequest)
    ));
    let action = click_label(&mut app, "AI digest");
    let Action::AskReviewChat(ask) = action else {
        panic!("digest must use the review companion")
    };
    assert!(ask.question.contains("digest"));
    assert!(ask.anchor.patch.contains("+first") && ask.anchor.patch.contains("+second"));
    assert_eq!(app.agents[&id].prompt.text(), "draft for main");
}

#[test]
fn close_during_fetch_caches_the_result_without_reopening_the_overlay() {
    use crate::views::pull_request::summary::{Control, Display};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch_send_prompt(&mut app, "/pr 8".into());
    let [Effect::FetchPullRequest { request_id, .. }] = effects.as_slice() else {
        panic!()
    };
    dispatch(Action::PullRequestControl(Control::Close), &mut app);
    dispatch_task_result(
        TaskResult::PullRequestLoaded {
            agent_id: id,
            request_id: *request_id,
            result: Ok(snapshot()),
        },
        &mut app,
    );
    let viewer = app.agents[&id].pull_request.as_ref().unwrap();
    assert!(viewer.summary.display == Display::Closed);
    assert!(viewer.data.is_some());
}
