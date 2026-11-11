//! `ResponseCompleted` speed fields feed `AgentView::inference_speed`.
use super::*;
use crate::views::agent_status::InferenceSpeed;

fn response_completed(ttft_ms: Option<u64>, tokens_per_sec: Option<f64>) -> XaiSessionUpdate {
    XaiSessionUpdate::ResponseCompleted {
        message_id: None,
        stop_reason: None,
        usage: None,
        signature: None,
        stop_sequence: None,
        ttft_ms,
        tokens_per_sec,
    }
}

#[test]
fn response_completed_records_speed_and_redraws_when_shown() {
    crate::appearance::cache::set_show_inference_speed(true);
    let mut app = make_app_with_agent("sess-A");
    let msg = make_ext_session_notification("sess-A", response_completed(Some(420), Some(55.5)));
    assert!(handle(msg, &mut app), "chip on: new speed must redraw");
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert_eq!(
        agent.inference_speed,
        InferenceSpeed::new(Some(420), Some(55.5))
    );
    crate::appearance::cache::set_show_inference_speed(false);
}

#[test]
fn response_completed_records_speed_without_redraw_when_hidden() {
    crate::appearance::cache::set_show_inference_speed(false);
    let mut app = make_app_with_agent("sess-A");
    let msg = make_ext_session_notification("sess-A", response_completed(Some(90), Some(12.0)));
    assert!(!handle(msg, &mut app), "chip off: nothing visible changed");
    // Kept, so turning the setting on shows the latest value at once.
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.inference_speed.is_some());
}

#[test]
fn response_completed_from_older_shell_has_no_speed() {
    // An older shell sends ResponseCompleted without the speed fields.
    let mut app = make_app_with_agent("sess-A");
    let raw_payload = serde_json::json!({
        "sessionId": "sess-A",
        "update": { "sessionUpdate": "response_completed", "stop_reason": "end_turn" }
    });
    let raw = serde_json::value::to_raw_value(&raw_payload).unwrap();
    let request = acp::ExtNotification::new("x.ai/session_notification", raw.into());
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let msg = AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
        request,
        response_tx: tx,
    });
    handle(msg, &mut app);
    assert_eq!(app.agents.get(&AgentId(0)).unwrap().inference_speed, None);
}

#[test]
fn replayed_response_completed_is_not_a_live_measurement() {
    crate::appearance::cache::set_show_inference_speed(true);
    let mut app = make_app_with_agent("sess-A");
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .session
        .loading_replay = true;
    let msg = make_ext_session_notification("sess-A", response_completed(Some(420), Some(55.5)));
    assert!(!handle(msg, &mut app));
    assert_eq!(app.agents.get(&AgentId(0)).unwrap().inference_speed, None);
    crate::appearance::cache::set_show_inference_speed(false);
}
