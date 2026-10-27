//! Purpose: Control the built-in Cua MCP connection from a slash command.
//! Input: A user command and the installed Cua Driver executable.
//! Output: A live MCP connection or a status message.

use super::*;
use crate::session::mcp_servers::McpClient;
use crate::util::config::{McpServerConfig, McpServerTransportConfig};
use std::path::PathBuf;

const SERVER_NAME: &str = "computer-use";
const USAGE: &str = "Usage: /computer-use [on|off|status] (alias: /cua).";
const CAPTURE_CONSENT_ALERT_ENV: &str = "CUA_DRIVER_ALLOW_CAPTURE_CONSENT_ALERT";
const DRIVER_ARGS: &[&str] = if cfg!(target_os = "macos") {
    &["mcp", "--embedded"]
} else {
    &["mcp", "--direct", "--embedded"]
};

impl SessionActor {
    /// Purpose: Apply a computer-use command without a model request.
    /// Input: An empty argument, on, off, or status.
    /// Output: The actual connection state or an error.
    /// Note: The saved MCP definition preserves the user's choice across sessions.
    pub(super) async fn configure_computer_use(
        self: &Arc<Self>,
        args: &str,
    ) -> Result<String, String> {
        let is_enabled = self
            .mcp_state
            .lock()
            .await
            .configs
            .iter()
            .any(|server| mcp_server_name(server) == SERVER_NAME);
        let enabled = match args.trim().to_ascii_lowercase().as_str() {
            "" => !is_enabled,
            "on" => true,
            "off" => false,
            "status" => return Ok(self.computer_use_status().await),
            _ => return Err(USAGE.to_string()),
        };
        if !enabled {
            self.set_mcp_server_enabled(SERVER_NAME, false, None)
                .await
                .map_err(|error| error.to_string())?;
            return Ok("Computer use is off. Run /computer-use to enable it.".to_string());
        }

        let path = find_driver()?;
        let server = driver_server(&path);
        let policy = xai_grok_workspace::permission::resolution::managed_settings();
        if !policy.mcp_allowlist.is_server_allowed(&server) {
            return Err("Cua Driver is blocked by your organization's MCP policy.".to_string());
        }
        crate::util::config::save_mcp_server_config(SERVER_NAME, &driver_config(&path))
            .await
            .map_err(|error| format!("Could not save the Cua configuration: {error}"))?;
        self.set_mcp_server_enabled(SERVER_NAME, true, Some(server))
            .await
            .map_err(|error| error.to_string())?;

        if let Some(client) = self.wait_for_computer_use_ready().await
            && client.ensure_initialized().await.is_ok()
        {
            return Ok("Computer use is on. Cua uses the host application's permissions. Run /computer-use off to stop it.".to_string());
        }
        self.set_mcp_server_enabled(SERVER_NAME, false, None)
            .await
            .map_err(|error| error.to_string())?;
        Err(
            "Cua Driver could not connect. Computer use remains off; check /mcps for diagnostics."
                .to_string(),
        )
    }

    /// Purpose: Wait for the transport and tool registration result of computer use.
    /// Input: The live MCP state and per-server completion notifications.
    /// Output: A connected client only when its tool registration succeeds.
    async fn wait_for_computer_use_ready(&self) -> Option<Arc<McpClient>> {
        tokio::time::timeout(Duration::from_secs(35), async {
            loop {
                let notified = self.mcp_handshakes_done.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let state = self.mcp_state.lock().await;
                let is_starting = state
                    .handshaking_servers_iter()
                    .any(|name| name == SERVER_NAME);
                if !is_starting {
                    if state.init_failed.contains_key(SERVER_NAME)
                        || state.auth_required.contains(SERVER_NAME)
                    {
                        return None;
                    }
                    return state.get_client(SERVER_NAME).cloned();
                }
                drop(state);
                notified.await;
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Purpose: Report the connection state without a tool call or permission prompt.
    /// Input: The live MCP state.
    /// Output: A status message for the user.
    async fn computer_use_status(&self) -> String {
        let state = self.mcp_state.lock().await;
        if !state
            .configs
            .iter()
            .any(|server| mcp_server_name(server) == SERVER_NAME)
        {
            return "Computer use is off. Run /computer-use to enable it.".to_string();
        }
        if state
            .handshaking_servers_iter()
            .any(|name| name == SERVER_NAME)
        {
            return "Computer use is connecting. Check /mcps for its status.".to_string();
        }
        let client = if state.init_failed.contains_key(SERVER_NAME)
            || state.auth_required.contains(SERVER_NAME)
        {
            None
        } else {
            state.get_client(SERVER_NAME).cloned()
        };
        drop(state);
        if let Some(client) = client
            && client.state_kind().await == xai_grok_mcp::servers::ClientStateKind::Ready
        {
            return "Computer use is on. Run /computer-use off to stop it.".to_string();
        }
        "Computer use is unavailable. Run /computer-use on to retry, or check /mcps.".to_string()
    }

    /// Purpose: Change a live MCP server and refresh the tool catalog.
    /// Input: A server name, the requested state, and an optional server definition.
    /// Output: The state change result. The user preference is saved.
    /// Note: The toggle task remains active if the caller cancels its wait.
    pub(super) async fn set_mcp_server_enabled(
        self: &Arc<Self>,
        server_name: &str,
        enabled: bool,
        server_config: Option<acp::McpServer>,
    ) -> Result<(), acp::Error> {
        let session = Arc::clone(self);
        let server_name = server_name.to_string();
        tokio::task::spawn_local(async move {
            session
                .apply_mcp_server_enabled(&server_name, enabled, server_config)
                .await
        })
        .await
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?
    }

    /// Purpose: Complete a server toggle outside the caller's cancellation scope.
    /// Input: The server name, requested state, and optional definition.
    /// Output: Updated clients, tools, and saved preference, or an error.
    async fn apply_mcp_server_enabled(
        self: &Arc<Self>,
        server_name: &str,
        enabled: bool,
        server_config: Option<acp::McpServer>,
    ) -> Result<(), acp::Error> {
        let (diff, event_tx, previous_client) = {
            let mut state = self.mcp_state.lock().await;
            let previous_client = state.get_client(server_name).cloned();
            let diff = update_enabled_server(&mut state, server_name, enabled, server_config)?;
            (diff, state.client_event_tx(), previous_client)
        };
        self.events
            .emit(xai_file_utils::events::Event::McpServerToggled {
                server_name: server_name.to_string(),
                enabled,
            });
        if let Some(diff) = diff {
            if diff.removed.iter().any(|name| name == server_name)
                && let Some(client) = previous_client
            {
                client.shutdown().await;
            }
            if (!diff.added.is_empty() || !diff.removed.is_empty())
                && let Some(tx) = event_tx
            {
                let _ = tx.send(xai_grok_mcp::servers::McpClientEvent::ConfigDiff {
                    added: diff.added,
                    removed: diff.removed.clone(),
                });
            }
            for name in diff.removed {
                self.unregister_server_tools(&name);
            }
        }
        self.ensure_mcp_tools_initialized().await;
        self.refresh_mcp_snapshot_and_schedule_reminder().await;
        if !enabled {
            self.emit_mcp_tools_changed_notifications(HashMap::from([(
                server_name.to_string(),
                Vec::new(),
            )]));
        }
        let cwd = Path::new(&self.session_info.cwd);
        crate::util::config::save_mcp_server_enabled_in(server_name, enabled, cwd)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        Ok(())
    }
}

/// Purpose: Apply a server toggle and permit a new attempt after a failed init.
/// Input: The MCP state, server name, requested state, and optional definition.
/// Output: A config diff or an error for an unknown server.
fn update_enabled_server(
    state: &mut McpState,
    server_name: &str,
    enabled: bool,
    server_config: Option<acp::McpServer>,
) -> Result<Option<crate::session::mcp_servers::McpConfigDiff>, acp::Error> {
    let mut configs = state.configs.clone();
    if enabled {
        if let Some(config) = server_config {
            configs.retain(|server| mcp_server_name(server) != server_name);
            configs.push(config);
        } else if !configs
            .iter()
            .any(|server| mcp_server_name(server) == server_name)
        {
            return Err(acp::Error::invalid_params()
                .data(format!("server '{server_name}' not found in config")));
        }
    } else {
        configs.retain(|server| mcp_server_name(server) != server_name);
    }
    let mut diff = state.update_configs_diff(configs.clone());
    if diff.is_none()
        && enabled
        && !state.is_server_handshaking(server_name)
        && (state.get_client(server_name).is_none() || state.init_failed.contains_key(server_name))
    {
        // An identical definition does not reset a completed failed init.
        // Evict only this server, then let the normal initializer retry it.
        let mut without_server = configs.clone();
        without_server.retain(|server| mcp_server_name(server) != server_name);
        state.update_configs_diff(without_server);
        diff = state.update_configs_diff(configs);
        if let Some(diff) = &mut diff {
            diff.removed.push(server_name.to_string());
        }
    }
    Ok(diff)
}

fn driver_server(path: &Path) -> acp::McpServer {
    acp::McpServer::Stdio(
        acp::McpServerStdio::new(SERVER_NAME, path.to_path_buf())
            .args(DRIVER_ARGS.iter().map(|arg| (*arg).to_string()).collect())
            .env(if cfg!(target_os = "macos") {
                vec![acp::EnvVariable::new(CAPTURE_CONSENT_ALERT_ENV, "1")]
            } else {
                Vec::new()
            }),
    )
}

fn driver_config(path: &Path) -> McpServerConfig {
    McpServerConfig {
        transport: McpServerTransportConfig::Stdio {
            command: path.to_string_lossy().into_owned(),
            args: DRIVER_ARGS.iter().map(|arg| (*arg).to_string()).collect(),
            // The user opts in through the slash command. Let macOS show its
            // native consent instead of checking the standalone app's record.
            // This does not grant or bypass the host's macOS permissions.
            env: cfg!(target_os = "macos")
                .then(|| HashMap::from([(CAPTURE_CONSENT_ALERT_ENV.to_string(), "1".to_string())])),
            cwd: None,
        },
        enabled: true,
        oauth: None,
        setup: None,
        startup_timeout_sec: Some(30),
        tool_timeout_sec: None,
        tool_timeouts: None,
        expose_image_base64: None,
    }
}

/// Purpose: Find the installed driver without a shell or an automatic download.
/// Input: The executable search path and the user's home directory.
/// Output: The driver path or an installation hint.
fn find_driver() -> Result<PathBuf, String> {
    let executable = if cfg!(windows) {
        "cua-driver.exe"
    } else {
        "cua-driver"
    };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join(executable))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin").join(executable));
    }
    #[cfg(target_os = "macos")]
    candidates.push(PathBuf::from(
        "/Applications/CuaDriver.app/Contents/MacOS/cua-driver",
    ));
    candidates.into_iter().find(|path| is_executable(path)).ok_or_else(|| {
        "Cua Driver is not installed. Install it from https://cua.ai/docs/cua-driver/quickstart, then run /computer-use again.".to_string()
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

#[cfg(all(test, unix))]
#[path = "computer_use_tests.rs"]
mod tests;
