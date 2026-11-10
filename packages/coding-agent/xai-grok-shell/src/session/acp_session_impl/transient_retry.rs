//! Turn-level retry of transient sampler failures.
//!
//! Purpose: The sampler retries 429, 5xx and connection errors inside one
//! request, but it never retries a stalled stream (`IdleTimeout`), and a
//! stream that breaks after it started used to end the turn. This module
//! resubmits the turn for those failures, with a slow backoff and two
//! budgets: a few retries per sampling step and a cap for the whole prompt.
//! It automates the manual "Continue" that revived a stalled session.
//!
//! Note: A failure class that the sampler already retried to exhaustion
//! (HTTP 5xx, connection errors) is not retried again here. Stacking the two
//! ladders would turn a hard outage into a ~17 minute wait.
//!
//! Note: Subagents and budgeted workflow children stay terminal. The early
//! returns in `handle_sampling_failure` keep the workflow children out, and
//! the spawn-time `enabled` flag keeps subagents out.
use super::*;

/// Resubmits per sampling step. Resets after a successful response.
pub(super) const MAX_TRANSIENT_TURN_RETRIES: u32 = 3;

/// Resubmits for the whole prompt. Long agentic prompts earn the per-step
/// budget again after every successful sample, so without this cap a partial
/// outage multiplies retries by the round count.
pub(super) const MAX_TRANSIENT_RETRIES_PER_PROMPT: u32 = 10;

/// Floor of the wall clock for one recovery episode.
const TRANSIENT_RETRY_WINDOW_FLOOR: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Slower than the sampler's own ladder, which is already spent when a
/// failure gets here. The sleep site adds jitter: idle timeouts skip the
/// sampler's jitter, so concurrent sessions would otherwise retry together.
const TRANSIENT_TURN_RETRY_BACKOFF: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(10),
    std::time::Duration::from_secs(30),
];

const _: () = assert!(MAX_TRANSIENT_TURN_RETRIES as usize == TRANSIENT_TURN_RETRY_BACKOFF.len());

/// Env kill switch: `GROK_TURN_TRANSIENT_RETRY=0` restores the old
/// fail-the-turn behavior for new sessions.
const TRANSIENT_RETRY_ENV: &str = "GROK_TURN_TRANSIENT_RETRY";

/// Display prefix of `SamplingError::EventStreamError`: the response stream
/// broke after it started (connection reset mid-body).
const MID_STREAM_TRANSPORT_PREFIX: &str = "reqwest error stream:";

/// In-stream Anthropic error events, which the Messages stream reports as a
/// synthesized 500 with the event type in front of the message. An HTTP 5xx
/// response renders its body differently, so these markers do not match it.
const MID_STREAM_SERVER_ERROR_MARKERS: [&str; 2] = [
    "(status 500): overloaded_error:",
    "(status 500): api_error:",
];

/// Transport failures that fail the same way on every attempt.
const DETERMINISTIC_TRANSPORT_MARKERS: [&str; 9] = [
    "builder error",
    "relative url",
    "invalid url",
    "certificate",
    "tls",
    "dns error",
    "failed to lookup address",
    "no such host",
    "nodename nor servname",
];

/// Transient-retry state for one prompt.
///
/// The actor holds it (in a `Cell`) because auto-recovery, stop-hook
/// continuations and the goal loop enter the turn loop again within one
/// prompt; a counter local to the loop would reset the prompt cap.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TransientRetryState {
    /// Resolved at spawn. `Default` is off, so test actors keep the old
    /// fail-fast behavior unless a test turns it on.
    pub(crate) enabled: bool,
    /// Resubmits used this sampling step.
    pub(crate) step_attempts: u32,
    /// Resubmits used this prompt.
    pub(crate) prompt_attempts: u32,
    /// First failure of the current recovery episode; cleared on success.
    pub(crate) episode_start: Option<tokio::time::Instant>,
}

/// One granted resubmit: the attempt number and the ceiling to show the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransientRetryGrant {
    pub(crate) attempt: u32,
    pub(crate) max_retries: u32,
    pub(crate) delay: std::time::Duration,
}

impl TransientRetryState {
    /// Spawn-time state. Subagents stay terminal: their parent decides what
    /// to do with a failed child.
    pub(crate) fn for_session(is_subagent: bool) -> Self {
        let enabled = !is_subagent && xai_grok_config::env_bool(TRANSIENT_RETRY_ENV) != Some(false);
        Self {
            enabled,
            ..Self::default()
        }
    }

    /// Clear every counter at the start of a prompt.
    pub(crate) fn begin_prompt(self) -> Self {
        Self {
            enabled: self.enabled,
            ..Self::default()
        }
    }

    /// A response arrived: the step budget and the episode clock reset.
    pub(crate) fn on_response(self) -> Self {
        Self {
            step_attempts: 0,
            episode_start: None,
            ..self
        }
    }

    /// Compaction rebuilt the request, so the step budget starts again. The
    /// episode clock keeps running: the outage has not ended.
    pub(crate) fn on_compaction(self) -> Self {
        Self {
            step_attempts: 0,
            ..self
        }
    }

    /// Whether both budgets and the episode window still allow a resubmit.
    fn budget_remaining(&self, window: std::time::Duration) -> bool {
        self.step_attempts < MAX_TRANSIENT_TURN_RETRIES
            && self.prompt_attempts < MAX_TRANSIENT_RETRIES_PER_PROMPT
            && self
                .episode_start
                .is_none_or(|start| start.elapsed() < window)
    }

    /// Spend one resubmit if `error` is transient and budget remains.
    /// Returns the new state and the grant, or `None` when the turn must fail.
    pub(crate) fn try_grant(
        self,
        error: &xai_grok_sampler::SamplingErrorInfo,
        idle_timeout: std::time::Duration,
    ) -> Option<(Self, TransientRetryGrant)> {
        if !self.enabled
            || !transient_retry_eligible(error)
            || !self.budget_remaining(transient_retry_window(idle_timeout))
        {
            return None;
        }
        let delay = transient_backoff_delay(self.step_attempts);
        let next = Self {
            step_attempts: self.step_attempts + 1,
            prompt_attempts: self.prompt_attempts + 1,
            episode_start: self
                .episode_start
                .or_else(|| Some(tokio::time::Instant::now())),
            ..self
        };
        let grant = TransientRetryGrant {
            attempt: next.step_attempts,
            max_retries: transient_display_ceiling(next.step_attempts, next.prompt_attempts),
            delay,
        };
        Some((next, grant))
    }
}

/// The episode window, checked at each failure. A stalled retry spends one
/// idle-timeout cycle, so `2 * idle_timeout` keeps at least two admissible.
fn transient_retry_window(idle_timeout: std::time::Duration) -> std::time::Duration {
    TRANSIENT_RETRY_WINDOW_FLOOR.max(idle_timeout.saturating_mul(2))
}

/// Delay before the resubmit after `attempts_used` resubmits; the last rung
/// repeats.
fn transient_backoff_delay(attempts_used: u32) -> std::time::Duration {
    let last = TRANSIENT_TURN_RETRY_BACKOFF.len() - 1;
    TRANSIENT_TURN_RETRY_BACKOFF[(attempts_used as usize).min(last)]
}

/// The `Retrying (N/M)` ceiling: attempts used this step plus the further
/// attempts both remaining budgets allow.
fn transient_display_ceiling(step_attempts: u32, prompt_attempts: u32) -> u32 {
    step_attempts
        + MAX_TRANSIENT_TURN_RETRIES
            .saturating_sub(step_attempts)
            .min(MAX_TRANSIENT_RETRIES_PER_PROMPT.saturating_sub(prompt_attempts))
}

/// Purpose: Decide whether a terminal sampler failure is worth a
/// turn-level resubmit.
/// Input: The sampler's error info.
/// Output: True for a stream stall (`IdleTimeout`) or a stream that broke
/// after it started. False for everything else.
/// Note: The sampler already retried HTTP 5xx and connection errors to
/// exhaustion, so they stay terminal here. 4xx, auth, and deterministic
/// transport errors (bad URL, TLS, DNS) fail the same way again. The vetoes
/// mirror `SamplingError::is_retry_vetoed`.
pub(super) fn transient_retry_eligible(error: &xai_grok_sampler::SamplingErrorInfo) -> bool {
    use xai_grok_sampler::SamplingErrorKind;
    if error.should_retry == Some(false)
        || xai_grok_sampling_types::is_context_length_error(&error.message)
        // A rejected image fails the same way again; the sampler already
        // stripped images and gave up.
        || error.error_code == Some(xai_grok_sampling_types::ApiErrorCode::InvalidImage)
    {
        return false;
    }
    match error.kind {
        SamplingErrorKind::IdleTimeout => true,
        SamplingErrorKind::Http => {
            error.message.starts_with(MID_STREAM_TRANSPORT_PREFIX)
                && !is_deterministic_transport_error(&error.message)
        }
        // Status-less: a server error event inside the stream. A status
        // means an HTTP response the sampler classified (and retried) itself,
        // except the in-stream events the Messages stream maps to a 500.
        SamplingErrorKind::Api => match error.status_code {
            None => error.is_retryable,
            Some(_) => MID_STREAM_SERVER_ERROR_MARKERS
                .iter()
                .any(|marker| error.message.contains(marker)),
        },
        SamplingErrorKind::Auth
        | SamplingErrorKind::Serialization
        | SamplingErrorKind::RateLimited
        | SamplingErrorKind::EmptyResponse
        | SamplingErrorKind::MaxTokensTruncation
        | SamplingErrorKind::DoomLoopDetected => false,
    }
}

/// The transcript note sent before a resubmit.
fn transient_retry_notice(cause: &str, grant: TransientRetryGrant) -> String {
    format!(
        "\n\n_{cause}. Retrying the request (attempt {} of {}); any partial output \
         above is discarded._\n\n",
        grant.attempt, grant.max_retries
    )
}

fn is_deterministic_transport_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    DETERMINISTIC_TRANSPORT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

impl SessionActor {
    /// Clear the transient-retry counters at the start of a prompt.
    pub(crate) fn begin_prompt_transient_retry(&self) {
        self.transient_retry
            .set(self.transient_retry.get().begin_prompt());
    }

    /// Called by `handle_sampling_failure` after the compaction and auth
    /// arms, before the terminal path. Returns the recovery when the failure
    /// is transient and budget remains.
    pub(super) fn transient_retry_recovery(
        &self,
        error: &xai_grok_sampler::SamplingErrorInfo,
    ) -> Option<SamplerFailureRecovery> {
        let state = self.transient_retry.get();
        let Some((next, grant)) = state.try_grant(error, self.inference_idle_timeout) else {
            if state.enabled && transient_retry_eligible(error) {
                xai_grok_telemetry::unified_log::error(
                    "shell.turn.transient_retry_exhausted",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "kind": error.kind.as_str(),
                        "status_code": error.status_code,
                        "step_retries_used": state.step_attempts,
                        "prompt_retries_used": state.prompt_attempts,
                    })),
                );
            }
            return None;
        };
        self.transient_retry.set(next);
        // The terminal path records idle timeouts too; count retried ones here.
        if error.kind == xai_grok_sampler::SamplingErrorKind::IdleTimeout {
            self.signals_handle().record_idle_timeout();
        }
        Some(SamplerFailureRecovery::RetryTransient {
            kind: error.kind,
            status_code: error.status_code,
            grant,
        })
    }

    /// Tell the user about the resubmit, then wait out its backoff.
    pub(super) async fn back_off_transient_retry(
        &self,
        kind: xai_grok_sampler::SamplingErrorKind,
        status_code: Option<u16>,
        grant: TransientRetryGrant,
        loop_index: u32,
    ) {
        let delay = xai_grok_sampler::retry::jittered(grant.delay);
        xai_grok_telemetry::unified_log::warn(
            "shell.turn.transient_retry_backoff",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "loop_index": loop_index,
                "kind": kind.as_str(),
                "status_code": status_code,
                "attempt": grant.attempt,
                "max_retries": grant.max_retries,
                "delay_ms": delay.as_millis() as u64,
            })),
        );
        let cause = match kind {
            xai_grok_sampler::SamplingErrorKind::IdleTimeout => "Response stalled",
            xai_grok_sampler::SamplingErrorKind::Http => "Connection problem",
            _ => "Server error",
        };
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Retrying {
                attempt: grant.attempt,
                max_retries: grant.max_retries,
                reason: format!("{cause}; retrying request"),
            },
        ))
        .await;
        // There is no way to retract the failed attempt's streamed text or
        // pending tool cards from the client, so say that the output above
        // is abandoned and the answer starts again.
        self.send_update(
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(transient_retry_notice(cause, grant)),
            ))),
            None,
        )
        .await;
        tokio::time::sleep(delay).await;
    }

    /// A response arrived: the step budget and episode clock reset.
    pub(super) fn transient_retry_on_response(&self) {
        self.transient_retry
            .set(self.transient_retry.get().on_response());
    }

    /// Compaction rebuilt the request: the step budget resets.
    pub(super) fn transient_retry_on_compaction(&self) {
        self.transient_retry
            .set(self.transient_retry.get().on_compaction());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampler::SamplingErrorKind;

    const IDLE: std::time::Duration = std::time::Duration::from_secs(300);

    fn error(
        kind: SamplingErrorKind,
        status_code: Option<u16>,
    ) -> xai_grok_sampler::SamplingErrorInfo {
        xai_grok_sampler::SamplingErrorInfo {
            kind,
            status_code,
            message: "inference idle timeout after 300s with no chunks".into(),
            is_retryable: false,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: xai_grok_sampling_types::SentCredential::Unknown,
        }
    }

    fn enabled() -> TransientRetryState {
        TransientRetryState {
            enabled: true,
            ..TransientRetryState::default()
        }
    }

    #[test]
    fn idle_timeout_retries_within_the_step_budget_then_fails() {
        let stall = error(SamplingErrorKind::IdleTimeout, None);
        let mut state = enabled();
        let mut delays = Vec::new();
        for attempt in 1..=MAX_TRANSIENT_TURN_RETRIES {
            let (next, grant) = state
                .try_grant(&stall, IDLE)
                .unwrap_or_else(|| panic!("attempt {attempt} must be granted"));
            assert_eq!(grant.attempt, attempt);
            assert_eq!(grant.max_retries, MAX_TRANSIENT_TURN_RETRIES);
            delays.push(grant.delay.as_secs());
            state = next;
        }
        assert_eq!(delays, vec![2, 10, 30]);
        assert!(
            state.try_grant(&stall, IDLE).is_none(),
            "the fourth stall in one step must fail the turn"
        );
    }

    #[test]
    fn a_response_resets_the_step_budget_but_not_the_prompt_cap() {
        let stall = error(SamplingErrorKind::IdleTimeout, None);
        let mut state = enabled();
        let mut granted = 0;
        // Each step stalls three times, then succeeds.
        for _ in 0..10 {
            for _ in 0..MAX_TRANSIENT_TURN_RETRIES {
                match state.try_grant(&stall, IDLE) {
                    Some((next, _)) => {
                        state = next;
                        granted += 1;
                    }
                    None => break,
                }
            }
            state = state.on_response();
        }
        assert_eq!(granted, MAX_TRANSIENT_RETRIES_PER_PROMPT);
        assert!(state.try_grant(&stall, IDLE).is_none());
        let fresh = state.begin_prompt();
        assert!(fresh.enabled);
        assert!(
            fresh.try_grant(&stall, IDLE).is_some(),
            "a new prompt resets the cap"
        );
    }

    #[test]
    fn the_ceiling_shrinks_when_the_prompt_cap_is_nearly_spent() {
        let stall = error(SamplingErrorKind::IdleTimeout, None);
        let state = TransientRetryState {
            prompt_attempts: MAX_TRANSIENT_RETRIES_PER_PROMPT - 1,
            ..enabled()
        };
        let (_, grant) = state.try_grant(&stall, IDLE).unwrap();
        assert_eq!(grant.attempt, 1);
        assert_eq!(
            grant.max_retries, 1,
            "only one retry is left for the prompt"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_episode_window_bounds_a_long_outage() {
        let stall = error(SamplingErrorKind::IdleTimeout, None);
        let (state, _) = enabled().try_grant(&stall, IDLE).unwrap();
        tokio::time::advance(transient_retry_window(IDLE)).await;
        assert!(state.try_grant(&stall, IDLE).is_none());
        let (state, _) = state.on_response().try_grant(&stall, IDLE).unwrap();
        assert_eq!(state.step_attempts, 1, "a response starts a new episode");
    }

    #[test]
    fn disabled_state_never_grants() {
        let stall = error(SamplingErrorKind::IdleTimeout, None);
        assert!(
            TransientRetryState::default()
                .try_grant(&stall, IDLE)
                .is_none()
        );
        assert!(
            !TransientRetryState::for_session(true).enabled,
            "subagents stay terminal"
        );
    }

    fn error_with(
        kind: SamplingErrorKind,
        status_code: Option<u16>,
        message: &str,
        is_retryable: bool,
    ) -> xai_grok_sampler::SamplingErrorInfo {
        xai_grok_sampler::SamplingErrorInfo {
            message: message.into(),
            is_retryable,
            ..error(kind, status_code)
        }
    }

    #[test]
    fn stalls_are_eligible() {
        assert!(transient_retry_eligible(&error(
            SamplingErrorKind::IdleTimeout,
            None
        )));
    }

    #[test]
    fn a_mid_stream_transport_break_is_eligible() {
        let broken = error_with(
            SamplingErrorKind::Http,
            None,
            "reqwest error stream: error decoding response body: connection reset",
            true,
        );
        assert!(transient_retry_eligible(&broken));
    }

    /// The sampler already spent its ~5.5 minute ladder on these.
    #[test]
    fn errors_the_sampler_already_retried_are_not_eligible() {
        let connect = error_with(
            SamplingErrorKind::Http,
            None,
            "error sending request for url (https://api.example.com/v1/responses)",
            true,
        );
        assert!(!transient_retry_eligible(&connect));
        for status in [500, 502, 503, 529] {
            let http_5xx = error_with(
                SamplingErrorKind::Api,
                Some(status),
                &format!("API error (status {status}): upstream overloaded"),
                true,
            );
            assert!(!transient_retry_eligible(&http_5xx), "{status}");
        }
        let rate_limited = error_with(SamplingErrorKind::RateLimited, Some(429), "slow down", true);
        assert!(!transient_retry_eligible(&rate_limited));
    }

    #[test]
    fn deterministic_transport_errors_are_not_eligible() {
        for message in [
            "reqwest error stream: builder error: relative URL without a base",
            "reqwest error stream: invalid peer certificate: UnknownIssuer",
            "reqwest error stream: dns error: failed to lookup address information",
            "error sending request: tls handshake eof",
        ] {
            let err = error_with(SamplingErrorKind::Http, None, message, false);
            assert!(!transient_retry_eligible(&err), "{message}");
        }
    }

    #[test]
    fn a_mid_stream_server_error_event_is_eligible() {
        // Status-less: a StreamError decoded from an SSE error frame.
        let stream_error = error_with(
            SamplingErrorKind::Api,
            None,
            "stream error (overloaded_error): Overloaded",
            true,
        );
        assert!(transient_retry_eligible(&stream_error));
        // The Messages stream maps an in-stream `error` event to a 500.
        let messages_event = error_with(
            SamplingErrorKind::Api,
            Some(500),
            "API error (status 500): overloaded_error: Overloaded",
            true,
        );
        assert!(transient_retry_eligible(&messages_event));
        // A status-less error that is not retryable is a config problem.
        let config = error_with(SamplingErrorKind::Api, None, "missing project", false);
        assert!(!transient_retry_eligible(&config));
    }

    #[test]
    fn client_errors_and_other_kinds_are_not_eligible() {
        for status in [400, 401, 403, 404, 422] {
            let err = error_with(
                SamplingErrorKind::Api,
                Some(status),
                &format!("API error (status {status}): bad request"),
                false,
            );
            assert!(!transient_retry_eligible(&err), "{status}");
        }
        for kind in [
            SamplingErrorKind::Auth,
            SamplingErrorKind::Serialization,
            SamplingErrorKind::RateLimited,
            SamplingErrorKind::EmptyResponse,
            SamplingErrorKind::MaxTokensTruncation,
            SamplingErrorKind::DoomLoopDetected,
        ] {
            assert!(
                !transient_retry_eligible(&error(kind, Some(500))),
                "{kind:?}"
            );
        }
        let mut vetoed = error(SamplingErrorKind::IdleTimeout, None);
        vetoed.should_retry = Some(false);
        assert!(!transient_retry_eligible(&vetoed));
        let overflow = error_with(
            SamplingErrorKind::Api,
            None,
            "The prompt is too long for this model's context window.",
            true,
        );
        assert!(!transient_retry_eligible(&overflow));
        let mut image = error_with(SamplingErrorKind::Api, None, "bad image", true);
        image.error_code = Some(xai_grok_sampling_types::ApiErrorCode::InvalidImage);
        assert!(!transient_retry_eligible(&image));
    }

    #[test]
    fn the_retry_notice_names_the_attempt() {
        let grant = TransientRetryGrant {
            attempt: 2,
            max_retries: 3,
            delay: std::time::Duration::from_secs(10),
        };
        let notice = transient_retry_notice("Response stalled", grant);
        assert!(notice.contains("Response stalled. Retrying the request (attempt 2 of 3)"));
        assert!(notice.starts_with("\n\n") && notice.ends_with("\n\n"));
    }

    /// End to end through `handle_sampling_failure`: an idle timeout is
    /// retried while budget remains and fails the turn once it runs out.
    #[tokio::test(flavor = "current_thread")]
    async fn handle_sampling_failure_retries_idle_timeout_then_fails() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = super::super::support::create_test_actor(
                    1_000,
                    200_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                )
                .await;
                actor.transient_retry.set(enabled());
                let actor = std::sync::Arc::new(actor);
                let stall = error(SamplingErrorKind::IdleTimeout, None);
                for attempt in 1..=MAX_TRANSIENT_TURN_RETRIES {
                    match actor.handle_sampling_failure(stall.clone()).await {
                        Ok(SamplerFailureRecovery::RetryTransient { kind, grant, .. }) => {
                            assert_eq!(kind, SamplingErrorKind::IdleTimeout);
                            assert_eq!(grant.attempt, attempt);
                        }
                        Ok(_) => panic!("attempt {attempt}: expected RetryTransient"),
                        Err(e) => panic!("attempt {attempt}: expected a retry, got {e:?}"),
                    }
                }
                assert!(
                    actor.handle_sampling_failure(stall).await.is_err(),
                    "an exhausted budget must fail the turn"
                );
            })
            .await;
    }
}
