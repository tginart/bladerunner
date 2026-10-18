//! A real agent behind a leader server, in this process rather than a child.

use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol as acp;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, simplex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    LineBufferedRead,
};

use crate::agent::config::Config as AgentConfig;
use crate::agent::mvp_agent::MvpAgent;

const SIMPLEX_BUF: usize = 8 * 1024 * 1024;

/// Purpose: Own the agent I/O tasks and retain its allocation.
/// Input: The task handles and an agent reference.
/// Output: An owner whose `keepalive` must outlive the ambient `LocalSet`.
/// Note: Abort the I/O tasks to end a connection. Destroy the `LocalSet` before `keepalive`.
pub struct InProcessAgent {
    pub tasks: Vec<JoinHandle<()>>,
    pub keepalive: Rc<MvpAgent>,
}

/// Purpose: Start a real agent on the current `LocalSet`.
/// Input: The request receiver and the response sender.
/// Output: The I/O tasks and an agent reference. Retain the reference past the `LocalSet`.
/// Note: This function panics if the configuration cannot construct an agent.
pub async fn spawn_agent(
    mut to_agent: UnboundedReceiver<String>,
    from_agent: UnboundedSender<String>,
) -> InProcessAgent {
    let (agent_in_read, mut agent_in_write) = simplex(SIMPLEX_BUF);
    let (agent_out_read, agent_out_write) = simplex(SIMPLEX_BUF);

    let (keepalive_tx, keepalive_rx) = oneshot::channel();

    // The bootstrap future stays on the task heap to limit the caller stack size.
    let connection = tokio::task::spawn_local(async move {
        let config = AgentConfig::default();
        let auth_manager = Arc::new(config.create_auth_manager());
        let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = Rc::new(
            MvpAgent::new(GatewaySender::new(gateway_tx), &config, auth_manager, None)
                .expect("valid agent config"),
        );
        // Do not start agent tasks if the caller cannot retain their owner.
        if keepalive_tx.send(Rc::clone(&agent)).is_err() {
            return;
        }
        let incoming = LineBufferedRead::spawn_local(agent_in_read.compat());
        let (conn, handle_io) =
            acp::AgentSideConnection::new(agent, agent_out_write.compat_write(), incoming, |fut| {
                tokio::task::spawn_local(fut);
            });
        tokio::task::spawn_local(
            GatewayReceiver::new(gateway_rx, conn)
                .with_on_meta(xai_file_utils::trace_context::span_from_meta_traceparent)
                .run(),
        );
        let _ = handle_io.await;
    });

    let requests = tokio::task::spawn_local(async move {
        while let Some(msg) = to_agent.recv().await {
            if agent_in_write.write_all(msg.as_bytes()).await.is_err()
                || agent_in_write.write_all(b"\n").await.is_err()
            {
                break;
            }
        }
    });

    let responses = tokio::task::spawn_local(async move {
        let mut reader = BufReader::new(agent_out_read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let msg = line.trim_end_matches(['\r', '\n']).to_string();
                    if !msg.is_empty() {
                        let _ = from_agent.send(msg);
                    }
                }
            }
        }
    });

    let keepalive = keepalive_rx
        .await
        .expect("in-process agent task ended before it built an agent");
    InProcessAgent {
        tasks: vec![connection, requests, responses],
        keepalive,
    }
}
