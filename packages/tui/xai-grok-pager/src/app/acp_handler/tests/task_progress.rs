//! `x.ai/task_progress` sets the progress bar value of a tasks-pane row.
use super::*;
use crate::app::task_progress::TaskProgressValue;

fn progress_notif(
    session_id: &str,
    task_id: Option<&str>,
    completed: u64,
    total: u64,
) -> acp::ExtNotification {
    let payload = TaskProgressNotification {
        session_id: acp::SessionId::new(session_id),
        task_id: task_id.map(str::to_string),
        completed,
        total,
    };
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    acp::ExtNotification::new(TASK_PROGRESS_METHOD, std::sync::Arc::from(raw))
}

#[test]
fn root_progress_is_keyed_by_task_id() {
    let mut app = make_app_with_parent_and_child("parent-sess", "child-sess");
    let notif = progress_notif("parent-sess", Some("task-1"), 3, 8);
    assert!(handle_task_progress(&notif, &mut app));

    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert_eq!(
        agent.task_progress.get("task-1"),
        TaskProgressValue::new(3, 8).as_ref()
    );
    let child = agent.subagent_views.get("child-sess").unwrap();
    assert!(child.task_progress.is_empty());
}

#[test]
fn progress_can_arrive_before_the_row_exists() {
    // The spawn tool sends the first value before `SubagentSpawned`.
    let mut app = make_app_with_agent("parent-sess");
    let notif = progress_notif("parent-sess", Some("sa-not-yet-spawned"), 0, 4);
    assert!(handle_task_progress(&notif, &mut app));
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.task_progress.contains_key("sa-not-yet-spawned"));
}

#[test]
fn subagent_self_report_lands_on_its_row_in_the_parent() {
    let mut app = make_app_with_parent_and_child("parent-sess", "child-sess");
    let notif = progress_notif("child-sess", None, 40, 100);
    assert!(handle_task_progress(&notif, &mut app));

    let agent = app.agents.get(&AgentId(0)).unwrap();
    // `make_subagent_info` gives the subagent the ID `sa-<child session>`.
    assert_eq!(
        agent.task_progress.get("sa-child-sess"),
        TaskProgressValue::new(40, 100).as_ref()
    );
}

#[test]
fn subagent_progress_for_its_own_task_stays_in_the_child_view() {
    let mut app = make_app_with_parent_and_child("parent-sess", "child-sess");
    let notif = progress_notif("child-sess", Some("task-child"), 1, 2);
    assert!(handle_task_progress(&notif, &mut app));

    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.task_progress.is_empty());
    let child = agent.subagent_views.get("child-sess").unwrap();
    assert!(child.task_progress.contains_key("task-child"));
}

#[test]
fn later_value_replaces_the_earlier_value() {
    let mut app = make_app_with_agent("parent-sess");
    handle_task_progress(&progress_notif("parent-sess", Some("t"), 1, 8), &mut app);
    handle_task_progress(&progress_notif("parent-sess", Some("t"), 5, 8), &mut app);
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert_eq!(
        agent.task_progress.get("t"),
        TaskProgressValue::new(5, 8).as_ref()
    );
}

#[test]
fn invalid_progress_is_ignored() {
    let mut app = make_app_with_parent_and_child("parent-sess", "child-sess");
    // A zero total has no bar.
    assert!(!handle_task_progress(
        &progress_notif("parent-sess", Some("t"), 1, 0),
        &mut app
    ));
    // Only a subagent can omit the task ID.
    assert!(!handle_task_progress(
        &progress_notif("parent-sess", None, 1, 2),
        &mut app
    ));
    // An unknown session has no view.
    assert!(!handle_task_progress(
        &progress_notif("unknown-sess", Some("t"), 1, 2),
        &mut app
    ));
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.task_progress.is_empty());
}

#[test]
fn ext_notification_method_routes_to_the_handler() {
    let mut app = make_app_with_agent("parent-sess");
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let msg = AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
        request: progress_notif("parent-sess", Some("t"), 2, 4),
        response_tx: tx,
    });
    assert!(handle(msg, &mut app));
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.task_progress.contains_key("t"));
}
