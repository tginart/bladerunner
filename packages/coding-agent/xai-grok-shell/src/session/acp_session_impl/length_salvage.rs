//! Length salvage: what the turn loop does when a response stops with
//! `StopReason::Length` (max_tokens truncation).
//!
//! Purpose: This module keeps a cut-off answer from failing the turn.
//! Method:
//!     1. A text-only `Length` stop continues: the turn pushes a continue
//!        reminder and samples again, up to a per-prompt budget.
//!     2. After the budget, the turn completes with the truncated answer.
//!     3. A `Length` stop with complete tool calls runs the calls. A streak
//!        cap stops a loop that does not converge.
//! Note: The sampler-side gate is `LengthPolicy`. Provider-agnostic. The
//! state lives in a `SessionActor` cell and resets once per prompt, because
//! stop-hook, goal-loop and recovery rounds enter the turn loop again.

use super::*;

/// Continues per prompt for text-only `Length` stops.
const DEFAULT_LENGTH_CONTINUE_BUDGET: u32 = 2;

/// Debug override: `GROK_LENGTH_SALVAGE=0` turns continuation off.
const LENGTH_SALVAGE_ENV: &str = "GROK_LENGTH_SALVAGE";

/// Consecutive `Length`-salvaged tool-call samples allowed per prompt.
pub(super) const MAX_OUTPUT_TOKEN_LIMIT_RETRIES: u32 = 5;

/// Pushed before every continuation, so each continuation request ends with
/// a user message. A request that ends with an assistant message is a
/// prefill on Anthropic-style backends, which can reject it with a 400. The
/// trailing clause keeps a stranded copy from hijacking the user's next
/// prompt.
pub(super) const LENGTH_CONTINUE_REMINDER_BODY: &str = "Your previous response exceeded the \
     output token limit and was cut off. Continue from exactly where it stopped — or if a \
     newer user message follows this note, answer that instead.";

/// Injected once per tool-call salvage streak so the model knows its output
/// was cut mid-step.
pub(super) const OUTPUT_TOKEN_LIMIT_REMINDER: &str = "Your response was cut off because it \
     exceeded the output token limit. Please break your work into smaller pieces. Continue \
     from where you left off.";

/// Pure form of the budget choice. Structured output turns do not continue:
/// two joined fragments do not validate as one document, so they keep the
/// old hard failure.
pub(super) fn resolve_length_salvage_budget(
    env: Option<bool>,
    has_json_schema: bool,
) -> Option<u32> {
    if env == Some(false) || has_json_schema {
        return None;
    }
    Some(DEFAULT_LENGTH_CONTINUE_BUDGET)
}

/// Per-prompt length-salvage state, held in `SessionActor::length_salvage`.
/// The default (no budget) keeps the hard failure until a prompt starts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PromptLengthSalvage {
    salvage: LengthSalvage,
    streak: LengthSalvageStreak,
}

impl SessionActor {
    /// Reset the length-salvage state at the start of a prompt.
    pub(super) fn begin_prompt_length_salvage(&self, has_json_schema: bool) {
        let budget = resolve_length_salvage_budget(
            xai_grok_config::env_bool(LENGTH_SALVAGE_ENV),
            has_json_schema,
        );
        self.length_salvage.set(PromptLengthSalvage {
            salvage: LengthSalvage::new(budget),
            streak: LengthSalvageStreak::default(),
        });
    }

    /// The policy the main turn asks the sampler for. `CompletePartial`
    /// delivers cut text so the turn can continue it; otherwise only
    /// complete tool calls survive a `Length` stop.
    pub(super) fn turn_length_policy(&self) -> xai_grok_sampling_types::LengthPolicy {
        if self.length_salvage.get().salvage.enabled() {
            xai_grok_sampling_types::LengthPolicy::CompletePartial
        } else {
            xai_grok_sampling_types::LengthPolicy::CompleteToolCalls
        }
    }

    /// True when the latest answer is known to be cut off.
    pub(super) fn length_salvage_is_truncated(&self) -> bool {
        self.length_salvage.get().salvage.is_truncated()
    }

    /// A tool step ended the current answer run.
    pub(super) fn length_salvage_step_boundary(&self) {
        let mut state = self.length_salvage.get();
        state.salvage.step_boundary();
        self.length_salvage.set(state);
    }

    /// Purpose: Advance the tool-call salvage streak for one completed sample.
    /// Input: Whether the sample is a `Length` stop with tool calls.
    /// Output: True when the streak passed its cap and the turn must fail.
    /// Note: On the first salvage of a streak, this pushes the reminder. The
    /// caller runs it before it records the assistant item, so the model sees
    /// the cut before its own salvaged calls. Kept out of line so the turn
    /// loop's poll frame does not grow.
    #[inline(never)]
    pub(super) fn observe_length_salvage_streak(&self, is_length_with_tool_calls: bool) -> bool {
        let mut state = self.length_salvage.get();
        let action = state.streak.on_sample(is_length_with_tool_calls);
        self.length_salvage.set(state);
        match action {
            LengthSalvageAction::Exhausted => {
                tracing::error!(
                    session_id = %self.session_info.id,
                    max = MAX_OUTPUT_TOKEN_LIMIT_RETRIES,
                    "consecutive Length-salvaged tool-call samples hit the cap; failing the turn"
                );
                true
            }
            LengthSalvageAction::Proceed { inject_reminder } => {
                if inject_reminder {
                    self.push_system_reminder(OUTPUT_TOKEN_LIMIT_REMINDER);
                }
                false
            }
            LengthSalvageAction::NotSalvage => false,
        }
    }

    /// Purpose: Apply the salvage state machine to a text-only `Length` stop.
    /// Output: True when the turn must sample again (a continue). False when
    /// the turn completes with the truncated answer.
    /// Note: Every continue pushes the reminder after the cut assistant
    /// item, so the continuation request ends with a user message on every
    /// backend. Kept out of line so the turn loop's poll frame does not grow.
    #[inline(never)]
    pub(super) fn advance_length_salvage(&self) -> bool {
        let mut state = self.length_salvage.get();
        let step = state.salvage.on_length_stop();
        self.length_salvage.set(state);
        let salvage = state.salvage;
        let details = Some(serde_json::json!({
            "continue_attempts": salvage.continues(),
            "continue_budget": salvage.budget(),
        }));
        match step {
            SalvageStep::Continue => {
                self.push_system_reminder(LENGTH_CONTINUE_REMINDER_BODY);
                tracing::warn!(
                    session_id = %self.session_info.id,
                    retry = salvage.continues(),
                    max = salvage.budget(),
                    "output token limit hit; asking the model to continue"
                );
                xai_grok_telemetry::unified_log::warn(
                    "shell.turn.length_truncation_continue",
                    Some(self.session_info.id.0.as_ref()),
                    details,
                );
                true
            }
            SalvageStep::Exhaust => {
                tracing::error!(
                    session_id = %self.session_info.id,
                    retries = salvage.continues(),
                    "output token limit continues exhausted; completing the turn truncated"
                );
                xai_grok_telemetry::unified_log::warn(
                    "shell.turn.length_truncation_exhausted",
                    Some(self.session_info.id.0.as_ref()),
                    details,
                );
                false
            }
            SalvageStep::None => false,
        }
    }

    /// Terminal failure when the tool-call salvage streak passes its cap:
    /// running more calls does not converge, so surface the old
    /// `MaxTokensTruncation` failure. The sampler emitted only `Completed`
    /// events for these samples, so the error signal is recorded here.
    pub(crate) async fn fail_turn_length_salvage_exhausted(&self) -> acp::Error {
        let kind = xai_grok_sampler::SamplingErrorKind::MaxTokensTruncation;
        let message = xai_grok_sampling_types::SamplingError::MaxTokensTruncation.to_string();
        self.signals_handle().record_error_typed(kind.as_str());
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: kind.as_str().to_owned(),
                message: message.clone(),
            },
        ))
        .await;
        acp::Error::internal_error().data(crate::sampling::error::terminal_error_data(
            message, None, kind,
        ))
    }
}

/// The turn loop's next step for a text-only `Length` stop.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SalvageStep {
    /// Push the continue reminder and sample again.
    Continue,
    /// The budget just ran out: log once, then complete the turn truncated.
    Exhaust,
    /// Only mark the turn truncated (already exhausted, or salvage is off).
    None,
}

/// Per-prompt state for text-only `Length` stops.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LengthSalvage {
    budget: Option<u32>,
    continues: u32,
    /// The latest answer is cut off; the TodoGate stays out of the way.
    truncated: bool,
    /// The exhaustion log fires once per prompt.
    exhaustion_reported: bool,
}

impl LengthSalvage {
    pub(super) fn new(budget: Option<u32>) -> Self {
        Self {
            budget: budget.filter(|b| *b > 0),
            ..Self::default()
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.budget.is_some()
    }

    pub(super) fn budget(&self) -> u32 {
        self.budget.unwrap_or(0)
    }

    pub(super) fn continues(&self) -> u32 {
        self.continues
    }

    pub(super) fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// A tool step ended the current answer run, so the cut is resolved.
    pub(super) fn step_boundary(&mut self) {
        self.truncated = false;
    }

    /// Advance the state machine for a text-only `Length` stop.
    pub(super) fn on_length_stop(&mut self) -> SalvageStep {
        if self.continues < self.budget() {
            self.continues += 1;
            return SalvageStep::Continue;
        }
        // A Length stop that leaks through with salvage off is not an exhaustion.
        let report_exhaustion = !self.exhaustion_reported && self.enabled();
        self.truncated = true;
        self.exhaustion_reported = true;
        if report_exhaustion {
            SalvageStep::Exhaust
        } else {
            SalvageStep::None
        }
    }
}

/// Consecutive `Length`-salvaged tool-call samples within one prompt. Each
/// salvage grows the context, so an unbounded streak could loop forever once
/// compaction stops freeing room.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LengthSalvageStreak {
    consecutive: u32,
}

/// What the turn loop does with a completed sample, per the salvage streak.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LengthSalvageAction {
    /// Not a `Length`-salvaged tool-call sample; the streak resets.
    NotSalvage,
    /// Run the salvaged calls. `inject_reminder` is set on the first sample
    /// of a streak.
    Proceed { inject_reminder: bool },
    /// The streak passed [`MAX_OUTPUT_TOKEN_LIMIT_RETRIES`]; fail the turn.
    Exhausted,
}

impl LengthSalvageStreak {
    pub(super) fn on_sample(&mut self, is_length_with_tool_calls: bool) -> LengthSalvageAction {
        if !is_length_with_tool_calls {
            self.consecutive = 0;
            return LengthSalvageAction::NotSalvage;
        }
        self.consecutive += 1;
        if self.consecutive > MAX_OUTPUT_TOKEN_LIMIT_RETRIES {
            return LengthSalvageAction::Exhausted;
        }
        LengthSalvageAction::Proceed {
            inject_reminder: self.consecutive == 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_on_by_default_and_off_by_env_or_schema() {
        assert_eq!(
            resolve_length_salvage_budget(None, false),
            Some(DEFAULT_LENGTH_CONTINUE_BUDGET)
        );
        assert_eq!(
            resolve_length_salvage_budget(Some(true), false),
            Some(DEFAULT_LENGTH_CONTINUE_BUDGET)
        );
        assert_eq!(resolve_length_salvage_budget(Some(false), false), None);
        assert_eq!(resolve_length_salvage_budget(None, true), None);
    }

    /// A length-truncated text response continues until the budget runs out,
    /// then the turn completes truncated.
    #[test]
    fn length_truncated_response_continues_then_exhausts_once() {
        let mut s = LengthSalvage::new(Some(2));
        assert_eq!(s.on_length_stop(), SalvageStep::Continue);
        assert_eq!(s.on_length_stop(), SalvageStep::Continue);
        assert!(!s.is_truncated());
        assert_eq!(s.on_length_stop(), SalvageStep::Exhaust);
        assert!(s.is_truncated());
        assert_eq!(s.on_length_stop(), SalvageStep::None);
        assert_eq!(s.continues(), 2);
    }

    #[test]
    fn step_boundary_clears_the_mark_but_not_the_budget() {
        let mut s = LengthSalvage::new(Some(1));
        assert_eq!(s.on_length_stop(), SalvageStep::Continue);
        assert_eq!(s.on_length_stop(), SalvageStep::Exhaust);
        s.step_boundary();
        assert!(!s.is_truncated());
        assert_eq!(
            s.on_length_stop(),
            SalvageStep::None,
            "the budget stays spent"
        );
    }

    #[test]
    fn disabled_or_zero_budget_never_continues() {
        for budget in [None, Some(0)] {
            let mut s = LengthSalvage::new(budget);
            assert!(!s.enabled());
            assert_eq!(s.on_length_stop(), SalvageStep::None);
            assert!(s.is_truncated());
        }
    }

    /// A complete truncated tool call runs; the streak caps a non-converging
    /// loop and resets on a normal sample.
    #[test]
    fn tool_call_salvage_streak_runs_calls_then_caps() {
        let mut streak = LengthSalvageStreak::default();
        assert_eq!(
            streak.on_sample(true),
            LengthSalvageAction::Proceed {
                inject_reminder: true
            }
        );
        for _ in 1..MAX_OUTPUT_TOKEN_LIMIT_RETRIES {
            assert_eq!(
                streak.on_sample(true),
                LengthSalvageAction::Proceed {
                    inject_reminder: false
                }
            );
        }
        assert_eq!(streak.on_sample(true), LengthSalvageAction::Exhausted);
        assert_eq!(streak.on_sample(false), LengthSalvageAction::NotSalvage);
        assert_eq!(
            streak.on_sample(true),
            LengthSalvageAction::Proceed {
                inject_reminder: true
            }
        );
    }

    /// Continues 1 and 2 both send a request that ends with a user message,
    /// never with the cut assistant message (an Anthropic prefill).
    #[tokio::test(flavor = "current_thread")]
    async fn every_continuation_request_ends_with_a_user_message() {
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
                actor.begin_prompt_length_salvage(false);
                assert_eq!(
                    actor.turn_length_policy(),
                    xai_grok_sampling_types::LengthPolicy::CompletePartial
                );
                actor
                    .chat_state_handle
                    .push_user_message(ConversationItem::user("write a long essay"));
                for continuation in 1..=DEFAULT_LENGTH_CONTINUE_BUDGET {
                    actor
                        .chat_state_handle
                        .push_assistant_response(ConversationItem::assistant(format!(
                            "part {continuation}"
                        )));
                    assert!(actor.advance_length_salvage(), "continue {continuation}");
                    let request = actor
                        .chat_state_handle
                        .build_request(
                            Vec::new(),
                            None,
                            false,
                            None,
                            actor.session_id_string(),
                            format!("req-continue-{continuation}"),
                        )
                        .await
                        .expect("chat state alive");
                    let last = request.items.last().expect("request has items");
                    assert!(
                        matches!(last, ConversationItem::User(_)),
                        "continue {continuation} must end with a user message, got {last:?}"
                    );
                }
                assert!(
                    !actor.advance_length_salvage(),
                    "the third cut completes the turn truncated"
                );
                assert!(actor.length_salvage_is_truncated());
            })
            .await;
    }
}
