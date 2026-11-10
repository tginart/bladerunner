//! Purpose: Check MCP spawn, admission and recovery paths of the session actor.
//! Input: Synthetic stdio and HTTP MCP servers written in Python, and
//!     synthetic plugin manifests.
//! Output: Regression assertions. No network access outside 127.0.0.1.

use super::super::disk_full_tests::{
    actor_with_mock_sampler, block_on_session, current_thread_local, drain_gateway, run_prompt,
};
use super::super::support::create_test_actor;
use super::*;
use std::io::{BufRead as _, Write as _};
use std::path::Path;
use std::time::Duration;
use xai_grok_test_support::sse::responses_api_script_exact;
use xai_grok_test_support::{MockInferenceServer, ScriptedResponse};

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

/// A streamable-HTTP MCP server on 127.0.0.1 that lists one tool, `proof`.
/// It binds a free port, prints the port, and listens only after a line on
/// stdin, so the port refuses connections until then and no other process
/// can take it. It answers every POST with plain JSON and has no SSE stream
/// (GET and DELETE get 405). `FIXTURE_MODE=oauth` also serves OAuth
/// authorization-server metadata. `FIXTURE_MODE=stall` holds every GET open
/// and answers every POST with 503.
const HTTP_SERVER_SCRIPT: &str = r#"
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE = os.environ.get('FIXTURE_MODE', 'mcp')

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send_json(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_empty(self, status):
        self.send_response(status)
        self.send_header('Content-Length', '0')
        self.end_headers()

    def do_GET(self):
        if MODE == 'stall':
            time.sleep(30)
            return
        if MODE == 'oauth' and 'oauth-authorization-server' in self.path:
            base = 'http://' + self.headers['Host']
            self.send_json(200, {
                'issuer': base,
                'authorization_endpoint': base + '/authorize',
                'token_endpoint': base + '/token',
                'response_types_supported': ['code'],
            })
            return
        self.send_empty(405)

    def do_DELETE(self):
        self.send_empty(405)

    def do_POST(self):
        if MODE == 'stall':
            self.send_empty(503)
            return
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        request = json.loads(body)
        if 'id' not in request:
            self.send_empty(202)
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
        self.send_json(200, {'jsonrpc': '2.0', 'id': request['id'], 'result': result})

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler, bind_and_activate=False)
server.daemon_threads = True
server.server_bind()
print(server.server_address[1], flush=True)
sys.stdin.readline()
server.server_activate()
server.serve_forever()
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
    if python3_is_missing() {
        return;
    }
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
    if python3_is_missing() {
        return;
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let mut server = HttpFixture::bind(root.path(), "mcp");
            actor.mcp_state.lock().await.configs = vec![http_server("flaky", &server.url())];
            actor.ensure_mcp_tools_initialized().await;
            wait_for_unreachable_retry(&actor, "flaky").await;

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

            server.listen();
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

/// Purpose: Verify that a turn start retries a server that was down at
///     startup. When every server failed, the model sees no `search_tool`
///     or `use_tool`, so a retry on an MCP tool call can never fire.
/// Input: The only configured server refuses connections at startup and
///     listens before the turn. The model answers with text only.
/// Output: The server is ready with its tool after the turn.
#[test]
fn turn_start_retries_an_unreachable_server_without_an_mcp_tool_call() {
    if python3_is_missing() {
        return;
    }
    block_on_session(|| {
        current_thread_local(async {
            let root = tempfile::tempdir().unwrap();
            let inference = MockInferenceServer::start()
                .await
                .expect("mock inference server");
            inference.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            drain_gateway(gateway_rx);
            let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            ack_persistence_flushes(persistence_rx);
            let actor = actor_with_mock_sampler(&inference, persistence_tx, gateway_tx, None).await;

            let mut server = HttpFixture::bind(root.path(), "mcp");
            {
                let mut state = actor.mcp_state.lock().await;
                // The retry is due at once; the cooldown has its own tests.
                state.unreachable_retry_cooldown = Duration::ZERO;
                state.configs = vec![http_server("flaky", &server.url())];
            }
            actor.ensure_mcp_tools_initialized().await;
            wait_for_unreachable_retry(&actor, "flaky").await;
            let model_facing = actor.prepare_tool_definitions_inner().await;
            assert!(
                !model_facing
                    .iter()
                    .any(|d| d.function.name == "search_tool"),
                "with no MCP tools the model has no MCP tool to call"
            );

            server.listen();
            run_prompt(&actor, "turn-start-retry")
                .await
                .expect("the turn must finish");
            wait_until(&actor, |state| {
                state.get_client("flaky").is_some() && !state.init_failed.contains_key("flaky")
            })
            .await;
            wait_for_registered_tool(&actor, "flaky__proof").await;
        });
    });
}

/// Purpose: Verify that a headless session keeps retrying a server whose
///     OAuth discovery and anonymous probe both fail before the spawn.
/// Input: A non-interactive session and a server that holds GETs open and
///     answers POSTs with 503.
/// Output: The server waits for an unreachable-server retry.
#[tokio::test(flavor = "current_thread")]
async fn headless_unreachable_before_spawn_is_retryable() {
    if python3_is_missing() {
        return;
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            actor.attach_non_interactive.set(true);
            let server = HttpFixture::start(root.path(), "stall");
            actor.mcp_state.lock().await.configs = vec![http_server("down", &server.url())];
            actor.ensure_mcp_tools_initialized().await;
            wait_for_unreachable_retry(&actor, "down").await;
        })
        .await;
}

/// Purpose: Verify that a cancelled retry pass gives its servers back
///     at once, not after the attempt lease.
/// Input: A due retry against a server whose OAuth discovery hangs. The
///     pass is dropped during the spawn.
/// Output: The server is due again one cooldown after the attempt.
#[tokio::test(flavor = "current_thread")]
async fn cancelled_retry_pass_releases_its_attempts() {
    if python3_is_missing() {
        return;
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let server = HttpFixture::start(root.path(), "stall");
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs = vec![http_server("down", &server.url())];
                state.unreachable_retry_cooldown = Duration::ZERO;
                state.record_unreachable_failure("down", "refused".to_string());
            }
            let pass = actor.retry_unreachable_servers_at(std::time::Instant::now());
            assert!(
                tokio::time::timeout(Duration::from_millis(300), pass)
                    .await
                    .is_err(),
                "the pass must still wait for OAuth discovery"
            );
            let state = actor.mcp_state.lock().await;
            assert!(state.is_unreachable_retry_pending("down"));
            assert!(
                state.has_due_unreachable_retry(std::time::Instant::now()),
                "a dropped pass must not hold the server for the attempt lease"
            );
        })
        .await;
}

/// Purpose: Verify that a respawn whose attempt went stale does not
///     install its client.
/// Input: A claimed retry attempt, then a config change that drops the
///     claim, then a working client for the old attempt.
/// Output: No client and no tools for the server.
#[tokio::test(flavor = "current_thread")]
async fn stale_retry_attempt_discards_its_client() {
    if python3_is_missing() {
        return;
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let server = HttpFixture::start(root.path(), "mcp");
            let config = http_server("flaky", &server.url());
            let token = {
                let mut state = actor.mcp_state.lock().await;
                state.configs = vec![config.clone()];
                state.unreachable_retry_cooldown = Duration::ZERO;
                state.record_unreachable_failure("flaky", "refused".to_string());
                let (_, token) = state
                    .take_unreachable_retry_candidates(std::time::Instant::now())
                    .remove(0);
                // The server is removed and added back during the attempt,
                // so the token is stale.
                assert!(state.update_configs(vec![]));
                assert!(state.update_configs(vec![config.clone()]));
                token
            };
            let ew = xai_file_utils::events::EventWriter::noop();
            let ctx = crate::session::mcp_servers::McpSpawnCtx::for_session(
                "s",
                &ew,
                OauthInteractivity::Interactive,
                None,
            );
            let client =
                crate::session::mcp_servers::start_mcp_server(config, None, None, None, &ctx)
                    .await
                    .expect("the fixture is up");
            assert!(
                actor
                    .install_retried_client("flaky", token, client)
                    .await
                    .is_none()
            );
            let state = actor.mcp_state.lock().await;
            assert!(state.get_client("flaky").is_none());
            drop(state);
            let bridge = actor.agent.borrow().tool_bridge().clone();
            assert!(
                !bridge
                    .tool_definitions()
                    .await
                    .iter()
                    .any(|d| d.function.name == "flaky__proof")
            );
        })
        .await;
}

/// Purpose: Tell whether the Python fixtures cannot run. A test returns
///     early in that case, so a host without python3 skips, not fails.
/// Input: None.
/// Output: True when python3 is not on PATH.
fn python3_is_missing() -> bool {
    let is_found = std::process::Command::new("/usr/bin/env")
        .args(["python3", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !is_found {
        eprintln!("skipped: python3 is not on PATH");
    }
    !is_found
}

/// Purpose: Define a streamable-HTTP server.
/// Input: The server name and URL.
/// Output: An HTTP server definition.
fn http_server(name: &str, url: &str) -> acp::McpServer {
    acp::McpServer::Http(acp::McpServerHttp::new(name, url))
}

/// Purpose: Wait until a server failed its connect and waits for an
///     unreachable-server retry.
/// Input: An actor and the server name.
/// Output: None, or a bounded assertion failure.
async fn wait_for_unreachable_retry(actor: &SessionActor, name: &str) {
    wait_until(actor, |state| {
        !state.is_server_handshaking(name) && state.init_failed.contains_key(name)
    })
    .await;
    assert!(
        actor
            .mcp_state
            .lock()
            .await
            .is_unreachable_retry_pending(name),
        "{name} must wait for an unreachable-server retry"
    );
}

/// Purpose: Acknowledge every persistence flush so a turn can finish.
/// Input: The persistence receiver of the actor.
/// Output: None. A local task drains the receiver.
fn ack_persistence_flushes(mut rx: tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = msg {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
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
struct HttpFixture {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    port: u16,
}

impl HttpFixture {
    /// Purpose: Start the HTTP fixture with its port bound but not
    ///     listening, so connections to it are refused.
    /// Input: The fixture directory and the `FIXTURE_MODE`.
    /// Output: The fixture. Call [`Self::listen`] to accept connections.
    fn bind(root: &Path, mode: &str) -> Self {
        let script = root.join("http_server.py");
        std::fs::write(&script, HTTP_SERVER_SCRIPT).unwrap();
        #[allow(clippy::disallowed_methods)] // killed and waited on in Drop
        let mut child = std::process::Command::new("/usr/bin/env")
            .args(["python3", "-u"])
            .arg(&script)
            .env("FIXTURE_MODE", mode)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line
            .trim()
            .parse()
            .expect("the HTTP fixture reports its port");
        Self { child, stdin, port }
    }

    /// Purpose: Start the HTTP fixture and wait until it accepts connections.
    /// Input: The fixture directory and the `FIXTURE_MODE`.
    /// Output: The running fixture.
    fn start(root: &Path, mode: &str) -> Self {
        let mut fixture = Self::bind(root, mode);
        fixture.listen();
        fixture
    }

    /// Purpose: Make the fixture listen and wait until it accepts connections.
    /// Input: None.
    /// Output: None, or a bounded assertion failure.
    fn listen(&mut self) {
        writeln!(self.stdin).unwrap();
        self.stdin.flush().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the HTTP fixture must start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Purpose: Verify that the `/mcps` login rebuild of an HTTP client keeps a
///     plugin server's OAuth client ID.
/// Input: A plugin whose `.mcp.json` declares an HTTP server with an OAuth
///     client ID, and an HTTP fixture that serves OAuth metadata.
/// Output: The rebuilt client carries the client ID.
#[tokio::test(flavor = "current_thread")]
async fn oauth_rebuild_keeps_the_plugin_client_id() {
    if python3_is_missing() {
        return;
    }
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let actor = actor(root.path()).await;
            let server = HttpFixture::start(root.path(), "oauth");
            let registry =
                plugin_registry_with_oauth(root.path(), "slack", &server.url(), "slack-byo-client");
            *actor.plugin_registry.borrow_mut() = Some(Arc::new(registry));
            actor.mcp_state.lock().await.configs = vec![http_server("slack", &server.url())];
            let client = actor
                .recreate_http_client_with_oauth("slack")
                .await
                .expect("the fixture serves OAuth metadata");
            assert_eq!(
                client
                    .byo_oauth_config()
                    .and_then(|c| c.client_id.as_deref()),
                Some("slack-byo-client")
            );
        })
        .await;
}

/// Purpose: Build a one-plugin registry whose MCP server declares an OAuth client ID.
/// Input: A fixture directory, the plugin and server name, the server URL
///     and the client ID.
/// Output: A plugin registry with the plugin active.
fn plugin_registry_with_oauth(
    root: &Path,
    name: &str,
    url: &str,
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
        "url": url,
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
