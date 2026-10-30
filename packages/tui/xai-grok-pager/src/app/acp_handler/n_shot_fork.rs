//! `x.ai/n_shot/fork`: the shell asks the pager to run an `n_shot` tool call as
//! session shots.
//!
//! The pager accepts at once and answers with the group label. The shots fork
//! when the parent's turn ends (see the n-shot tick), so no shot copies a
//! half-finished turn. An explicit decline lets the shell use subagent shots.
//! The pager queues shots only after it sends the acceptance reply.

use agent_client_protocol as acp;

use super::routing::{SessionMatch, find_session_match};
use crate::app::app_view::AppView;
use crate::app::n_shot::{NShotRequest, next_group_label, parse_request_shots};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NShotForkParams {
    session_id: String,
    args: serde_json::Value,
}

/// Purpose: Accept or decline an `n_shot` tool call as session shots.
/// Input: The ext request and the app.
/// Output: True when the screen changed. The answer goes to the shell.
pub(super) fn handle_n_shot_fork(
    ext: xai_acp_lib::AcpArgs<acp::ExtRequest>,
    app: &mut AppView,
) -> bool {
    let request = match prepare_request(app, ext.request.params.get()) {
        Ok(request) => request,
        Err(detail) => {
            let _ = ext.response_tx.send(Err(acp::Error::new(-32602, detail)));
            return false;
        }
    };
    let body = serde_json::json!({ "label": request.label });
    let response = acp::ExtResponse::new(
        serde_json::value::to_raw_value(&body)
            .expect("serialize n-shot fork response")
            .into(),
    );
    if ext.response_tx.send(Ok(response)).is_err() {
        return false;
    }
    app.n_shot_requests.push(request);
    true
}

/// Purpose: Validate the fork request without a change to the app.
/// Input: The app and the raw request params.
/// Output: A request to queue after acceptance, or an explicit decline.
fn prepare_request(app: &AppView, params: &str) -> Result<NShotRequest, String> {
    let params: NShotForkParams =
        serde_json::from_str(params).map_err(|e| format!("invalid n-shot fork params: {e}"))?;
    let sid = acp::SessionId::new(params.session_id);
    let parent = match find_session_match(app, &sid) {
        Some(SessionMatch::Root(id)) => id,
        _ => return Err("no top-level session view for this session".into()),
    };
    let agent = app.agents.get(&parent).ok_or("session view is gone")?;
    let in_git_repo = agent.current_branch.is_some()
        || crate::git_info::cwd_git_info_lazy(&agent.session.cwd).is_some();
    if !in_git_repo {
        return Err("session shots need a git repository".into());
    }
    let shots = parse_request_shots(&params.args)?;
    let label = next_group_label(&app.n_shot_groups, &app.n_shot_requests, parent);
    Ok(NShotRequest {
        parent,
        label,
        shots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::acp_handler::tests::make_app_with_agent;
    use crate::app::agent::AgentId;

    fn fork_request(
        args: serde_json::Value,
    ) -> (
        xai_acp_lib::AcpArgs<acp::ExtRequest>,
        xai_acp_lib::AcpRxo<acp::ExtResponse>,
    ) {
        let params = serde_json::json!({ "sessionId": "session-1", "args": args });
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let ext = xai_acp_lib::AcpArgs {
            request: acp::ExtRequest::new(
                "x.ai/n_shot/fork",
                serde_json::value::to_raw_value(&params).unwrap().into(),
            ),
            response_tx,
        };
        (ext, response_rx)
    }

    fn app_in_git_repo() -> AppView {
        let mut app = make_app_with_agent("session-1");
        app.agents.get_mut(&AgentId(0)).unwrap().current_branch = Some("main".into());
        app
    }

    #[test]
    fn failed_acceptance_reply_does_not_queue_shots_or_reserve_label() {
        let mut app = app_in_git_repo();
        let args = serde_json::json!({ "prompt": "fix it", "n": 2 });
        let (ext, response_rx) = fork_request(args.clone());
        drop(response_rx);

        assert!(!handle_n_shot_fork(ext, &mut app));
        assert!(app.n_shot_requests.is_empty());
        assert!(app.n_shot_groups.is_empty());

        let (ext, mut response_rx) = fork_request(args);
        assert!(handle_n_shot_fork(ext, &mut app));
        let response = response_rx.try_recv().unwrap().unwrap();
        let body: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
        assert_eq!(body["label"], "n-shot-1");
        assert_eq!(app.n_shot_requests.len(), 1);
    }

    #[test]
    fn successful_acceptance_queues_one_panel_with_the_returned_label() {
        let mut app = app_in_git_repo();
        let (ext, mut response_rx) =
            fork_request(serde_json::json!({ "prompt": "fix it", "n": 2 }));

        assert!(handle_n_shot_fork(ext, &mut app));
        let response = response_rx.try_recv().unwrap().unwrap();
        let body: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
        assert_eq!(app.n_shot_requests.len(), 1);
        let request = &app.n_shot_requests[0];
        assert_eq!(body["label"], request.label);
        assert_eq!(request.parent, AgentId(0));
        assert_eq!(request.shots.len(), 2);
    }

    #[test]
    fn explicit_decline_leaves_no_queued_panel() {
        let mut app = app_in_git_repo();
        let (ext, mut response_rx) =
            fork_request(serde_json::json!({ "prompt": "fix it", "n": 0 }));

        assert!(!handle_n_shot_fork(ext, &mut app));
        let error = response_rx.try_recv().unwrap().unwrap_err();
        assert_eq!(error.code, acp::ErrorCode::InvalidParams);
        assert!(app.n_shot_requests.is_empty());
        assert!(app.n_shot_groups.is_empty());
    }
}
