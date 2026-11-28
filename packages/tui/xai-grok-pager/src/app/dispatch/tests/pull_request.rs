//! Exercise the reviewer's path from a slash command to a scoped AI reply.
use super::*;
use crate::app::app_view::InputOutcome;
use crate::views::{
    modal::ActiveModal,
    pull_request::github::{Item, Snapshot},
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

fn snapshot() -> Snapshot {
    Snapshot {
        badge: "PR #8 · OPEN".into(),
        subject: "Fix resume".into(),
        changed_files: 1,
        additions: 2,
        deletions: 0,
        thread_status: "0 open threads".into(),
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

fn agent_frame(app: &mut AppView) -> ratatui::buffer::Buffer {
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
        crate::app::agent_view::BannerSlotParams {
            mouse_pos: app.last_mouse_pos,
            ..crate::app::agent_view::BannerSlotParams::none()
        },
        &crate::app::bundle::BundleState::default(),
        false,
        false,
        &mut vec![],
        crate::app::agent_view::AppRenderParams::default(),
    );
    buf
}

fn label_position(buf: &ratatui::buffer::Buffer, label: &str) -> (u16, u16) {
    for y in buf.area.y..buf.area.bottom() {
        let row: String = (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        if let Some(byte) = row.find(label) {
            return (buf.area.x + row[..byte].chars().count() as u16, y);
        }
    }
    panic!("Missing {label} from frame");
}

fn pointer_event(kind: MouseEventKind, (column, row): (u16, u16)) -> Event {
    Event::Mouse(MouseEvent {
        column,
        row,
        kind,
        modifiers: KeyModifiers::NONE,
    })
}

fn click_label(app: &mut AppView, label: &str) -> Action {
    let pos = label_position(&agent_frame(app), label);
    let event = pointer_event(MouseEventKind::Down(MouseButton::Left), pos);
    let InputOutcome::Action(action) = app.handle_input(&event) else {
        panic!("inactive {label}")
    };
    action
}

#[test]
fn overlay_colors_both_counts_and_highlights_only_the_control_under_the_pointer() {
    use crate::views::pull_request::Viewer;
    use ratatui::style::Modifier;
    let mut app = test_app_with_agent();
    let mut viewer = Viewer::loading();
    viewer.data = Some(snapshot());
    app.agents.get_mut(&AgentId(0)).unwrap().pull_request = Some(Box::new(viewer));
    let theme = crate::theme::Theme::current();
    let mut frame = agent_frame(&mut app);
    for (label, color) in [("+2", theme.accent_success), ("−0", theme.accent_error)] {
        let (x, y) = label_position(&frame, label);
        assert_eq!(frame[(x, y)].fg, color);
        assert_eq!(frame[(x + 1, y)].fg, color);
        assert_eq!(frame[(x - 1, y)].fg, theme.text_primary);
    }
    let mut previous = None;
    for label in ["[-]", "[x]", "[Fullscreen]", "[Refresh]"] {
        let pos = label_position(&frame, label);
        assert!(matches!(
            app.handle_input(&pointer_event(MouseEventKind::Moved, pos)),
            InputOutcome::Changed
        ));
        frame = agent_frame(&mut app);
        assert!(frame[pos].modifier.contains(Modifier::UNDERLINED));
        assert_eq!(frame[pos].bg, theme.bg_highlight);
        if let Some(old) = previous {
            assert_eq!(frame[old].bg, theme.bg_dark);
        }
        previous = Some(pos);
    }
    let exit = pointer_event(MouseEventKind::Moved, (0, 0));
    assert!(matches!(app.handle_input(&exit), InputOutcome::Changed));
    assert_eq!(agent_frame(&mut app)[previous.unwrap()].bg, theme.bg_dark);

    let minimize = click_label(&mut app, "[-]");
    dispatch(minimize, &mut app);
    let pos = label_position(&agent_frame(&mut app), "[+]");
    assert!(matches!(
        app.handle_input(&pointer_event(MouseEventKind::Moved, pos)),
        InputOutcome::Changed
    ));
    assert_eq!(agent_frame(&mut app)[pos].bg, theme.bg_highlight);
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

/// Purpose: Verify PR card controls in the dashboard session popup.
/// Input: Mouse clicks on a loaded PR card and a session draft.
/// Output: Each control changes the attached session and preserves its draft.
#[test]
fn dashboard_popup_routes_pr_controls_to_the_attached_session() {
    use crate::views::pull_request::{Viewer, summary::Display};
    use ratatui::layout::Rect;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let mut viewer = Viewer::loading();
    viewer.target = "8".into();
    viewer.data = Some(snapshot());
    let agent = app.agents.get_mut(&id).unwrap();
    agent.pull_request = Some(Box::new(viewer));
    agent.prompt.set_text("draft for main");
    app.active_view = ActiveView::AgentDashboard;
    ensure_dashboard_state(&mut app);
    let dashboard = app.dashboard.as_mut().unwrap();
    dashboard.attached_agent = Some(id);
    dashboard.popup_outer_rect = Some(Rect::new(0, 0, 132, 38));

    for (label, expected) in [
        ("[-]", Display::Chip),
        ("[+]", Display::Card),
        ("[x]", Display::Closed),
    ] {
        let action = click_label(&mut app, label);
        assert!(dispatch(action, &mut app).is_empty());
        assert!(
            app.agents[&id]
                .pull_request
                .as_ref()
                .unwrap()
                .summary
                .display
                == expected
        );
    }
    assert!(dispatch(Action::OpenPullRequest(String::new()), &mut app).is_empty());
    let action = click_label(&mut app, "[Refresh]");
    let effects = dispatch(action, &mut app);
    let [
        Effect::FetchPullRequest {
            agent_id,
            request_id,
            target,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("refresh must fetch the attached session's PR: {effects:?}");
    };
    assert_eq!(*agent_id, id);
    assert_eq!(target, "8");
    dispatch_task_result(
        TaskResult::PullRequestLoaded {
            agent_id: id,
            request_id: *request_id,
            result: Ok(snapshot()),
        },
        &mut app,
    );
    let action = click_label(&mut app, "[Fullscreen]");
    dispatch(action, &mut app);
    assert!(matches!(
        app.agents[&id].active_modal,
        Some(ActiveModal::PullRequest)
    ));
    assert!(matches!(app.active_view, ActiveView::AgentDashboard));
    assert_eq!(app.agents[&id].prompt.text(), "draft for main");
}

/// Purpose: Route dashboard review actions through the attached session.
/// Input: A digest or typed question, a cancel action, an answer, and a handoff draft.
/// Output: Each action reaches the session and preserves its main draft.
#[test]
fn dashboard_review_actions_reach_the_attached_session() {
    use crate::views::pull_request::summary::Control;
    use xai_grok_shell::extensions::review_chat::AskResponse;

    for is_digest in [false, true] {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let effects = dispatch(Action::OpenPullRequest("8".into()), &mut app);
        let [Effect::FetchPullRequest { request_id, .. }] = effects.as_slice() else {
            panic!("open must fetch the PR");
        };
        dispatch_task_result(
            TaskResult::PullRequestLoaded {
                agent_id: id,
                request_id: *request_id,
                result: Ok(snapshot()),
            },
            &mut app,
        );
        app.agents
            .get_mut(&id)
            .unwrap()
            .prompt
            .set_text("main draft");
        app.active_view = ActiveView::AgentDashboard;
        ensure_dashboard_state(&mut app);
        app.dashboard.as_mut().unwrap().attached_agent = Some(id);
        dispatch(Action::PullRequestControl(Control::Expand), &mut app);
        let key = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
        let outcome = if is_digest {
            app.handle_input(&key(KeyCode::Char('d'), KeyModifiers::NONE))
        } else {
            app.handle_input(&key(KeyCode::Char('?'), KeyModifiers::NONE));
            app.handle_input(&Event::Paste("Explain this PR".into()));
            app.handle_input(&key(KeyCode::Enter, KeyModifiers::NONE))
        };
        let InputOutcome::Action(Action::AskReviewChat(request)) = outcome else {
            panic!("review input must prepare a question");
        };
        let effects = dispatch(Action::AskReviewChat(request.clone()), &mut app);
        assert!(
            matches!(effects.as_slice(), [Effect::AskReviewChat { agent_id, request: sent }]
            if *agent_id == id && sent.child_id == request.child_id)
        );
        let chat = app.agents[&id]
            .pull_request
            .as_ref()
            .unwrap()
            .chat
            .clone()
            .unwrap();
        assert!(chat.borrow().busy);
        let InputOutcome::Action(cancel @ Action::KillSubagent(_)) =
            app.handle_input(&key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("Ctrl+C must prepare review cancellation");
        };
        let effects = dispatch(cancel, &mut app);
        assert!(
            matches!(effects.as_slice(), [Effect::KillSubagent { session_id, subagent_id }]
            if session_id.0.as_ref() == request.session_id && subagent_id == &request.child_id)
        );
        dispatch_task_result(
            TaskResult::ReviewChatAnswered {
                agent_id: id,
                parent_session_id: request.session_id,
                child_id: request.child_id.clone(),
                response: Ok(AskResponse {
                    child_id: request.child_id,
                    success: true,
                    output: "Review answer".into(),
                    error: None,
                }),
            },
            &mut app,
        );
        assert!(!chat.borrow().busy);
        app.handle_input(&key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        app.handle_input(&Event::Paste("Apply the reviewed fix".into()));
        let InputOutcome::Action(Action::ForwardReviewRequest(text)) =
            app.handle_input(&key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        else {
            panic!("handoff must prepare a forward request");
        };
        let effects = dispatch(Action::ForwardReviewRequest(text), &mut app);
        assert!(effects.iter().any(
            |effect| matches!(effect, Effect::SendPrompt { agent_id, text, .. }
            if *agent_id == id && text.contains("Apply the reviewed fix"))
        ));
        assert!(chat.borrow().draft.is_none());
        assert_eq!(app.agents[&id].prompt.text(), "main draft");
        assert!(matches!(app.active_view, ActiveView::AgentDashboard));
    }
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
