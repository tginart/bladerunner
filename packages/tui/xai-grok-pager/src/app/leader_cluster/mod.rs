//! Multi-client leader cluster: a REAL leader IPC server fronting a REAL
//! `MvpAgent` in a child test process, with each client a full pager view-model
//! (`AppView`) wired through the production leader bridge. Deterministically
//! exercises the multi-client surface the PTY `LeaderCluster` covers, one
//! layer down, without terminals or screen captures. Each leader generation
//! occupies its own process so a crash ends every local task and session thread.
//!
//! Per client: `LeaderClient::connect(sock).into_channels()` →
//! [`bridge_channels`] → one `AppView`; inbound ACP is pumped through
//! `acp_handler::handle`, user intent is driven through `dispatch`, and
//! effects run through the real `effects::execute` (the same loop
//! `event_loop::run` performs, minus the terminal).
//!
//! Env sandboxing follows this crate's `serial(BLADERUNNER_HOME)` idiom; note
//! `grok_home()` is process-cached (OnceLock), so disk assertions always go
//! through [`effective_grok_home`] rather than assuming the temp dir won.
//!
//! The scenarios are `#[ignore]`d in the shared lib test binary: the harness
//! mutates process-global env (proxy URLs, `XAI_API_KEY`,
//! `GROK_LEADER_SOCKET`, `BLADERUNNER_HOME`) for a real agent's whole lifetime, and
//! in a several-thousand-test process that poisons concurrently-running tests
//! (and `grok_home()`'s OnceLock is usually already pinned). Run on demand:
//!
//! ```bash
//! cargo test -p xai-grok-pager --lib -- app::leader_cluster --ignored --test-threads=1
//! ```
//!
//! Follow-up to un-ignore: move the scenarios to a dedicated test binary
//! (single-process isolation via a test-harness feature over the pub(crate)
//! seams), where env is set before any process-global's first touch.
//!
//! Unix-only: the leader transport here is a unix socket.

use std::future::Future;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use agent_client_protocol as acp;
use tempfile::TempDir;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpClientRx, acp_send};
use xai_grok_shell::leader::{
    ClientCapabilities as LeaderClientCapabilities, ClientMode, ConnectionStatus,
    LEADER_SOCKET_ENV, LeaderClient, LeaderEnvUrls, LeaderLock, LeaderReconnector,
    LeaderServerControlState, LeaderServerMetadata, ReconnectPolicy, run_leader_server,
};
use xai_grok_test_support::MockInferenceServer;

use super::actions::{Action, TaskResult};
use super::agent::AgentState;
use super::agent_view::AgentView;
use super::app_view::{AppView, AuthState, TrustState};
use super::{acp_handler, dispatch, effects};
use crate::acp::leader_bridge::bridge_channels;
use crate::acp::model_state::ModelState;
use crate::scrollback::block::RenderBlock;

const PUMP_TICK: Duration = Duration::from_millis(10);
const TURN_BUDGET: Duration = Duration::from_secs(60);

const GENERATION_CHILD_ENV: &str = "BLADERUNNER_CLUSTER_GENERATION";
const GENERATION_TEST: &str = "app::leader_cluster::generation_worker";

/// Purpose: Drive a scenario and its pager clients on one local runtime.
/// Input: The scenario future.
/// Output: The completed scenario after all child generations have been reaped.
fn run_cluster_scenario(scenario: impl Future<Output = ()>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, scenario);
}

/// Purpose: Own every task and actor of one leader generation through its process.
/// Input: The child test process.
/// Output: A process owner that kills and reaps the child before its environment is removed.
struct LeaderGeneration {
    child: Child,
}

impl LeaderGeneration {
    fn kill_and_wait(&mut self) -> ExitStatus {
        if self
            .child
            .try_wait()
            .expect("inspect generation exit")
            .is_none()
        {
            self.child.kill().expect("kill leader generation");
        }
        self.child.wait().expect("reap leader generation")
    }
}

impl Drop for LeaderGeneration {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

/// Purpose: Run the real leader server and agent in an isolated child process.
/// Input: The inherited mock-server configuration and sandbox socket path.
/// Output: A live generation that the parent can crash without any graceful shutdown writes.
#[test]
#[ignore = "child entrypoint; started by PagerLeaderCluster"]
fn generation_worker() {
    if std::env::var_os(GENERATION_CHILD_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let sock_path = PathBuf::from(std::env::var_os(LEADER_SOCKET_ENV).expect("generation socket"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut keepalive = None;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (acp_tx, acp_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock_path.clone(),
            lock_path: sock_path.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: xai_grok_version::VERSION.to_string(),
        });
        let server = tokio::task::spawn_local(async move {
            run_leader_server(
                sock_path,
                acp_tx,
                response_rx,
                CancellationToken::new(),
                true,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
                xai_grok_shell::agent::activity::AgentActivity::default(),
                tokio::sync::watch::channel(true).1,
                tokio::sync::watch::channel(false).0,
                tokio::sync::watch::channel(xai_grok_shell::leader::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await
            .expect("generation leader server");
        });
        let agent = xai_grok_shell::leader::in_process::spawn_agent(acp_rx, response_tx).await;
        keepalive = Some(agent.keepalive);
        server.await.expect("generation server task");
    });
    drop(local);
    drop(keepalive);
}

/// Await a bring-up step with a hard budget so an on-demand run that hangs
/// names its phase instead of parking until the test-runner kill.
async fn bounded<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .unwrap_or_else(|_| panic!("leader-cluster bring-up timed out: {what}"))
}

/// The grok home the agent actually persisted under: `grok_home()` is
/// process-cached, so an earlier test in this binary may have pinned it.
fn effective_grok_home() -> PathBuf {
    xai_grok_config::grok_home()
}

/// Concatenated agent-message text across a view's scrollback (copy of the
/// acp_handler tests' helper; that one is test-mod private).
fn agent_message_text(view: &AgentView) -> String {
    let mut out = String::new();
    for i in 0..view.scrollback.len() {
        if let Some(entry) = view.scrollback.get(i)
            && let RenderBlock::AgentMessage(msg) = &entry.block
        {
            out.push_str(&msg.text());
        }
    }
    out
}

/// One pager client: a full `AppView` behind the production leader bridge.
struct ClusterClient {
    app: AppView,
    rx: AcpClientRx,
    tasks: JoinSet<TaskResult>,
    progress_tx: tokio::sync::mpsc::UnboundedSender<effects::RestoreProgressMsg>,
    _progress_rx: tokio::sync::mpsc::UnboundedReceiver<effects::RestoreProgressMsg>,
    bridge_cancel: CancellationToken,
    /// Present when the client was built with a reconnector: observes
    /// generation bumps after a leader kill/respawn.
    status_rx: Option<tokio::sync::watch::Receiver<ConnectionStatus>>,
}

impl ClusterClient {
    /// Drain everything currently ready (inbound ACP + finished tasks).
    /// Returns whether anything was processed.
    fn pump_once(&mut self) -> bool {
        let mut progressed = false;
        while let Ok(msg) = self.rx.try_recv() {
            acp_handler::handle(msg, &mut self.app);
            self.drain_pending_effects();
            progressed = true;
        }
        while let Some(joined) = self.tasks.try_join_next() {
            if let Ok(result) = joined {
                let effs = dispatch::dispatch(Action::TaskComplete(result), &mut self.app);
                self.process_effects(effs);
            }
            progressed = true;
        }
        progressed
    }

    fn drain_pending_effects(&mut self) {
        if !self.app.pending_effects.is_empty() {
            let effs = std::mem::take(&mut self.app.pending_effects);
            self.process_effects(effs);
        }
    }

    /// The event loop's `process_effects`, minus terminal/auth-handle wiring
    /// (that fn is event_loop-private; this mirrors its body).
    fn process_effects(&mut self, effs: Vec<super::actions::Effect>) {
        let flags = super::event_loop::session_flags_for_effects(&mut self.app, &effs);
        for eff in effs {
            let (_quit, _meta) = effects::execute(
                eff,
                &mut self.tasks,
                &self.app.acp_tx,
                &self.app.cwd,
                &flags,
                &self.progress_tx,
            );
        }
        self.drain_pending_effects();
    }

    /// Dispatch a user action and run its effects.
    fn act(&mut self, action: Action) {
        let effs = dispatch::dispatch(action, &mut self.app);
        self.process_effects(effs);
    }

    /// Pump until `pred(app)` holds, within [`TURN_BUDGET`]. No fixed sleeps
    /// beyond the pump tick; panics with `what` on expiry. Single-client sugar
    /// over [`pump_clients_until`] so there is exactly one pump loop.
    async fn pump_until(&mut self, what: &str, pred: impl Fn(&AppView) -> bool) {
        pump_clients_until(&mut [self], what, |clients| pred(&clients[0].app)).await;
    }

    /// The most recently created agent view (scenarios add tabs in order).
    fn latest_agent(&self) -> &AgentView {
        self.app
            .agents
            .values()
            .last()
            .expect("client has no agent view yet")
    }

    fn agent_for_session(&self, sid: &str) -> &AgentView {
        self.app
            .agents
            .values()
            .find(|a| {
                a.session
                    .session_id
                    .as_ref()
                    .is_some_and(|s| s.0.as_ref() == sid)
            })
            .unwrap_or_else(|| panic!("no agent view for session {sid}"))
    }

    /// Create a new session through the real dispatch → effect → agent path.
    async fn new_session(&mut self) -> String {
        self.act(Action::NewSession);
        self.pump_until("session/new completes", |app| {
            app.agents
                .values()
                .any(|a| a.session.session_id.is_some() && !a.session.loading_replay)
        })
        .await;
        self.latest_agent()
            .session
            .session_id
            .as_ref()
            .expect("session id set")
            .0
            .to_string()
    }

    /// Attach to an existing session (viewer path) and wait for the replay to
    /// land.
    async fn load_session(&mut self, sid: &str) {
        self.act(Action::LoadSession(sid.to_string(), None, false));
        let sid_owned = sid.to_string();
        self.pump_until("session/load completes", move |app| {
            app.agents.values().any(|a| {
                a.session
                    .session_id
                    .as_ref()
                    .is_some_and(|s| s.0.as_ref() == sid_owned)
                    && !a.session.loading_replay
            })
        })
        .await;
    }

    /// Drive one full turn on the active agent and wait until it lands
    /// (sentinel visible + agent back to Idle).
    async fn run_turn(&mut self, prompt: &str, sentinel: &str) {
        self.act(Action::SendPrompt(prompt.to_string()));
        let sentinel_owned = sentinel.to_string();
        self.pump_until("turn completes", move |app| {
            app.agents.values().any(|a| {
                matches!(a.session.state, AgentState::Idle)
                    && agent_message_text(a).contains(&sentinel_owned)
            })
        })
        .await;
    }

    fn sever(self) {
        self.bridge_cancel.cancel();
    }
}

/// Pump several clients until `pred` holds across them, within
/// [`TURN_BUDGET`]; panics with `what` on expiry.
async fn pump_clients_until(
    clients: &mut [&mut ClusterClient],
    what: &str,
    pred: impl Fn(&[&mut ClusterClient]) -> bool,
) {
    let deadline = tokio::time::Instant::now() + TURN_BUDGET;
    loop {
        for client in clients.iter_mut() {
            client.pump_once();
        }
        if pred(clients) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pump_clients_until budget expired: {what}"
        );
        tokio::time::sleep(PUMP_TICK).await;
    }
}

/// The cluster: leader server + real agent, plus knobs to kill/respawn the
/// leader generation under the same socket path.
struct PagerLeaderCluster {
    sock_path: PathBuf,
    server: MockInferenceServer,
    // Drop order keeps the process dead before the flock, environment, and temp home disappear.
    generation: Option<LeaderGeneration>,
    workdir: TempDir,
    authenticated: bool,
    /// Held for the cluster's lifetime so a `LeaderReconnector`-driven
    /// `connect_or_spawn` can never win the flock and spawn a subprocess —
    /// it always takes the wait-for-socket path onto our in-process server.
    _flock: LeaderLock,
    /// Restored on drop, INCLUDING panic unwinds. Field order is load-bearing:
    /// `_flock` drops first (removes its lock/sock files while the env still
    /// points at the sandbox), then the guards restore the env, then the temp
    /// home is deleted.
    _env: Vec<crate::test_util::EnvVarGuard>,
    _grok_home: TempDir,
}

impl PagerLeaderCluster {
    /// Stand up the cluster. Callers MUST be `#[serial_test::serial(BLADERUNNER_HOME)]`
    /// (env mutation) and run inside `run_cluster_scenario`.
    async fn start() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let server = MockInferenceServer::start().await.expect("mock server");
        let grok_home = TempDir::new().unwrap();
        let workdir = TempDir::new().unwrap();
        let sock_path = grok_home.path().join("leader-cluster.sock");

        let env = vec![
            crate::test_util::EnvVarGuard::set("BLADERUNNER_HOME", grok_home.path()),
            crate::test_util::EnvVarGuard::set("GROK_CLI_CHAT_PROXY_BASE_URL", server.url()),
            crate::test_util::EnvVarGuard::set("GROK_XAI_API_BASE_URL", server.url()),
            crate::test_util::EnvVarGuard::set("XAI_API_KEY", "test-key-for-ci"),
            crate::test_util::EnvVarGuard::set("GROK_TELEMETRY_ENABLED", "false"),
            crate::test_util::EnvVarGuard::set("GROK_FEEDBACK_ENABLED", "false"),
            crate::test_util::EnvVarGuard::set("GROK_TRACE_UPLOAD", "false"),
            // Pin every leader-path derivation (LeaderLock::new / reconnect's
            // connect_or_spawn) to this cluster's socket.
            crate::test_util::EnvVarGuard::set(LEADER_SOCKET_ENV, &sock_path),
        ];

        // Hold the flock for the cluster's lifetime (see field doc).
        let mut flock = LeaderLock::new("");
        assert!(
            flock.try_acquire().expect("acquire cluster flock"),
            "cluster flock unexpectedly held"
        );
        flock.write_pid().expect("stamp cluster flock");

        let mut cluster = Self {
            sock_path,
            server,
            generation: None,
            workdir,
            authenticated: false,
            _flock: flock,
            _env: env,
            _grok_home: grok_home,
        };
        cluster.spawn_leader_generation().await;
        cluster
    }

    /// Purpose: Start a generation only after the previous process has been reaped.
    /// Input: The cluster's fixed socket and inherited sandbox configuration.
    /// Output: A real server and agent with no tasks shared with an earlier generation.
    async fn spawn_leader_generation(&mut self) {
        assert!(
            self.generation.is_none(),
            "previous generation is still owned"
        );
        let log_path = self._grok_home.path().join("leader-generation.log");
        let log = std::fs::File::create(&log_path).expect("create generation log");
        let child = Command::new(std::env::current_exe().expect("current test binary"))
            .args(["--exact", GENERATION_TEST, "--ignored", "--nocapture"])
            .env(GENERATION_CHILD_ENV, "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("clone generation log"))
            .stderr(log)
            .spawn()
            .expect("spawn leader generation");
        self.generation = Some(LeaderGeneration { child });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !self.sock_path.exists() && tokio::time::Instant::now() < deadline {
            let status = self.generation.as_mut().unwrap().child.try_wait().unwrap();
            assert!(
                status.is_none(),
                "leader generation exited before bind: {status:?}\n{}",
                std::fs::read_to_string(&log_path).unwrap(),
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(self.sock_path.exists(), "leader socket never bound");
    }

    /// Purpose: Crash the generation and reap every local task and resident actor before replacement.
    /// Input: The current generation's child process.
    /// Output: Its confirmed exit status and a vacant socket path.
    async fn kill_leader(&mut self) -> ExitStatus {
        let mut generation = self.generation.take().expect("live leader generation");
        let status = generation.kill_and_wait();
        match std::fs::remove_file(&self.sock_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove crashed generation socket: {error}"),
        }
        self.authenticated = false;
        status
    }

    async fn respawn_leader(&mut self) {
        self.spawn_leader_generation().await;
    }

    /// Connect a pager client. With `reconnect: true` the bridge gets a real
    /// `LeaderReconnector` (socket pinned via `GROK_LEADER_SOCKET`, flock held
    /// by the cluster, so reconnects always adopt the in-process server).
    async fn client(&mut self, name: &str, reconnect: bool) -> ClusterClient {
        let conn = bounded(
            "client connect",
            LeaderClient::connect(
                self.sock_path.clone(),
                name,
                ClientMode::Stdio,
                LeaderClientCapabilities {
                    client_version: Some("0.0.0-test".to_string()),
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("cluster client connect");
        let (leader_tx, leader_rx) = conn.into_channels();

        let cancel = CancellationToken::new();
        let (reconnector, status_rx) = if reconnect {
            let (status_tx, status_rx) = LeaderReconnector::status_channel();
            let reconnector = LeaderReconnector::new(
                name,
                ClientMode::Stdio,
                LeaderEnvUrls {
                    grok_ws_url: String::new(),
                    grok_ws_origin: String::new(),
                },
                LeaderClientCapabilities {
                    client_version: Some("0.0.0-test".to_string()),
                    ..Default::default()
                },
                status_tx,
            );
            (Some(reconnector), Some(status_rx))
        } else {
            (None, None)
        };

        let bridge = bridge_channels(
            leader_tx,
            leader_rx,
            cancel.clone(),
            reconnector,
            ReconnectPolicy::unbounded(),
        )
        .expect("bridge spawn");
        let tx = bridge.channel.tx;
        let rx = bridge.channel.rx;

        // Same handshake the pager performs after bridging (spawn path).
        let _init: acp::InitializeResponse = bounded(
            "initialize",
            acp_send(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                    .client_capabilities(
                        acp::ClientCapabilities::new()
                            .fs(acp::FileSystemCapabilities::new())
                            .terminal(false),
                    )
                    .meta(
                        serde_json::json!({
                            "startupHints": {
                                "nonInteractive": true,
                                "skipGitStatus": true,
                                "skipProjectLayout": true
                            },
                            "clientType": "pager-cluster",
                            "clientVersion": "0.0.0-test",
                        })
                        .as_object()
                        .cloned(),
                    ),
                &tx,
            ),
        )
        .await
        .expect("initialize through bridge");
        if !self.authenticated {
            let _: acp::AuthenticateResponse = bounded(
                "authenticate",
                acp_send(
                    acp::AuthenticateRequest::new(acp::AuthMethodId::new("xai.api_key"))
                        .meta(serde_json::json!({ "headless": true }).as_object().cloned()),
                    &tx,
                ),
            )
            .await
            .expect("authenticate through bridge");
            self.authenticated = true;
        }

        let mut app = AppView::new(tx, ModelState::default(), Vec::new());
        app.leader_mode = true;
        app.auth_state = AuthState::Done;
        app.trust_state = TrustState::Done;
        app.cwd = self.workdir.path().to_path_buf();

        let (progress_tx, progress_rx) = tokio::sync::mpsc::unbounded_channel();
        ClusterClient {
            app,
            rx,
            tasks: JoinSet::new(),
            progress_tx,
            _progress_rx: progress_rx,
            bridge_cancel: cancel,
            status_rx,
        }
    }

    /// Inference request count (chat/responses/messages only), for
    /// no-turn-was-re-driven invariants.
    fn inference_request_count(&self) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|e| {
                e.path.contains("/chat/completions")
                    || e.path.contains("/responses")
                    || e.path.contains("/messages")
            })
            .count()
    }
}

impl Drop for PagerLeaderCluster {
    fn drop(&mut self) {
        // Reap the process before field destruction restores the environment and removes its home.
        drop(self.generation.take());
    }
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

mod scenarios;
