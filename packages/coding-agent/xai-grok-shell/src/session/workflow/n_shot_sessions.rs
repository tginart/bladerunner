//! Ask the client to run an `n_shot` tool call as session shots.
//!
//! Session shots are full sessions that the client forks from this one, each
//! in its own worktree. Only the client can create live sessions, so the shell
//! sends `x.ai/n_shot/fork` and falls back to subagent shots when the client
//! does not answer it (headless mode, other ACP clients).

use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::Client as _;

const N_SHOT_FORK_METHOD: &str = "x.ai/n_shot/fork";
const N_SHOT_FORK_TIMEOUT: Duration = Duration::from_secs(10);

/// Purpose: Ask the client to fork this session into n-shot sessions.
/// Input: The gateway, this session id, and the `n-shot` workflow args.
/// Output: The group label when the client accepted, or `None` to fall back.
pub(crate) async fn request_session_shots(
    gateway: &xai_acp_lib::AcpAgentGatewaySender,
    session_id: &acp::SessionId,
    args: &serde_json::Value,
) -> Option<String> {
    let params = serde_json::json!({ "sessionId": session_id.0, "args": args });
    let raw = serde_json::value::to_raw_value(&params).ok()?;
    let request = acp::ExtRequest::new(N_SHOT_FORK_METHOD, raw.into());
    let response = match tokio::time::timeout(N_SHOT_FORK_TIMEOUT, gateway.ext_method(request))
        .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "client declined n-shot sessions; using subagent shots");
            return None;
        }
        Err(_) => {
            tracing::warn!("client did not answer n-shot fork in time; using subagent shots");
            return None;
        }
    };
    let body: serde_json::Value = serde_json::from_str(response.0.get()).ok()?;
    body.get("label")?.as_str().map(String::from)
}
