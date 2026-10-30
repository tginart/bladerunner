//! Ask the client to run an `n_shot` tool call as session shots.
//!
//! Session shots are full sessions that the client forks from this one, each
//! in its own worktree. Only the client can create live sessions. The shell
//! falls back to subagent shots only when the client definitely did not accept
//! `x.ai/n_shot/fork`. A timeout does not cancel a queued gateway request.

use std::future::Future;
use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::Client as _;

const N_SHOT_FORK_METHOD: &str = "x.ai/n_shot/fork";
const N_SHOT_FORK_TIMEOUT: Duration = Duration::from_secs(10);
const UNCERTAIN_ACCEPTANCE: &str = concat!(
    "The client may still start the n-shot sessions. ",
    "Subagent fallback is disabled to avoid duplicate work. ",
    "Do not retry until the client outcome is known.",
);

/// Purpose: Ask the client to fork this session into n-shot sessions.
/// Input: The gateway, this session id, and the `n-shot` workflow args.
/// Output: The accepted label, `None` for safe fallback, or an uncertain-outcome error.
pub(crate) async fn request_session_shots(
    gateway: &xai_acp_lib::AcpAgentGatewaySender,
    session_id: &acp::SessionId,
    args: &serde_json::Value,
) -> Result<Option<String>, String> {
    let params = serde_json::json!({ "sessionId": session_id.0, "args": args });
    let raw = serde_json::value::to_raw_value(&params).expect("serialize n-shot fork params");
    let request = acp::ExtRequest::new(N_SHOT_FORK_METHOD, raw.into());
    await_acceptance(gateway.ext_method(request), N_SHOT_FORK_TIMEOUT).await
}

/// Purpose: Permit fallback only when the client did not accept the request.
/// Input: The client response future and the maximum wait duration.
/// Output: The accepted label, a safe fallback, or an uncertain-outcome error.
async fn await_acceptance(
    response: impl Future<Output = acp::Result<acp::ExtResponse>>,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let response = match tokio::time::timeout(timeout, response).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            let not_sent = xai_acp_lib::acp_channel_failure(&e)
                == Some(xai_acp_lib::AcpChannelFailure::SendFailed);
            let declined = matches!(
                e.code,
                acp::ErrorCode::MethodNotFound | acp::ErrorCode::InvalidParams
            );
            if not_sent || declined {
                tracing::debug!(error = %e, "client declined n-shot sessions; using subagent shots");
                return Ok(None);
            }
            return Err(format!(
                "n-shot fork response failed: {e}. {UNCERTAIN_ACCEPTANCE}"
            ));
        }
        Err(_) => {
            tracing::warn!("n-shot fork timed out; acceptance is unknown; no subagent fallback");
            return Err(format!("n-shot fork timed out. {UNCERTAIN_ACCEPTANCE}"));
        }
    };
    let body: serde_json::Value = serde_json::from_str(response.0.get())
        .map_err(|e| format!("invalid n-shot fork response: {e}. {UNCERTAIN_ACCEPTANCE}"))?;
    let label = body
        .get("label")
        .and_then(serde_json::Value::as_str)
        .filter(|label| !label.is_empty())
        .ok_or_else(|| format!("n-shot fork response has no label. {UNCERTAIN_ACCEPTANCE}"))?;
    Ok(Some(label.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use xai_acp_lib::{AcpAgentGatewaySender, AcpClientMessage};

    fn request() -> acp::ExtRequest {
        acp::ExtRequest::new(
            N_SHOT_FORK_METHOD,
            serde_json::value::to_raw_value(&serde_json::json!({}))
                .unwrap()
                .into(),
        )
    }

    fn response(body: serde_json::Value) -> acp::ExtResponse {
        acp::ExtResponse::new(serde_json::value::to_raw_value(&body).unwrap().into())
    }

    struct DelayedClient {
        request_tx: mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
        accepted_tx: mpsc::UnboundedSender<()>,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for DelayedClient {
        async fn request_permission(
            &self,
            _: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            unreachable!()
        }

        async fn session_notification(&self, _: acp::SessionNotification) -> acp::Result<()> {
            unreachable!()
        }

        async fn ext_method(&self, _: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
            let (accept_tx, accept_rx) = tokio::sync::oneshot::channel();
            self.request_tx.send(accept_tx).unwrap();
            accept_rx.await.unwrap();
            self.accepted_tx.send(()).unwrap();
            Ok(response(serde_json::json!({ "label": "n-shot-1" })))
        }
    }

    #[tokio::test]
    async fn timeout_does_not_allow_fallback_when_gateway_can_accept_late() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (request_tx, mut request_rx) = mpsc::unbounded_channel();
                let (accepted_tx, mut accepted_rx) = mpsc::unbounded_channel();
                let (gateway, receiver) =
                    xai_acp_lib::acp_gateway::<acp::AgentSide, _>(DelayedClient {
                        request_tx,
                        accepted_tx,
                    });
                let gateway_task = tokio::task::spawn_local(receiver.run());
                let (outcome, accept_tx) = tokio::join!(
                    await_acceptance(gateway.ext_method(request()), Duration::from_millis(1)),
                    request_rx.recv(),
                );
                assert!(
                    outcome.is_err(),
                    "an unknown outcome must not permit fallback"
                );

                // The real gateway still forwards the request after the timeout.
                // A late client acceptance must not race a subagent fallback.
                accept_tx.unwrap().send(()).unwrap();
                accepted_rx.recv().await.unwrap();
                gateway_task.abort();
            })
            .await;
    }

    #[tokio::test]
    async fn timeout_does_not_allow_fallback_when_request_is_still_queued() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gateway = AcpAgentGatewaySender::new(tx);
        let outcome =
            await_acceptance(gateway.ext_method(request()), Duration::from_millis(1)).await;
        assert!(outcome.is_err());
        let AcpClientMessage::ExtMethod(ext) = rx.try_recv().unwrap() else {
            panic!("expected queued n-shot fork request");
        };
        assert!(ext.response_tx.is_closed());
    }

    #[tokio::test]
    async fn explicit_declines_allow_fallback() {
        for code in [-32601, -32602] {
            let outcome = await_acceptance(
                std::future::ready(Err(acp::Error::new(code, "declined"))),
                N_SHOT_FORK_TIMEOUT,
            )
            .await;
            assert_eq!(outcome.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn closed_gateway_before_send_allows_fallback() {
        let (tx, rx) = mpsc::unbounded_channel::<AcpClientMessage>();
        drop(rx);
        let gateway = AcpAgentGatewaySender::new(tx);
        let outcome = request_session_shots(
            &gateway,
            &acp::SessionId::new("session-1"),
            &serde_json::json!({ "prompt": "fix it", "n": 2 }),
        )
        .await;
        assert_eq!(outcome.unwrap(), None);
    }

    #[tokio::test]
    async fn dropped_response_after_send_does_not_allow_fallback() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gateway = AcpAgentGatewaySender::new(tx);
        let mut result = Box::pin(await_acceptance(
            gateway.ext_method(request()),
            N_SHOT_FORK_TIMEOUT,
        ));
        assert!(futures::poll!(result.as_mut()).is_pending());
        drop(rx.try_recv().unwrap());
        assert!(result.await.is_err());
    }

    #[tokio::test]
    async fn client_error_does_not_allow_fallback() {
        let outcome = await_acceptance(
            std::future::ready(Err(acp::Error::internal_error())),
            N_SHOT_FORK_TIMEOUT,
        )
        .await;
        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn malformed_acceptance_does_not_allow_fallback() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "label": 1 }),
            serde_json::json!({ "label": "" }),
        ] {
            let outcome =
                await_acceptance(std::future::ready(Ok(response(body))), N_SHOT_FORK_TIMEOUT).await;
            assert!(outcome.is_err());
        }
    }

    #[tokio::test]
    async fn accepted_label_is_preserved() {
        let outcome = await_acceptance(
            std::future::ready(Ok(response(serde_json::json!({ "label": "n-shot-1" })))),
            N_SHOT_FORK_TIMEOUT,
        )
        .await;
        assert_eq!(outcome.unwrap().as_deref(), Some("n-shot-1"));
    }
}
