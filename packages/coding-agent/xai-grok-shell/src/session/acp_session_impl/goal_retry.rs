//! Purpose: Recover autonomous goal rounds after transient provider failures.
//! Input: A turn error, a consecutive retry budget, and a goal cancellation signal.
//! Output: A bounded retry delay, or a terminal decision.
//! Set `GROK_GOAL_MAX_CONSECUTIVE_RETRIES` to change the default of eight.
//! Set it to zero to disable goal retries. Delays use 20 percent jitter.

use std::time::Duration;

use agent_client_protocol as acp;
use tokio_util::sync::CancellationToken;

use super::SessionActor;

const DEFAULT_MAX_RETRIES: u32 = 8;
const MAX_DELAY: Duration = Duration::from_secs(300);
const MAX_RETRIES_ENV: &str = "GROK_GOAL_MAX_CONSECUTIVE_RETRIES";

/// Purpose: Bound consecutive failures independently of the sampler retry budget.
/// Input: A configured retry limit and each round result.
/// Output: A retry grant while the consecutive budget remains.
pub(super) struct GoalRetryState {
    consecutive_retries: u32,
    max_retries: u32,
}

impl GoalRetryState {
    pub(super) fn from_env() -> Self {
        let max_retries = match std::env::var(MAX_RETRIES_ENV) {
            Ok(value) => value.parse().unwrap_or_else(|_| {
                tracing::warn!(%value, "invalid goal retry limit; using the default");
                DEFAULT_MAX_RETRIES
            }),
            Err(_) => DEFAULT_MAX_RETRIES,
        };
        Self {
            consecutive_retries: 0,
            max_retries,
        }
    }

    pub(super) fn on_success(&mut self) {
        self.consecutive_retries = 0;
    }

    /// Purpose: Spend one retry only for a transient error within the budget.
    /// Input: The turn error and a jitter value from zero through one.
    /// Output: The attempt number, retry ceiling, and capped delay.
    pub(super) fn try_retry(
        &mut self,
        error: &acp::Error,
        jitter: f64,
    ) -> Option<(u32, u32, Duration)> {
        if self.consecutive_retries >= self.max_retries || !is_retryable(error) {
            return None;
        }
        let base_secs = 15_u64.saturating_mul(
            1_u64
                .checked_shl(self.consecutive_retries)
                .unwrap_or(u64::MAX),
        );
        let delay = Duration::from_secs_f64(
            base_secs.min(MAX_DELAY.as_secs()) as f64 * (0.8 + 0.4 * jitter),
        )
        .min(MAX_DELAY);
        self.consecutive_retries += 1;
        Some((self.consecutive_retries, self.max_retries, delay))
    }
}

/// Purpose: Prefer sampler metadata over the generic JSON-RPC internal-error code.
/// Input: A terminal turn error with optional sampler metadata.
/// Output: True for temporary transport, timeout, throttling, or server failures.
pub(super) fn is_retryable(error: &acp::Error) -> bool {
    let data = error.data.as_ref();
    if data
        .and_then(|d| d.get("should_retry"))
        .and_then(|v| v.as_bool())
        == Some(false)
        || matches!(
            i32::from(error.code),
            -32000 | -32002 | -32600 | -32602 | -32800
        )
    {
        return false;
    }
    let kind = data
        .and_then(|d| d.get("error_kind"))
        .and_then(|v| v.as_str());
    if matches!(
        kind,
        Some(
            "auth"
                | "serialization"
                | "empty_response"
                | "max_tokens_truncation"
                | "doom_loop_detected"
        )
    ) {
        return false;
    }
    // Idle timeouts are terminal inside the sampler but recoverable for a goal.
    if kind == Some("idle_timeout") {
        return true;
    }
    let sampler_retryable = data
        .and_then(|d| d.get("is_retryable"))
        .and_then(|v| v.as_bool());
    if let Some(status) = crate::sampling::error::http_status_from_error(error) {
        return sampler_retryable != Some(false)
            && reqwest::StatusCode::from_u16(status)
                .is_ok_and(xai_grok_sampling_types::is_retryable_api_status);
    }
    let detail = SessionActor::turn_error_detail(error)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if [
        "invalid client configuration",
        "invalid configuration",
        "invalid model",
        "invalid request",
        "invalid_request",
        "invalid_model",
        "unauthorized",
        "authentication",
        "certificate",
        "invalid url",
        "builder error",
        "user interrupt",
        "request cancelled",
        "request canceled",
        "user cancelled",
        "user canceled",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
    {
        return false;
    }
    if let Some(retryable) = sampler_retryable {
        return retryable;
    }
    if i32::from(error.code) == crate::sampling::error::RATE_LIMITED_ERROR_CODE {
        return true;
    }
    [
        "server error",
        "temporarily overloaded",
        "network",
        "transport",
        "connection",
        "timeout",
        "timed out",
        "request error:",
        "reqwest error stream:",
        "no response from model",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
}

/// Wait for a retry delay. A goal transition wins even when both futures are ready.
pub(super) async fn wait_for_retry(delay: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::goal_tracker::{GoalPauseReason, GoalStatus, GoalTracker};

    fn server_error() -> acp::Error {
        acp::Error::internal_error().data(serde_json::json!({
            "message": "Server error", "http_status": 503,
        }))
    }

    #[test]
    fn goal_retry_classifies_transient_and_permanent_errors() {
        for status in [429, 500, 502, 503, 504, 520, 529] {
            let error = acp::Error::internal_error().data(serde_json::json!({
                "message": "upstream unavailable", "http_status": status,
            }));
            assert!(is_retryable(&error), "HTTP {status}");
        }
        for message in [
            "Server error — Something went wrong on our side",
            "connection reset by peer",
            "network disconnected",
            "request timed out",
        ] {
            assert!(is_retryable(&acp::Error::internal_error().data(message)));
        }
        for status in [400, 401, 403, 404, 422, 525, 526] {
            let error = acp::Error::internal_error().data(serde_json::json!({
                "message": "Server error", "http_status": status,
            }));
            assert!(!is_retryable(&error), "HTTP {status}");
        }
        for error in [
            acp::Error::auth_required().data("connection failed"),
            acp::Error::invalid_params().data("invalid model"),
            acp::Error::internal_error()
                .data("invalid client configuration: Google ADC is not set up"),
            acp::Error::internal_error().data("invalid request: timeout is not a valid parameter"),
            acp::Error::new(-32800, "network request cancelled"),
            acp::Error::internal_error(),
        ] {
            assert!(!is_retryable(&error), "{error:?}");
        }
        let vetoed = acp::Error::internal_error().data(serde_json::json!({
            "http_status": 503, "should_retry": false,
        }));
        assert!(!is_retryable(&vetoed));
        let auth = acp::Error::internal_error().data(serde_json::json!({
            "message": "Server error", "error_kind": "auth", "is_retryable": false,
        }));
        assert!(!is_retryable(&auth));
        let invalid_stream = acp::Error::internal_error().data(serde_json::json!({
            "message": "stream error (invalid_request_error): invalid tool name",
            "error_kind": "api", "is_retryable": true,
        }));
        assert!(!is_retryable(&invalid_stream));
        let timeout = acp::Error::internal_error().data(serde_json::json!({
            "error_kind": "idle_timeout", "is_retryable": false,
        }));
        assert!(is_retryable(&timeout));
    }

    #[test]
    fn goal_retry_recovers_and_resets_consecutive_budget() {
        let mut state = GoalRetryState {
            consecutive_retries: 0,
            max_retries: 2,
        };
        for _ in 0..3 {
            assert_eq!(state.try_retry(&server_error(), 0.5).unwrap().0, 1);
            assert_eq!(state.try_retry(&server_error(), 0.5).unwrap().0, 2);
            // A successful round earns a fresh consecutive budget.
            state.on_success();
            assert_eq!(state.consecutive_retries, 0);
        }
    }

    #[test]
    fn goal_retry_backoff_is_exponential_jittered_and_capped() {
        let mut state = GoalRetryState {
            consecutive_retries: 0,
            max_retries: 10,
        };
        for seconds in [15, 30, 60, 120, 240, 300, 300, 300, 300, 300] {
            assert_eq!(
                state.try_retry(&server_error(), 0.5).unwrap().2,
                Duration::from_secs(seconds)
            );
        }
        assert!(state.try_retry(&server_error(), 0.5).is_none());
        state.on_success();
        let low = state.try_retry(&server_error(), 0.0).unwrap().2;
        state.on_success();
        let high = state.try_retry(&server_error(), 1.0).unwrap().2;
        assert_eq!(low, Duration::from_secs(12));
        assert_eq!(high, Duration::from_secs(18));
        state.consecutive_retries = 100;
        state.max_retries = 101;
        assert_eq!(state.try_retry(&server_error(), 1.0).unwrap().2, MAX_DELAY);
    }

    #[tokio::test]
    async fn goal_retry_pauses_after_exhaustion_or_permanent_error() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (actor, _rx) = super::super::support::build_actor().await;
                let mut state = GoalRetryState {
                    consecutive_retries: 0,
                    max_retries: 2,
                };
                for error in [
                    server_error(),
                    acp::Error::invalid_params().data("invalid model"),
                ] {
                    actor.goal_tracker.lock().create_goal(
                        "retry-test".into(),
                        "test".into(),
                        None,
                        0,
                        "now".into(),
                        None,
                    );
                    while state.try_retry(&error, 0.5).is_some() {
                        assert_eq!(actor.goal_tracker.lock().status(), Some(GoalStatus::Active));
                    }
                    let (_, _, pause) = SessionActor::post_turn_goal_degradation_plan(&Err(error));
                    assert!(actor.apply_infra_pause_after_turn_err(pause.unwrap()).await);
                    assert_eq!(
                        actor.goal_tracker.lock().status(),
                        Some(GoalStatus::InfraPaused)
                    );
                    state.on_success();
                }
            })
            .await;
    }

    #[tokio::test]
    async fn goal_retry_preserves_sampler_classification() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (actor, _rx) = super::super::support::build_actor().await;
                for (sampling_error, retryable) in [
                    (
                        xai_grok_sampling_types::SamplingError::InvalidConfiguration(
                            "Google ADC is not set up",
                        ),
                        false,
                    ),
                    (
                        xai_grok_sampling_types::SamplingError::IdleTimeout { elapsed_secs: 30 },
                        true,
                    ),
                    (
                        xai_grok_sampling_types::SamplingError::Api {
                            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                            message: "Server error".into(),
                            model_metadata: None,
                            retry_after_secs: None,
                            should_retry: Some(false),
                            error_code: None,
                        },
                        false,
                    ),
                ] {
                    let info = xai_grok_sampler::SamplingErrorInfo::from(&sampling_error);
                    let error = actor
                        .handle_sampling_failure(info)
                        .await
                        .err()
                        .expect("the sampler failure stays terminal");
                    assert_eq!(is_retryable(&error), retryable, "{error:?}");
                }
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn goal_retry_pause_and_stop_interrupt_backoff() {
        for stop in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut tracker = GoalTracker::new(dir.path().to_path_buf());
            tracker.create_goal("test".into(), "test".into(), None, 0, "now".into(), None);
            let cancel = tracker.retry_cancel().unwrap();
            let wait = tokio::spawn(async move { wait_for_retry(MAX_DELAY, &cancel).await });
            tokio::task::yield_now().await;
            let start = tokio::time::Instant::now();
            if stop {
                tracker.clear();
            } else {
                assert!(tracker.pause(GoalPauseReason::User));
                assert!(tracker.resume());
                // A fast resume cannot revive an old retry timer.
                assert!(!tracker.retry_cancel().unwrap().is_cancelled());
            }
            assert!(!wait.await.unwrap());
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn goal_retry_user_cancel_aborts_backoff() {
        let cancel = CancellationToken::new();
        let wait = tokio::spawn(async move { wait_for_retry(MAX_DELAY, &cancel).await });
        tokio::task::yield_now().await;
        let start = tokio::time::Instant::now();
        wait.abort();
        assert!(wait.await.unwrap_err().is_cancelled());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn goal_retry_queued_pause_and_stop_wake_backoff() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for command in ["/goal pause", "/goal stop"] {
                    let (actor, _rx) = super::super::support::build_actor().await;
                    super::super::support::set_goal_harness_for_tests(&actor);
                    actor.goal_tracker.lock().create_goal(
                        "test".into(),
                        "test".into(),
                        None,
                        0,
                        "now".into(),
                        None,
                    );
                    let cancel = actor.goal_tracker.lock().retry_cancel().unwrap();
                    let wait =
                        tokio::spawn(async move { wait_for_retry(MAX_DELAY, &cancel).await });
                    tokio::task::yield_now().await;
                    let start = tokio::time::Instant::now();
                    let (respond_to, _response) = tokio::sync::oneshot::channel();
                    let cancel_running = actor
                        .queue_input(super::super::QueueInputRequest::new(
                            vec![acp::ContentBlock::Text(acp::TextContent::new(command))],
                            "user-pause".into(),
                            super::super::PromptMode::Agent,
                            respond_to,
                        ))
                        .await;
                    assert!(!cancel_running);
                    assert!(!wait.await.unwrap());
                    assert_eq!(
                        actor.goal_tracker.lock().status(),
                        Some(GoalStatus::UserPaused)
                    );
                    assert!(start.elapsed() < Duration::from_secs(1));
                }
            })
            .await;
    }
}
