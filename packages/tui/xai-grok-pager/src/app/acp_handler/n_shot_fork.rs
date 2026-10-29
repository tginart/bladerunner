//! `x.ai/n_shot/fork`: the shell asks the pager to run an `n_shot` tool call as
//! session shots.
//!
//! The pager accepts at once and answers with the group label. The shots fork
//! when the parent's turn ends (see the n-shot tick), so no shot copies a
//! half-finished turn. An error answer makes the shell use subagent shots.

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
    let reply = accept(app, ext.request.params.get());
    let response = reply.map(|label| {
        let body = serde_json::json!({ "label": label });
        acp::ExtResponse::new(
            serde_json::value::to_raw_value(&body)
                .expect("serialize n-shot fork response")
                .into(),
        )
    });
    let changed = response.is_ok();
    ext.response_tx
        .send(response.map_err(|detail| acp::Error::new(-32602, detail)))
        .ok();
    changed
}

fn accept(app: &mut AppView, params: &str) -> Result<String, String> {
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
    app.n_shot_requests.push(NShotRequest {
        parent,
        label: label.clone(),
        shots,
    });
    Ok(label)
}
