//! MCP server status for the Messages `system`/`init` line.
//!
//! One bounded `x.ai/mcp/list` read gives the real handshake state of each
//! server. The shell's startup grace runs later, on the prompt, so this read
//! must not add a second long wait.

use std::path::Path;
use std::time::Duration;

use agent_client_protocol as acp;
use xai_acp_lib::{AcpAgentTx, acp_send};
use xai_grok_shell::util::config as cli_config;

use crate::headless::reducer::McpServer;
use crate::views::mcps_modal::{McpsListResponse, McpsServerEntry};

/// Bound on the `x.ai/mcp/list` request, so a hung handler cannot stall `init`.
const MCP_LIST_TIMEOUT: Duration = Duration::from_secs(1);

/// Purpose: Give each configured MCP server its status for the `init` line.
/// Input: The agent connection, the session ID and the session directory.
/// Output: One entry per server. If the list request fails or times out,
///     every configured server shows as `pending`.
pub(crate) async fn resolve_mcp_servers_for_init(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
    cwd: &Path,
) -> Vec<McpServer> {
    match tokio::time::timeout(MCP_LIST_TIMEOUT, fetch_mcp_list(acp_tx, session_id)).await {
        Ok(Some(list)) => servers_from_list(list),
        Ok(None) => pending_from_config(cwd),
        Err(_) => {
            tracing::warn!("headless: mcp/list exceeded the init-status deadline");
            pending_from_config(cwd)
        }
    }
}

fn servers_from_list(list: McpsListResponse) -> Vec<McpServer> {
    list.servers
        .into_iter()
        .map(|entry| McpServer {
            status: init_wire_status(&entry).to_string(),
            name: entry.name,
        })
        .collect()
}

/// Messages status tokens. A server without a session status has not
/// started its handshake yet, so it is `pending`, not `failed`.
fn init_wire_status(entry: &McpsServerEntry) -> &'static str {
    let Some(session) = &entry.session else {
        return "pending";
    };
    if session.setup_required || session.auth_required {
        return "needs-auth";
    }
    if !session.enabled {
        return "disabled";
    }
    match session.status.as_deref() {
        Some("ready") => "connected",
        Some("initializing") | None => "pending",
        Some("setuprequired") => "needs-auth",
        Some(_) => "failed",
    }
}

fn pending_from_config(cwd: &Path) -> Vec<McpServer> {
    cli_config::load_mcp_servers(cwd, &xai_grok_tools::types::compat::CompatConfig::default())
        .iter()
        .filter_map(|server| {
            let name = match server {
                acp::McpServer::Http(http) => http.name.clone(),
                acp::McpServer::Sse(sse) => sse.name.clone(),
                acp::McpServer::Stdio(stdio) => stdio.name.clone(),
                _ => return None,
            };
            Some(McpServer {
                name,
                status: "pending".to_string(),
            })
        })
        .collect()
}

async fn fetch_mcp_list(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
) -> Option<McpsListResponse> {
    let params = serde_json::value::to_raw_value(&serde_json::json!({
        "sessionId": session_id.0.as_ref(),
        "cache": true,
    }))
    .ok()?;
    let request = acp::ExtRequest::new("x.ai/mcp/list", params.into());
    match acp_send(request, acp_tx).await {
        Ok(response) => decode_mcp_list(response.0.get()),
        Err(e) => {
            tracing::warn!(error = %e, "headless: mcp/list failed");
            None
        }
    }
}

fn decode_mcp_list(raw: &str) -> Option<McpsListResponse> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let payload = value.get("result").unwrap_or(&value);
    match serde_json::from_value(payload.clone()) {
        Ok(list) => Some(list),
        Err(e) => {
            tracing::warn!(error = %e, "headless: mcp/list payload did not match McpsListResponse");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statuses(raw: serde_json::Value) -> Vec<(String, String)> {
        let list = decode_mcp_list(&raw.to_string()).expect("list decodes");
        servers_from_list(list)
            .into_iter()
            .map(|s| (s.name, s.status))
            .collect()
    }

    #[test]
    fn init_status_follows_the_handshake_state() {
        let raw = serde_json::json!({"servers": [
            {"name": "ok", "session": {"enabled": true, "status": "ready"}},
            {"name": "slow", "session": {"enabled": true, "status": "initializing"}},
            {"name": "down", "session": {"enabled": true, "status": "unavailable"}},
            {"name": "login", "session": {"enabled": true, "status": "unavailable", "authRequired": true}},
            {"name": "off", "session": {"enabled": false}},
            {"name": "unstarted", "session": {"enabled": true}},
            {"name": "catalog-only"},
        ]});
        assert_eq!(
            statuses(raw),
            [
                ("ok", "connected"),
                ("slow", "pending"),
                ("down", "failed"),
                ("login", "needs-auth"),
                ("off", "disabled"),
                ("unstarted", "pending"),
                ("catalog-only", "pending"),
            ]
            .map(|(n, s)| (n.to_string(), s.to_string()))
        );
    }

    #[test]
    fn list_payload_may_be_wrapped_in_result() {
        let raw = serde_json::json!({"result": {"servers": [
            {"name": "ok", "session": {"enabled": true, "status": "ready"}},
        ]}});
        assert_eq!(statuses(raw), [("ok".to_string(), "connected".to_string())]);
    }
}
