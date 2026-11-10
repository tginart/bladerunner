//! Length salvage: what the turn loop does when a response stops with
//! `StopReason::Length` (max_tokens truncation).
//!
//! Purpose: This module keeps a cut-off answer from failing the turn.
//! Method:
//!     1. A text-only `Length` stop continues: the turn pushes a reminder once
//!        per run and samples again, up to a per-prompt budget.
//!     2. After the budget, the turn completes with the truncated answer.
//!     3. A `Length` stop with complete tool calls runs the calls. A streak
//!        cap stops a loop that does not converge.
//! Note: The sampler-side gate is `LengthPolicy`. Provider-agnostic.

use super::*;

/// Continues per prompt for text-only `Length` stops.
const DEFAULT_LENGTH_CONTINUE_BUDGET: u32 = 2;

/// Debug override: `GROK_LENGTH_SALVAGE=0` turns continuation off.
const LENGTH_SALVAGE_ENV: &str = "GROK_LENGTH_SALVAGE";

/// Consecutive `Length`-salvaged tool-call samples allowed per prompt.
pub(super) const MAX_OUTPUT_TOKEN_LIMIT_RETRIES: u32 = 5;

/// Injected once per continuation run. The trailing clause keeps a stranded
/// copy from hijacking the user's next prompt.
pub(super) const LENGTH_CONTINUE_REMINDER_BODY: &str = "Your previous response exceeded the \
     output token limit and was cut off. Continue from exactly where it stopped — or if a \
     newer user message follows this note, answer that instead.";

/// Injected once per tool-call salvage streak so the model knows its output
/// was cut mid-step.
pub(super) const OUTPUT_TOKEN_LIMIT_REMINDER: &str = "Your response was cut off because it \
     exceeded the output token limit. Please break your work into smaller pieces. Continue \
     from where you left off.";

/// Pure form of [`SessionActor::length_salvage_budget`]. Structured output
/// turns do not continue: two joined fragments do not validate as one
/// document, so they keep the old hard failure.
pub(super) fn resolve_length_salvage_budget(
    env: Option<bool>,
    has_json_schema: bool,
) -> Option<u32> {
    if env == Some(false) || has_json_schema {
        return None;
    }
    Some(DEFAULT_LENGTH_CONTINUE_BUDGET)
}

impl SessionActor {
    /// `Some(budget)` continues text-only `Length` stops; `None` keeps the
    /// sampler's hard `MaxTokensTruncation` failure.
    pub(super) fn length_salvage_budget(&self, has_json_schema: bool) -> Option<u32> {
        resolve_length_salvage_budget(
            xai_grok_config::env_bool(LENGTH_SALVAGE_ENV),
            has_json_schema,
        )
    }

    /// Purpose: Advance the tool-call salvage streak for one completed sample.
    /// Input: The streak. Whether the sample is a `Length` stop with tool calls.
    /// Output: True when the streak passed its cap and the turn must fail.
    /// Note: On the first salvage of a streak, this pushes the reminder. The
    /// caller runs it before it records the assistant item, so the model sees
    /// the cut before its own salvaged calls. Kept out of line so the turn
    /// loop's poll frame does not grow.
    #[inline(never)]
    pub(super) fn observe_length_salvage_streak(
        &self,
        streak: &mut LengthSalvageStreak,
        is_length_with_tool_calls: bool,
    ) -> bool {
        match streak.on_sample(is_length_with_tool_calls) {
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
    /// Input: The per-prompt salvage state.
    /// Output: True when the turn must sample again (a continue). False when
    /// the turn completes with the truncated answer.
    /// Note: A continue pushes the reminder once per run. Kept out of line so
    /// the turn loop's poll frame does not grow.
    #[inline(never)]
    pub(super) fn advance_length_salvage(&self, salvage: &mut LengthSalvage) -> bool {
        let step = salvage.on_length_stop();
        let details = Some(serde_json::json!({
            "continue_attempts": salvage.continues(),
            "continue_budget": salvage.budget(),
        }));
        match step {
            SalvageStep::Continue { inject_reminder } => {
                if inject_reminder {
                    self.push_system_reminder(LENGTH_CONTINUE_REMINDER_BODY);
                }
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
    /// Sample again; push the reminder first when `inject_reminder` is set.
    Continue { inject_reminder: bool },
    /// The budget just ran out: log once, then complete the turn truncated.
    Exhaust,
    /// Only mark the turn truncated (already exhausted, or salvage is off).
    None,
}

/// Per-prompt state for text-only `Length` stops.
pub(super) struct LengthSalvage {
    budget: Option<u32>,
    continues: u32,
    /// Set while the next continue should push the reminder. A tool step
    /// sets it again, so a second truncation run gets its own cue.
    reminder_armed: bool,
    /// The latest answer is cut off; the TodoGate stays out of the way.
    truncated: bool,
    /// The exhaustion log fires once per prompt.
    exhaustion_reported: bool,
}

impl LengthSalvage {
    pub(super) fn new(budget: Option<u32>) -> Self {
        Self {
            budget: budget.filter(|b| *b > 0),
            continues: 0,
            reminder_armed: true,
            truncated: false,
            exhaustion_reported: false,
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

    /// A tool step ended the current answer run.
    pub(super) fn step_boundary(&mut self) {
        self.reminder_armed = true;
        self.truncated = false;
    }

    /// Advance the state machine for a text-only `Length` stop.
    pub(super) fn on_length_stop(&mut self) -> SalvageStep {
        if self.continues < self.budget() {
            self.continues += 1;
            let inject_reminder = self.reminder_armed;
            self.reminder_armed = false;
            return SalvageStep::Continue { inject_reminder };
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
#[derive(Default)]
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
        assert_eq!(
            s.on_length_stop(),
            SalvageStep::Continue {
                inject_reminder: true
            }
        );
        assert_eq!(
            s.on_length_stop(),
            SalvageStep::Continue {
                inject_reminder: false
            }
        );
        assert!(!s.is_truncated());
        assert_eq!(s.on_length_stop(), SalvageStep::Exhaust);
        assert!(s.is_truncated());
        assert_eq!(s.on_length_stop(), SalvageStep::None);
        assert_eq!(s.continues(), 2);
    }

    #[test]
    fn step_boundary_rearms_the_reminder_but_not_the_budget() {
        let mut s = LengthSalvage::new(Some(3));
        assert!(matches!(s.on_length_stop(), SalvageStep::Continue { .. }));
        s.step_boundary();
        assert_eq!(
            s.on_length_stop(),
            SalvageStep::Continue {
                inject_reminder: true
            },
            "a second truncation run gets its own reminder"
        );
        assert_eq!(s.continues(), 2);
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
}
