//! Purpose: Control the built-in Cua MCP connection from a slash command.
//! Input: A user command and the installed Cua Driver executable.
//! Output: A live MCP connection or a status message.
//! Note: `/computer-use install` runs the official Cua Driver installer first.

use super::*;
use crate::session::mcp_servers::McpClient;
use crate::util::config::{McpServerConfig, McpServerTransportConfig};
use std::path::PathBuf;

const SERVER_NAME: &str = "computer-use";
const USAGE: &str = "Usage: /computer-use [on|off|status|install] (alias: /cua).";
const QUICKSTART_URL: &str = "https://cua.ai/docs/cua-driver/quickstart";
/// The TUI runs this command when the user clicks the link in the
/// not-installed message. Keep it in sync with the pager's link allowlist.
const INSTALL_COMMAND: &str = "/computer-use install";
// The driver-only installer. It installs to ~/.local/bin, which
// `find_driver` checks, so there is no need to edit the shell profile.
const INSTALL_SCRIPT: &str =
    "set -o pipefail; curl -fsSL https://cua.ai/driver/install.sh | bash -s -- --no-modify-path";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const INSTALL_ERROR_LINES: usize = 8;
const CAPTURE_CONSENT_ALERT_ENV: &str = "CUA_DRIVER_ALLOW_CAPTURE_CONSENT_ALERT";
const DRIVER_ARGS: &[&str] = if cfg!(target_os = "macos") {
    &["mcp", "--embedded"]
} else {
    &["mcp", "--direct", "--embedded"]
};

impl SessionActor {
    /// Purpose: Apply a computer-use command without a model request.
    /// Input: An empty argument, on, off, status, or install.
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
            "install" => return self.install_computer_use().await,
            _ => return Err(USAGE.to_string()),
        };
        if !enabled {
            self.set_mcp_server_enabled(SERVER_NAME, false, None)
                .await
                .map_err(|error| error.to_string())?;
            return Ok("Computer use is off. Run /computer-use to enable it.".to_string());
        }

        let path = find_driver().ok_or_else(not_installed_message)?;
        self.enable_computer_use(&path).await
    }

    /// Purpose: Install Cua Driver when it is missing, then turn computer use on.
    /// Input: The installed driver, or the network for the official installer.
    /// Output: The connection state, or an error with the installer's last output.
    async fn install_computer_use(self: &Arc<Self>) -> Result<String, String> {
        if let Some(path) = find_driver() {
            return self.enable_computer_use(&path).await;
        }
        if cfg!(windows) {
            return Err(format!(
                "Automatic installation is not available on Windows. Install Cua Driver from {QUICKSTART_URL}, then run /computer-use again."
            ));
        }
        if let Some(home) = dirs::home_dir()
            && !is_server_allowed(&driver_server(&home.join(".local/bin/cua-driver")))
        {
            return Err(POLICY_BLOCKED.to_string());
        }
        self.send_host_turn_slash_command_output(
            "Installing Cua Driver from cua.ai. This can take a minute…\n\n",
        )
        .await;
        run_installer().await?;
        let path = find_driver().ok_or_else(|| {
            "The installer finished, but Cua Driver was not found. Run /computer-use install again, or check ~/.local/bin.".to_string()
        })?;
        let message = self.enable_computer_use(&path).await?;
        Ok(format!("Cua Driver is installed. {message}"))
    }

    /// Purpose: Save and connect the computer-use MCP server for a known driver.
    /// Input: The driver executable path.
    /// Output: The connection state or an error. A failed connection stays off.
    async fn enable_computer_use(self: &Arc<Self>, path: &Path) -> Result<String, String> {
        let server = driver_server(path);
        if !is_server_allowed(&server) {
            return Err(POLICY_BLOCKED.to_string());
        }
        crate::util::config::save_mcp_server_config(SERVER_NAME, &driver_config(path))
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

const POLICY_BLOCKED: &str = "Cua Driver is blocked by your organization's MCP policy.";

fn is_server_allowed(server: &acp::McpServer) -> bool {
    xai_grok_workspace::permission::resolution::managed_settings()
        .mcp_allowlist
        .is_server_allowed(server)
}

/// Purpose: Tell the user that the driver is missing and how to get it.
/// Output: A message. On macOS and Linux, it has a link that runs the installer.
/// Note: The angle brackets let the markdown link destination hold a space.
fn not_installed_message() -> String {
    if cfg!(windows) {
        return format!(
            "Cua Driver is not installed. Install it from {QUICKSTART_URL}, then run /computer-use again."
        );
    }
    format!(
        "Cua Driver is not installed. [Click here to install it](<{INSTALL_COMMAND}>), or see {QUICKSTART_URL}."
    )
}

/// Purpose: Run the official Cua Driver installer without a terminal.
/// Input: The network and the user's home directory.
/// Output: Ok when the installer exits with status 0, or an error with its last lines.
/// Note: stdin is null, so the installer cannot prompt or read the TUI's input.
async fn run_installer() -> Result<(), String> {
    let mut command = tokio::process::Command::new("/bin/bash");
    command
        .args(["-c", INSTALL_SCRIPT])
        .env("CUA_DRIVER_NO_MODIFY_PATH", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| "The Cua Driver installer timed out.".to_string())?
        .map_err(|error| format!("Could not start the Cua Driver installer: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let details = tail_lines(&output.stderr, &output.stdout, INSTALL_ERROR_LINES);
    Err(format!(
        "The Cua Driver installer failed ({}). Install it from {QUICKSTART_URL} instead.\n\n```\n{details}\n```",
        output.status
    ))
}

/// Purpose: Keep the last lines of installer output for an error message.
/// Input: stderr, stdout, and the maximum number of lines.
/// Output: The last lines of stderr, or of stdout when stderr is empty.
fn tail_lines(stderr: &[u8], stdout: &[u8], max_lines: usize) -> String {
    let stream = if stderr.iter().all(u8::is_ascii_whitespace) {
        stdout
    } else {
        stderr
    };
    let text = String::from_utf8_lossy(stream);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(max_lines)..].join("\n")
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
/// Output: The driver path, or `None` when no candidate is executable.
fn find_driver() -> Option<PathBuf> {
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
    candidates.into_iter().find(|path| is_executable(path))
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
