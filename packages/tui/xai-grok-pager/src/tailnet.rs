//! Tailnet status for `/tailnet`.
//!
//! Purpose: Show which machines are on the user's tailnet and whether they are online.
//! Method: Run `tailscale status --json` and format the peers as text.
//! Note: This module only reads status. It never changes the tailnet.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;

const TAILSCALE_COMMANDS: &[&str] = &[
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

/// A one-screen report of the tailnet: the state, then one line per machine.
pub async fn report() -> Result<String> {
    let status = read_status_json().await?;
    format_report(&status)
}

async fn read_status_json() -> Result<Value> {
    for command in TAILSCALE_COMMANDS {
        let Ok(output) = tokio::process::Command::new(command)
            .args(["status", "--json"])
            .stderr(Stdio::null())
            .output()
            .await
        else {
            continue;
        };
        if output.status.success() {
            return serde_json::from_slice(&output.stdout)
                .context("could not read the tailscale status");
        }
    }
    bail!("Tailscale is not installed, or is not running")
}

fn format_report(status: &Value) -> Result<String> {
    let state = status["BackendState"].as_str().unwrap_or("unknown");
    if state != "Running" {
        return Ok(format!("tailnet: {state}"));
    }
    let mut machines = vec![&status["Self"]];
    if let Some(peers) = status["Peer"].as_object() {
        machines.extend(peers.values());
    }
    let online = machines.iter().filter(|m| m["Online"] == true).count();
    let mut lines = vec![format!("tailnet: {online} of {} online", machines.len())];
    lines.extend(machines.into_iter().map(describe_machine));
    Ok(lines.join("\n"))
}

fn describe_machine(machine: &Value) -> String {
    let name = machine["HostName"].as_str().unwrap_or("?");
    let os = machine["OS"].as_str().unwrap_or("?");
    let state = if machine["Online"] == true { "online " } else { "offline" };
    let ip = machine["TailscaleIPs"][0].as_str().unwrap_or("");
    format!("  {name:<24} {os:<8} {state}  {ip}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lists_self_and_peers_with_online_count() {
        let status = serde_json::json!({
            "BackendState": "Running",
            "Self": {"HostName": "mac", "OS": "macOS", "Online": true, "TailscaleIPs": ["100.1.1.1"]},
            "Peer": {
                "a": {"HostName": "dgx", "OS": "linux", "Online": true, "TailscaleIPs": ["100.2.2.2"]},
                "b": {"HostName": "spark-b3fd", "OS": "linux", "Online": false, "TailscaleIPs": ["100.3.3.3"]}
            }
        });
        let report = format_report(&status).unwrap();
        assert!(report.starts_with("tailnet: 2 of 3 online"));
        assert!(report.contains("dgx"));
        assert!(report.contains("offline"));
    }

    #[test]
    fn stopped_tailnet_reports_its_state() {
        let status = serde_json::json!({"BackendState": "Stopped"});
        assert_eq!(format_report(&status).unwrap(), "tailnet: Stopped");
    }
}
