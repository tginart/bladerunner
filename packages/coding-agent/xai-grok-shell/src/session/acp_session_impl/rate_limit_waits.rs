//! Purpose: This module paces the model requests of subagent sessions.
//!
//! A subagent child shares the rate limit of its provider with its parent and
//! its siblings. When the sampler gives up on a 429, a main session shows the
//! error to the user. A child has no user to retry for it, so the error used
//! to fail the whole child. This module lets a child wait and resubmit.
//!
//! The module also holds the process-wide sampling gate. The gate is a
//! semaphore that limits how many subagent requests are in flight at the
//! same time, so a wide fan-out does not cause a burst of 429s.
//!
//! The logic is provider-generic: it reads only `SamplingErrorKind`, the
//! `Retry-After` value, and the `x-should-retry` veto that the sampler parsed.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use xai_grok_sampler::{SamplingErrorInfo, SamplingErrorKind};

use super::{SamplerFailureRecovery, SessionActor};
use crate::extensions::notification::{RetryState, SessionUpdate as XaiSessionUpdate};

/// Most 429 waits that one child turn can do. With the sampler backoff
/// (2 s doubled per attempt, capped at 30 s) eight waits use about the
/// full [`MAX_TOTAL_WAIT_PER_TURN`].
pub(crate) const MAX_WAITS_PER_TURN: u32 = 8;

/// Most time that one child turn can spend in 429 waits, summed over waits.
pub(crate) const MAX_TOTAL_WAIT_PER_TURN: Duration = Duration::from_secs(150);

/// Env var that lowers the number of concurrent subagent model requests.
pub(crate) const SAMPLING_LIMIT_ENV: &str = "GROK_SUBAGENT_SAMPLING_LIMIT";

/// Ceiling for the sampling limit. `Semaphore::new` panics above
/// `Semaphore::MAX_PERMITS`, and no real fan-out needs more than this.
pub(crate) const MAX_SAMPLING_LIMIT: usize = 512;

/// Purpose: The sampling policy of one subagent child session.
/// Note: Clones share the gate and the wait budget. Every clone of the
/// child's `ToolContext` thus sees the same budget.
#[derive(Clone)]
pub(crate) struct SubagentSampling {
    gate: Arc<Semaphore>,
    budget: Arc<parking_lot::Mutex<RateLimitWaitBudget>>,
}

impl SubagentSampling {
    pub(crate) fn new(gate: Arc<Semaphore>) -> Self {
        Self {
            gate,
            budget: Arc::default(),
        }
    }
}

/// The result of one 429 in a child turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateLimitWaitDecision {
    /// Wait for `backoff`, then resubmit. `attempt` starts at 1 in each turn.
    Wait { attempt: u32, backoff: Duration },
    /// The turn used all [`MAX_WAITS_PER_TURN`] waits.
    AttemptsSpent { attempts: u32 },
    /// The next wait would go past [`MAX_TOTAL_WAIT_PER_TURN`].
    TotalWaitSpent { attempts: u32 },
}

/// Purpose: The 429 wait budget of one child session, reset for each turn.
#[derive(Debug, Default)]
pub(crate) struct RateLimitWaitBudget {
    turn_idx: Option<usize>,
    attempts: u32,
    total_waited: Duration,
}

impl RateLimitWaitBudget {
    /// Purpose: Decide if the child waits for a 429 in turn `turn_idx`.
    /// Input: The turn index and the planned backoff for each attempt.
    /// Output: A [`RateLimitWaitDecision`]. A `Wait` decision is charged.
    /// Method:
    ///     1. If `turn_idx` is a new turn, clear the budget.
    ///     2. If all attempts are used, stop.
    ///     3. If the next backoff goes past the total budget, stop.
    ///     4. If not, charge the attempt and its backoff.
    /// Note: The budget stops instead of a short wait. A short wait would
    /// resubmit before the provider window clears.
    pub(crate) fn decide(
        &mut self,
        turn_idx: usize,
        backoff_for_attempt: impl FnOnce(u32) -> Duration,
    ) -> RateLimitWaitDecision {
        if self.turn_idx != Some(turn_idx) {
            *self = Self {
                turn_idx: Some(turn_idx),
                ..Self::default()
            };
        }
        if self.attempts >= MAX_WAITS_PER_TURN {
            return RateLimitWaitDecision::AttemptsSpent {
                attempts: self.attempts,
            };
        }
        let attempt = self.attempts + 1;
        let backoff = backoff_for_attempt(attempt);
        if self.total_waited + backoff > MAX_TOTAL_WAIT_PER_TURN {
            return RateLimitWaitDecision::TotalWaitSpent {
                attempts: self.attempts,
            };
        }
        self.attempts = attempt;
        self.total_waited += backoff;
        RateLimitWaitDecision::Wait { attempt, backoff }
    }
}

/// Purpose: Give the backoff before 429 wait number `attempt`.
/// Input: The 1-based attempt and the `Retry-After` seconds, if any.
/// Output: The provider's `Retry-After` when it is positive, else the
/// sampler's jittered exponential backoff. Both are capped at 30 s.
pub(crate) fn rate_limit_backoff(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    match retry_after_secs.filter(|secs| *secs > 0) {
        Some(secs) => Duration::from_secs(secs).min(xai_grok_sampler::retry::MAX_RETRY_BACKOFF),
        None => xai_grok_sampler::retry::retry_backoff_with_jitter(attempt),
    }
}

/// Purpose: Resolve the number of concurrent subagent model requests.
/// Input: The `GROK_SUBAGENT_SAMPLING_LIMIT` value and the resolved
/// concurrent-subagent limit.
/// Output: The env value when it is a positive integer, else
/// `max_concurrent`. The result is in `1..=MAX_SAMPLING_LIMIT`.
pub(crate) fn resolve_sampling_limit(env_value: Option<&str>, max_concurrent: usize) -> usize {
    let from_env = env_value.and_then(|raw| match raw.trim().parse::<usize>() {
        Ok(limit) if limit > 0 => Some(limit),
        _ => {
            tracing::warn!(
                name = SAMPLING_LIMIT_ENV,
                value = raw,
                "ignoring a sampling limit that is not a positive integer"
            );
            None
        }
    });
    from_env
        .unwrap_or(max_concurrent)
        .clamp(1, MAX_SAMPLING_LIMIT)
}

/// The process-wide subagent sampling gate, sized from the env and config.
pub(crate) fn new_subagent_sampling_gate(max_concurrent: usize) -> Arc<Semaphore> {
    let env_value = std::env::var(SAMPLING_LIMIT_ENV).ok();
    Arc::new(Semaphore::new(resolve_sampling_limit(
        env_value.as_deref(),
        max_concurrent,
    )))
}

impl SessionActor {
    /// Purpose: Hold a subagent sampling slot while one model request runs.
    /// Output: `None` for a main session, which is not gated.
    /// Note: The gate is never closed, so `None` from `acquire_owned`
    /// cannot occur. The fallback is an ungated request, not a failure.
    pub(crate) async fn acquire_subagent_sampling_permit(&self) -> Option<OwnedSemaphorePermit> {
        let sampling = self.tool_context.subagent_sampling.as_ref()?;
        sampling.gate.clone().acquire_owned().await.ok()
    }

    /// Purpose: Wait out a 429 in a subagent turn, then ask for a resubmit.
    /// Input: The sampling failure that the sampler gave up on.
    /// Output: `Some(CompactAndResubmit)` after the wait. `None` when the
    /// failure is not a 429, the server sent `x-should-retry: false`, the
    /// session is not a subagent, or the turn budget is spent. The caller
    /// then fails the turn as before.
    /// Method:
    ///     1. Charge the wait against the budget of the current turn.
    ///     2. Tell the client that the child is in a retry wait.
    ///     3. Sleep for the backoff.
    /// Note: The turn loop treats `CompactAndResubmit` as "rebuild the
    /// request and send it again", which is the resubmit a 429 needs.
    /// A cancel aborts the turn task, so the sleep is a cancel point.
    pub(crate) async fn wait_out_subagent_rate_limit(
        &self,
        error: &SamplingErrorInfo,
    ) -> Option<SamplerFailureRecovery> {
        if !matches!(error.kind, SamplingErrorKind::RateLimited) {
            return None;
        }
        // `x-should-retry: false` is a server veto. The sampler treats it as
        // fatal, so the child must not resubmit or charge the budget.
        if error.should_retry == Some(false) {
            return None;
        }
        let sampling = self.tool_context.subagent_sampling.as_ref()?;
        let turn_idx = self.chat_state_handle.get_prompt_index().await;
        let decision = sampling.budget.lock().decide(turn_idx, |attempt| {
            rate_limit_backoff(attempt, error.retry_after_secs)
        });
        let RateLimitWaitDecision::Wait { attempt, backoff } = decision else {
            tracing::warn!(
                session_id = %self.session_info.id,
                ?decision,
                "subagent rate-limit wait budget spent; failing the turn"
            );
            return None;
        };
        tracing::info!(
            session_id = %self.session_info.id,
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            "subagent rate limited; waiting before resubmit"
        );
        self.send_xai_notification(XaiSessionUpdate::RetryState(RetryState::Retrying {
            attempt,
            max_retries: MAX_WAITS_PER_TURN,
            reason: format!(
                "Rate limited; waiting {}s before retrying",
                backoff.as_secs().max(1)
            ),
        }))
        .await;
        tokio::time::sleep(backoff).await;
        Some(SamplerFailureRecovery::CompactAndResubmit)
    }
}

#[cfg(test)]
#[path = "rate_limit_waits_tests.rs"]
mod tests;
