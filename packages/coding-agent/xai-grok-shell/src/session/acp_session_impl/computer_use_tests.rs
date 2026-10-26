//! Purpose: Check MCP retry and independent server readiness with local fixtures.
//! Input: Synthetic stdio servers with a file gate for a slow handshake.
//! Output: Regression assertions without Cua or desktop access.

use super::super::support::create_test_actor;
use super::*;
use crate::session::mcp_servers::McpClient;

const SERVER_SCRIPT: &str = r#"
import json
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
        if len(sys.argv) > 1:
            gate = Path(sys.argv[1])
            gate.with_suffix('.started').touch()
            while not gate.exists():
                time.sleep(0.01)
        result = {'tools': [{
            'name': 'proof', 'description': 'Synthetic proof tool',
            'inputSchema': {'type': 'object', 'properties': {}},
        }]}
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
            let definitions = actor.agent.borrow().tool_bridge().tool_definitions().await;
            assert!(
                definitions
                    .iter()
                    .any(|tool| tool.function.name == "computer-use__proof")
            );
            std::fs::write(gate, "release").unwrap();
            wait_ready(&actor, "slow").await.shutdown().await;
            client.shutdown().await;
        })
        .await;
}
