//! Purpose: Check MCP spawn, admission and recovery paths of the session actor.
//! Input: Synthetic stdio and HTTP MCP servers written in Python, and
//!     synthetic plugin manifests.
//! Output: Regression assertions. No network access outside 127.0.0.1.

use super::super::support::create_test_actor;
use super::*;
use std::path::Path;
use std::time::Duration;

/// A stdio MCP server. It lists one tool, named by `FIXTURE_TOOL_NAME`, and
/// echoes the arguments of a `tools/call` back as text.
const STDIO_SERVER_SCRIPT: &str = r#"
import json
import os
import sys

tool = os.environ.get('FIXTURE_TOOL_NAME', 'proof')
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
        result = {'tools': [{
            'name': tool, 'description': 'Synthetic fixture tool',
            'inputSchema': {'type': 'object', 'properties': {'value': {'type': 'string'}}},
        }]}
    elif method == 'tools/call':
        args = request['params'].get('arguments') or {}
        text = 'called ' + request['params']['name'] + ' with ' + str(args.get('value'))
        result = {'content': [{'type': 'text', 'text': text}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
"#;

/// A streamable-HTTP MCP server on `127.0.0.1:<argv[1]>` that lists one
/// tool, `proof`. It answers every POST with plain JSON and has no SSE
/// stream (GET and DELETE get 405).
const HTTP_SERVER_SCRIPT: &str = r#"
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(405)
        self.send_header('Content-Length', '0')
        self.end_headers()

    do_DELETE = do_GET

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        request = json.loads(body)
        if 'id' not in request:
            self.send_response(202)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        method = request['method']
        if method == 'initialize':
            result = {
                'protocolVersion': request['params']['protocolVersion'],
                'capabilities': {'tools': {}},
                'serverInfo': {'name': 'fixture', 'version': '1'},
            }
        elif method == 'tools/list':
            result = {'tools': [{
                'name': 'proof', 'description': 'Synthetic proof tool',
                'inputSchema': {'type': 'object', 'properties': {}},
            }]}
        else:
            result = {}
        data = json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()
"#;

/// Purpose: Create an actor whose fixture files stay inside a temporary directory.
/// Input: The fixture directory.
/// Output: An actor with no initial MCP configuration.
async fn actor(root: &Path) -> Arc<SessionActor> {
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = create_test_actor(0, 100_000, 85, gateway_tx, persistence_tx).await;
    actor.session_info.cwd = root.to_str().unwrap().to_string();
    std::fs::write(root.join("stdio_server.py"), STDIO_SERVER_SCRIPT).unwrap();
    Arc::new(actor)
}

/// Purpose: Define the stdio fixture server with a given tool name.
/// Input: The fixture directory, the server name and the raw tool name.
/// Output: A stdio server definition.
fn stdio_server(root: &Path, name: &str, tool: &str) -> acp::McpServer {
    let script = root.join("stdio_server.py");
    acp::McpServer::Stdio(
        acp::McpServerStdio::new(name, "/usr/bin/env")
            .args(vec![
                "python3".to_string(),
                "-u".to_string(),
                script.to_str().unwrap().to_string(),
            ])
            .env(vec![acp::EnvVariable::new("FIXTURE_TOOL_NAME", tool)]),
    )
}

/// Purpose: Wait until the bridge holds a tool with the given client name.
/// Input: An actor and a qualified tool name.
/// Output: None, or a bounded assertion failure.
async fn wait_for_registered_tool(actor: &SessionActor, name: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let bridge = actor.agent.borrow().tool_bridge().clone();
            if bridge
                .tool_definitions()
                .await
                .iter()
                .any(|d| d.function.name == name)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("tool {name} must be registered"));
}

/// Purpose: Verify that a tool whose qualified name is over the provider
///     limit is kept, stays out of every provider function list, and still
///     dispatches by its original name.
/// Input: A stdio fixture with a 40-char server name and a 40-char tool name.
/// Output: A registered catalog entry and a successful call.
#[tokio::test(flavor = "current_thread")]
async fn long_qualified_mcp_name_is_kept_and_dispatches() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let server = format!("srv_{}", "s".repeat(36));
            let tool = format!("tool_{}", "t".repeat(35));
            let qualified = format!("{server}__{tool}");
            assert!(qualified.len() > xai_grok_mcp::tool_name::PROVIDER_TOOL_NAME_MAX_CHARS);
            actor.mcp_state.lock().await.configs = vec![stdio_server(root.path(), &server, &tool)];
            actor.ensure_mcp_tools_initialized().await;
            wait_for_registered_tool(&actor, &qualified).await;

            // The model sees MCP tools only through search_tool / use_tool.
            let model_facing = actor.prepare_tool_definitions_inner().await;
            for definition in &model_facing {
                assert!(
                    xai_grok_mcp::tool_name::validate_tool_name(&definition.function.name).is_ok(),
                    "provider function name {} breaks the provider rules",
                    definition.function.name
                );
            }

            let bridge = actor.agent.borrow().tool_bridge().clone();
            let result = bridge
                .call(&qualified, serde_json::json!({"value": "ok"}), "call-1")
                .await
                .expect("long qualified name must dispatch");
            assert!(
                result
                    .prompt_text
                    .contains(&format!("called {tool} with ok")),
                "the server must get the raw tool name: {}",
                result.prompt_text
            );
        })
        .await;
}

/// Purpose: Verify that a server that was down at startup gets its tools
///     once it comes up and the retry cooldown is over.
/// Input: An HTTP server config on a free port with no listener, then the
///     HTTP fixture started on that port.
/// Output: The server first waits for a retry, then is ready with its tool.
#[tokio::test(flavor = "current_thread")]
async fn unreachable_server_is_retried_and_becomes_available() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let port = free_local_port();
            let url = format!("http://127.0.0.1:{port}/mcp");
            actor.mcp_state.lock().await.configs =
                vec![acp::McpServer::Http(acp::McpServerHttp::new("flaky", &url))];
            actor.ensure_mcp_tools_initialized().await;
            wait_until(&actor, |state| {
                !state.is_server_handshaking("flaky") && state.init_failed.contains_key("flaky")
            })
            .await;
            assert!(
                actor
                    .mcp_state
                    .lock()
                    .await
                    .is_unreachable_retry_pending("flaky"),
                "a refused connection must be retryable"
            );

            // A retry inside the cooldown does nothing.
            actor
                .retry_unreachable_servers_at(std::time::Instant::now())
                .await;
            assert!(
                actor
                    .mcp_state
                    .lock()
                    .await
                    .init_failed
                    .contains_key("flaky")
            );

            let _server = HttpFixture::start(root.path(), port);
            let after_cooldown = std::time::Instant::now()
                + crate::session::mcp_servers::McpState::UNREACHABLE_RETRY_COOLDOWN;
            actor.retry_unreachable_servers_at(after_cooldown).await;
            {
                let state = actor.mcp_state.lock().await;
                assert!(!state.init_failed.contains_key("flaky"));
                assert!(!state.is_unreachable_retry_pending("flaky"));
                assert!(state.get_client("flaky").is_some());
            }
            wait_for_registered_tool(&actor, "flaky__proof").await;
        })
        .await;
}

/// Purpose: Find a TCP port on 127.0.0.1 that nothing listens on now.
/// Input: None.
/// Output: The port number.
fn free_local_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Purpose: Wait until the MCP state satisfies a condition.
/// Input: An actor and the condition.
/// Output: None, or a bounded assertion failure.
async fn wait_until(
    actor: &SessionActor,
    condition: impl Fn(&crate::session::mcp_servers::McpState) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition(&*actor.mcp_state.lock().await) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("MCP state must reach the condition");
}

/// The HTTP fixture process. It is killed on drop.
struct HttpFixture(std::process::Child);

impl HttpFixture {
    /// Purpose: Start the HTTP fixture and wait until it accepts connections.
    /// Input: The fixture directory and the port.
    /// Output: The running fixture.
    fn start(root: &Path, port: u16) -> Self {
        let script = root.join("http_server.py");
        std::fs::write(&script, HTTP_SERVER_SCRIPT).unwrap();
        #[allow(clippy::disallowed_methods)] // killed and waited on in Drop
        let child = std::process::Command::new("/usr/bin/env")
            .args(["python3", "-u"])
            .arg(&script)
            .arg(port.to_string())
            .spawn()
            .unwrap();
        let fixture = Self(child);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the HTTP fixture must start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        fixture
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Purpose: Verify that the shared spawn OAuth map carries a plugin's client ID.
/// Input: A plugin whose `.mcp.json` declares an HTTP server with an OAuth client ID.
/// Output: The client ID in the map that the `/mcps` login rebuild uses.
#[tokio::test(flavor = "current_thread")]
async fn spawn_oauth_config_map_includes_plugin_client_id() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let registry = plugin_registry_with_oauth(root.path(), "slack", "slack-byo-client");
            *actor.plugin_registry.borrow_mut() = Some(Arc::new(registry));
            let map = actor.spawn_oauth_config_map(root.path());
            assert_eq!(
                map.get("slack").and_then(|c| c.client_id.as_deref()),
                Some("slack-byo-client")
            );
        })
        .await;
}

/// Purpose: Build a one-plugin registry whose MCP server declares an OAuth client ID.
/// Input: A fixture directory, the plugin and server name, and the client ID.
/// Output: A plugin registry with the plugin active.
fn plugin_registry_with_oauth(
    root: &Path,
    name: &str,
    client_id: &str,
) -> xai_grok_agent::plugins::PluginRegistry {
    use xai_grok_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
    use xai_grok_agent::plugins::manifest::PluginManifest;
    use xai_grok_agent::plugins::{PluginOrigin, PluginRegistry, PluginScope};

    let plugin_root = root.join(name);
    std::fs::create_dir_all(&plugin_root).unwrap();
    let mcp_json = plugin_root.join(".mcp.json");
    let config = serde_json::json!({"mcpServers": {name: {
        "type": "http",
        "url": "https://mcp.example.invalid/mcp",
        "oauth": {"clientId": client_id},
    }}});
    std::fs::write(&mcp_json, config.to_string()).unwrap();
    let manifest = PluginManifest {
        name: name.into(),
        version: None,
        description: None,
        author: None,
        homepage: None,
        repository: None,
        license: None,
        keywords: vec![],
        skills: None,
        commands: None,
        agents: None,
        hooks: None,
        mcp_servers: None,
        lsp_servers: None,
        runtime: None,
    };
    let plugin = DiscoveredPlugin {
        manifest,
        id: PluginId::new(PluginScope::User, &plugin_root, name),
        root: plugin_root.clone(),
        canonical_root: plugin_root.clone(),
        scope: PluginScope::User,
        origin: PluginOrigin::UserGrok,
        trusted: true,
        skill_dirs: vec![],
        command_dirs: vec![],
        agent_dirs: vec![],
        hooks_path: None,
        mcp_config_path: Some(mcp_json),
        lsp_config_path: None,
        runtime_wasm: None,
        conflict: None,
    };
    PluginRegistry::from_discovered(vec![plugin], &[], &[name.to_string()])
}
