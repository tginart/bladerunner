//! Carry the chosen reasoning effort into a new session.
//!
//! `/new` and `/clear` open a session without an effort in `_meta`. The
//! process-wide current effort (the last `/effort` choice, or the
//! `[models].default_reasoning_effort` seed) must reach the session actor.
//! If it does not, the fresh chat samples at the catalog default of the model
//! and the effort picker snaps back to that default.

use agent_client_protocol as acp;
use xai_grok_sampler::SamplerConfig as SamplingConfig;
use xai_grok_sampling_types::{
    REASONING_EFFORT_META_KEY, ReasoningEffort, reasoning_effort_meta_value,
};

use crate::agent::models::ModelsManager;

impl ModelsManager {
    /// Put `effort` on `sampling` when the model of `sampling` supports
    /// reasoning effort. A `None` effort or an unsupported model leaves the
    /// catalog default in place. Returns `true` when the effort was applied.
    pub(crate) fn apply_supported_effort(
        &self,
        sampling: &mut SamplingConfig,
        effort: Option<ReasoningEffort>,
    ) -> bool {
        let Some(effort) = effort else {
            return false;
        };
        if !self.model_supports_reasoning_effort(&sampling.model) {
            return false;
        }
        sampling.reasoning_effort = Some(effort);
        true
    }
}

/// `_meta` that asks `session/set_model` for `effort`, or `None` for no effort.
pub(super) fn reasoning_effort_meta(effort: Option<ReasoningEffort>) -> Option<acp::Meta> {
    let effort = effort?;
    let mut meta = acp::Meta::new();
    meta.insert(
        REASONING_EFFORT_META_KEY.to_string(),
        reasoning_effort_meta_value(effort),
    );
    Some(meta)
}
