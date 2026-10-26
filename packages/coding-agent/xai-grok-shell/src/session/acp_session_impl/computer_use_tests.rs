//! Purpose: Check MCP retry and independent server readiness with local fixtures.
//! Input: Synthetic stdio servers with a file gate for a slow handshake.
//! Output: Regression assertions without Cua or desktop access.

use super::super::support::{create_test_actor, test_agent_with_tools};
use super::*;
use crate::session::mcp_servers::McpClient;
use crate::session::tool_index::{Bm25ToolSearchIndex, ToolMetadataSnapshot};
use xai_grok_tools::types::tool_index::ToolSearchIndex;

const SERVER_SCRIPT: &str = r#"
import json
import os
from pathlib import Path
import sys
import time

for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    if method == 'initialize':
        result = {
            'protocolVersion': request['params']['protocolVersion'],
            'capabilities': {'tools': {}},
            'serverInfo': {'name': 'fixture', 'version': '1'},
        }
    elif method == 'tools/list':
        if os.environ.get('FIXTURE_FAIL_TOOLS_LIST') == '1':
            print(json.dumps({
                'jsonrpc': '2.0', 'id': request['id'],
                'error': {'code': -32603, 'message': 'Synthetic list failure'},
            }), flush=True)
            continue
        if len(sys.argv) > 1:
            gate = Path(sys.argv[1])
            gate.with_suffix('.started').touch()
            while not gate.exists():
                time.sleep(0.01)
        result = {'tools': [{
            'name': 'proof', 'description': 'Synthetic proof tool',
            'inputSchema': {'type': 'object', 'properties': {}},
        }]}
        if os.environ.get('FIXTURE_EMPTY_TOOLS') == '1':
            result['tools'] = []
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
"#;

/// Purpose: Create an actor whose fixture files stay inside a temporary directory.
/// Input: The fixture directory.
/// Output: An actor with no initial MCP configuration.
async fn actor(root: &Path) -> Arc<SessionActor> {
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = create_test_actor(0, 100_000, 85, gateway_tx, persistence_tx).await;
    actor.session_info.cwd = root.to_str().unwrap().to_string();
    std::fs::write(root.join("server.py"), SERVER_SCRIPT).unwrap();
    Arc::new(actor)
}

/// Purpose: Define a synthetic MCP process with an optional tool-list gate.
/// Input: A fixture directory, server name, executable, and optional gate path.
/// Output: A stdio server definition.
fn server(root: &Path, name: &str, executable: &Path, gate: Option<&Path>) -> acp::McpServer {
    let mut args = vec![
        "python3".to_string(),
        "-u".to_string(),
        root.join("server.py").to_str().unwrap().to_string(),
    ];
    if let Some(gate) = gate {
        args.push(gate.to_str().unwrap().to_string());
    }
    acp::McpServer::Stdio(acp::McpServerStdio::new(name, executable).args(args))
}

/// Purpose: Wait for a named server without a wait for other configured servers.
/// Input: An actor and the expected server name.
/// Output: A ready client, or a bounded assertion failure.
async fn wait_ready(actor: &SessionActor, name: &str) -> Arc<McpClient> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let client = {
                let state = actor.mcp_state.lock().await;
                if state.is_server_handshaking(name) {
                    None
                } else {
                    state.get_client(name).cloned()
                }
            };
            if let Some(client) = client {
                client.ensure_initialized().await.unwrap();
                return client;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server must become ready independently")
}

/// Purpose: Observe the published snapshot after a named tool becomes available.
/// Input: An actor and a qualified fixture tool name.
/// Output: The snapshot that contains the tool.
async fn wait_for_tool_snapshot(actor: &SessionActor, name: &str) -> ToolMetadataSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = {
                let snapshot = actor.tool_metadata_snapshot.lock().unwrap();
                snapshot
                    .tools
                    .iter()
                    .any(|tool| tool.qualified_name == name)
                    .then(|| snapshot.clone())
            };
            if let Some(snapshot) = snapshot {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the connected fixture tool must reach the snapshot")
}

#[tokio::test(flavor = "current_thread")]
/// Purpose: Verify a successful retry after the same executable path becomes valid.
/// Input: A missing executable that the fixture later creates.
/// Output: A ready client after one explicit enable.
async fn enable_retries_an_identical_config_after_spawn_failure() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let executable = root.path().join("recovered-driver");
            let config = server(root.path(), SERVER_NAME, &executable, None);
            actor.mcp_state.lock().await.configs = vec![config.clone()];
            actor.ensure_mcp_tools_initialized().await;
            {
                let state = actor.mcp_state.lock().await;
                assert!(state.has_finished_init());
                assert!(state.get_client(SERVER_NAME).is_none());
            }
            std::os::unix::fs::symlink("/usr/bin/env", &executable).unwrap();
            {
                let mut state = actor.mcp_state.lock().await;
                let diff = update_enabled_server(&mut state, SERVER_NAME, true, Some(config))
                    .unwrap()
                    .expect("an unavailable server must trigger a fresh init");
                assert_eq!(diff.added, [SERVER_NAME]);
                assert_eq!(diff.removed, [SERVER_NAME]);
            }
            actor.ensure_mcp_tools_initialized().await;
            wait_ready(&actor, SERVER_NAME).await.shutdown().await;
        })
        .await;
}

/// Purpose: Keep MCP startup active after the caller cancels a server toggle.
/// Input: A synthetic server and a toggle future dropped at its first suspension.
/// Output: A ready client and a successful identical enable without a state reset.
#[tokio::test(flavor = "current_thread")]
async fn canceled_computer_use_enable_does_not_abandon_initialization() {
    const CHILD: &str = "BLADERUNNER_TEST_COMPUTER_USE_CANCEL";
    if std::env::var_os(CHILD).is_none() {
        let sandbox = xai_grok_test_support::TestSandbox::new();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        sandbox.apply_to_std_command(&mut command);
        let output = command
            .args([
                "canceled_computer_use_enable_does_not_abandon_initialization",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(sandbox.workspace())
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let config_path = crate::util::config::user_config_path();
            std::fs::write(&config_path, "disabled_mcp_servers = ['computer-use']\n").unwrap();
            let actor = actor(root.path()).await;
            let config = server(root.path(), SERVER_NAME, Path::new("/usr/bin/env"), None);
            let mut enable =
                Box::pin(actor.set_mcp_server_enabled(SERVER_NAME, true, Some(config.clone())));
            assert!(futures::poll!(enable.as_mut()).is_pending());
            drop(enable);

            let client = wait_ready(&actor, SERVER_NAME).await;
            assert!(actor.mcp_state.lock().await.is_initialized());
            tokio::time::timeout(Duration::from_secs(5), async {
                while std::fs::read_to_string(&config_path)
                    .unwrap()
                    .contains(SERVER_NAME)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the detached toggle must finish its preference save");
            tokio::time::timeout(
                Duration::from_secs(5),
                actor.set_mcp_server_enabled(SERVER_NAME, true, Some(config)),
            )
            .await
            .expect("an identical enable must not wait for abandoned initialization")
            .unwrap();
            client.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
/// Purpose: Verify independent tool availability before a slow server completes.
/// Input: A fast computer-use fixture and a second fixture with a closed gate.
/// Output: Usable computer-use tools while the second handshake remains active.
async fn healthy_computer_use_becomes_ready_before_an_unrelated_handshake() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let gate = root.path().join("release");
            actor.mcp_state.lock().await.configs = vec![
                server(root.path(), SERVER_NAME, Path::new("/usr/bin/env"), None),
                server(root.path(), "slow", Path::new("/usr/bin/env"), Some(&gate)),
            ];
            actor.ensure_mcp_tools_initialized().await;
            let client = wait_ready(&actor, SERVER_NAME).await;
            assert!(actor.mcp_state.lock().await.is_server_handshaking("slow"));
            let snapshot = wait_for_tool_snapshot(&actor, "computer-use__proof").await;
            assert!(!snapshot.mcp_initialized);
            let definitions = actor.agent.borrow().tool_bridge().tool_definitions().await;
            assert!(
                definitions
                    .iter()
                    .any(|tool| tool.function.name == "computer-use__proof")
            );
            std::fs::write(gate, "release").unwrap();
            wait_ready(&actor, "slow").await.shutdown().await;
            assert!(
                wait_for_tool_snapshot(&actor, "slow__proof")
                    .await
                    .mcp_initialized
            );
            client.shutdown().await;
        })
        .await;
}

/// Purpose: Reject an initialized transport whose tool list failed.
/// Input: A fixture that accepts initialize but rejects tools/list.
/// Output: No ready computer-use client and an unavailable status.
#[tokio::test(flavor = "current_thread")]
async fn tool_list_failure_does_not_report_computer_use_ready() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let mut config = server(root.path(), SERVER_NAME, Path::new("/usr/bin/env"), None);
            let acp::McpServer::Stdio(config) = &mut config else {
                unreachable!();
            };
            config
                .env
                .push(acp::EnvVariable::new("FIXTURE_FAIL_TOOLS_LIST", "1"));
            actor.mcp_state.lock().await.configs = vec![acp::McpServer::Stdio(config.clone())];
            actor.ensure_mcp_tools_initialized().await;
            let client = wait_ready(&actor, SERVER_NAME).await;
            assert_eq!(
                client.state_kind().await,
                xai_grok_mcp::servers::ClientStateKind::Ready
            );
            assert!(
                actor
                    .mcp_state
                    .lock()
                    .await
                    .init_failed
                    .contains_key(SERVER_NAME)
            );
            assert!(actor.wait_for_computer_use_ready().await.is_none());
            assert!(actor.computer_use_status().await.contains("unavailable"));
            client.shutdown().await;
        })
        .await;
}

/// Purpose: Keep discovery tools visible while a late server can still supply tools.
/// Input: An empty or failed first server and a second server behind a gate.
/// Output: An incomplete empty index, visible discovery tools, and final readiness.
#[tokio::test(flavor = "current_thread")]
async fn empty_or_failed_early_server_keeps_computer_use_discovery_available() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for environment in ["FIXTURE_EMPTY_TOOLS", "FIXTURE_FAIL_TOOLS_LIST"] {
                let root = tempfile::tempdir().unwrap();
                let actor = actor(root.path()).await;
                *actor.agent.borrow_mut() = test_agent_with_tools(vec![
                    xai_grok_tools::registry::types::ToolConfig::for_tool::<
                        xai_grok_tools::implementations::search_tool::SearchTool,
                    >(),
                    xai_grok_tools::registry::types::ToolConfig::for_tool::<
                        xai_grok_tools::implementations::use_tool::UseTool,
                    >(),
                ])
                .await;
                let bridge = actor.agent.borrow().tool_bridge().clone();
                bridge
                    .update_resource(xai_grok_tools::types::tool_index::ToolIndex(Arc::new(
                        Bm25ToolSearchIndex::new(actor.tool_metadata_snapshot.clone()),
                    )))
                    .await;
                let gate = root.path().join("release");
                let mut first = server(root.path(), SERVER_NAME, Path::new("/usr/bin/env"), None);
                let acp::McpServer::Stdio(config) = &mut first else {
                    unreachable!();
                };
                config.env.push(acp::EnvVariable::new(environment, "1"));
                actor.mcp_state.lock().await.configs = vec![
                    first,
                    server(root.path(), "slow", Path::new("/usr/bin/env"), Some(&gate)),
                ];
                actor.ensure_mcp_tools_initialized().await;
                let client = wait_ready(&actor, SERVER_NAME).await;
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !actor
                        .mcp_reminder_dirty
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                let index = Bm25ToolSearchIndex::new(actor.tool_metadata_snapshot.clone());
                let snapshot = index.search_snapshot("", 0);
                assert!(!snapshot.is_ready, "{environment}");
                assert_eq!(snapshot.total_hidden_tools, 0);
                let definitions = bridge.tool_definitions().await;
                for name in ["search_tool", "use_tool"] {
                    assert!(definitions.iter().any(|tool| tool.function.name == name));
                }
                std::fs::write(gate, "release").unwrap();
                wait_ready(&actor, "slow").await.shutdown().await;
                assert!(
                    wait_for_tool_snapshot(&actor, "slow__proof")
                        .await
                        .mcp_initialized
                );
                client.shutdown().await;
            }
        })
        .await;
}
