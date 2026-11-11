//! Purpose: Decide if a copied child transcript can resume in the child's
//! context window.
//!
//! Spawn adds the system prompt, AGENTS.md and the resume task before the
//! first compaction can run, so a copy that fills the window is not safe.
//! The old guard refused every copy above 80% of the window, which also
//! refused transcripts that had ample headroom on a large model. This
//! policy admits a copy up to 95% of the window, but never at or above the
//! auto-compact threshold.
//!
//! Note: A copy under the threshold needs no compaction before the first
//! turn. A child with an output token budget never auto-compacts, so this
//! rule is also safe for it.

/// Highest share of the window that a copied transcript can use. The rest
/// holds the spawn prelude.
const RESUME_WINDOW_FIT_PERCENT: u8 = 95;

/// Purpose: The child context window and auto-compact threshold that decide
/// resume admission.
#[derive(Debug, Clone, Copy)]
pub(super) struct ResumeWindowPolicy {
    pub context_window: u64,
    pub auto_compact_threshold_percent: u8,
}

impl ResumeWindowPolicy {
    /// The token count at which a copied transcript stops fitting:
    /// 95% of the window, or the auto-compact threshold if that is lower.
    pub(super) fn token_limit(&self) -> u64 {
        let fit_percent = RESUME_WINDOW_FIT_PERCENT.min(self.auto_compact_threshold_percent);
        self.context_window * u64::from(fit_percent) / 100
    }

    /// Purpose: Tell if a transcript of `estimated_tokens` can resume.
    /// Output: `false` for an unknown (zero) window, for an estimate above
    /// [`Self::token_limit`], and for an estimate at or above the
    /// auto-compact threshold.
    pub(super) fn fits(&self, estimated_tokens: u64) -> bool {
        self.context_window != 0
            && estimated_tokens <= self.token_limit()
            && !xai_token_estimation::exceeds_threshold(
                estimated_tokens,
                self.context_window,
                self.auto_compact_threshold_percent,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(threshold_percent: u8) -> ResumeWindowPolicy {
        ResumeWindowPolicy {
            context_window: 100_000,
            auto_compact_threshold_percent: threshold_percent,
        }
    }

    #[test]
    fn limit_is_95_percent_capped_at_the_threshold() {
        assert_eq!(policy(100).token_limit(), 95_000);
        assert_eq!(policy(85).token_limit(), 85_000);
    }

    /// The old 80% guard refused this transcript; it now resumes.
    #[test]
    fn a_transcript_above_80_percent_resumes_when_it_has_headroom() {
        assert!(policy(100).fits(94_000));
        assert!(policy(90).fits(89_999));
    }

    #[test]
    fn a_transcript_at_the_threshold_or_over_the_limit_is_refused() {
        assert!(!policy(90).fits(90_000));
        assert!(!policy(100).fits(95_001));
    }

    #[test]
    fn an_unknown_window_is_refused() {
        let unknown = ResumeWindowPolicy {
            context_window: 0,
            auto_compact_threshold_percent: 90,
        };
        assert!(!unknown.fits(1));
    }
}
