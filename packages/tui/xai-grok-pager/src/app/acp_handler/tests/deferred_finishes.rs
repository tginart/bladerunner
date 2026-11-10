//! A `SubagentFinished` that arrives before its `SubagentSpawned` is held and
//! applied once the spawn arrives, so the row does not stay "running".

use super::*;

/// An `x.ai/session_notification` envelope whose `_meta.eventId` carries
/// `event_seq`, so the dedup highwater applies to it.
fn notification_with_seq(
    session_id: &str,
    update: XaiSessionUpdate,
    event_seq: u64,
) -> AcpClientMessage {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let payload = SessionNotification {
        session_id: acp::SessionId::new(session_id),
        update,
        meta: Some(serde_json::json!({ "eventId": format!("{session_id}-{event_seq}") })),
    };
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
        request: acp::ExtNotification::new("x.ai/session_notification", raw.into()),
        response_tx: tx,
    })
}

fn assert_child_finished(app: &AppView, child_sid: &str) {
    let finish = snapshot_after_subagent_finish(app, child_sid);
    assert!(
        finish.finished,
        "the held finish must apply after the spawn"
    );
    assert_eq!(finish.status.as_deref(), Some("completed"));
    assert!(matches!(
        finish.block_kind,
        SubagentBlockKind::Completed { .. }
    ));
    assert_eq!(app.agents[&AgentId(0)].deferred_subagent_finishes.len(), 0);
}

#[test]
fn finish_before_spawn_is_applied_when_the_spawn_arrives() {
    let mut app = make_app_with_agent("sess-parent");
    let child_sid = "child-early-finish";

    let _ = handle(
        make_ext_session_notification("sess-parent", test_subagent_finished(child_sid)),
        &mut app,
    );
    let agent = &app.agents[&AgentId(0)];
    assert!(!agent.subagent_sessions.contains_key(child_sid));
    assert_eq!(agent.deferred_subagent_finishes.len(), 1);

    let _ = handle(
        make_ext_session_notification(
            "sess-parent",
            test_subagent_spawned("sess-parent", child_sid),
        ),
        &mut app,
    );

    assert_child_finished(&app, child_sid);
}

/// The shell can emit the finish before the spawn (the spawn waits on a busy
/// parent), so the finish has the lower event seq. Replayed as is, the
/// dedup highwater from the spawn would drop it.
#[test]
fn finish_with_a_lower_event_seq_still_applies_after_the_spawn() {
    let mut app = make_app_with_agent("sess-parent");
    let child_sid = "child-low-seq";

    let _ = handle(
        notification_with_seq("sess-parent", test_subagent_finished(child_sid), 5),
        &mut app,
    );
    let _ = handle(
        notification_with_seq(
            "sess-parent",
            test_subagent_spawned("sess-parent", child_sid),
            6,
        ),
        &mut app,
    );

    assert_child_finished(&app, child_sid);
    assert_eq!(
        app.agents[&AgentId(0)].last_applied_xai_event_seq,
        Some(6),
        "the replayed finish must not move the highwater back"
    );
}

/// The finish arm does not read `output`, and the output has no size limit,
/// so a held finish does not keep it.
#[test]
fn held_finish_drops_the_child_output() {
    let mut app = make_app_with_agent("sess-parent");
    let child_sid = "child-big-output";
    let mut finished = test_subagent_finished(child_sid);
    let XaiSessionUpdate::SubagentFinished { output, .. } = &mut finished else {
        panic!("test_subagent_finished must build a SubagentFinished");
    };
    *output = Some("x".repeat(1 << 20));

    let _ = handle(
        make_ext_session_notification("sess-parent", finished),
        &mut app,
    );

    let held = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .deferred_subagent_finishes
        .take(child_sid, std::time::Instant::now())
        .expect("the finish must be held");
    let parsed: SessionNotification = serde_json::from_str(held.params.get()).unwrap();
    assert!(matches!(
        parsed.update,
        XaiSessionUpdate::SubagentFinished { output: None, .. }
    ));
    assert!(held.params.get().len() < 64 * 1024);
}

/// A finish whose spawn arrives in order still applies at once.
#[test]
fn finish_after_spawn_is_not_deferred() {
    let (_, finish) =
        run_subagent_lifecycle_via_method("x.ai/session_notification", "child-in-order");
    assert!(finish.finished);
}
