pub mod command;
pub mod http;

use std::time::Duration;

use crate::config::HookSpec;
use crate::event::HookEventEnvelope;
use serde::Deserialize;

use crate::result::{HookDecision, HttpInfo, StopHookOutcome};

/// How a hook's output is interpreted, per the event's [`GateKind`]: `Observe`
/// ignores output, `Tool` parses the allow/deny vocabulary, `Stop` the stop
/// vocabulary.
pub use crate::event::GateKind;

pub struct RunContext<'a> {
    pub session_id: &'a str,
    pub workspace_root: &'a str,
    pub process_scope: Option<xai_grok_tools::util::ProcessScope>,
}

/// Result of running a single hook (any handler type).
#[derive(Debug)]
pub enum HookRunnerResult {
    Decision(HookDecision),
    /// An allowed pre-tool hook replaced the target tool arguments.
    Rewrite(serde_json::Value),
    Stop(StopHookOutcome),
    Success,
    /// Failed: the caller fails open.
    Failed(String),
}

/// JSON from `PreToolUse` gate hooks:
/// `{"decision": "allow" | "deny", "reason": "…"}`.
#[derive(Debug, Deserialize)]
pub(crate) struct GateHookJson {
    #[serde(default)]
    pub decision: String,
    #[serde(default, rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,
    #[serde(default, rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<PreToolSpecificOutput>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PreToolSpecificOutput {
    #[serde(default, rename = "hookEventName")]
    pub event: Option<String>,
    #[serde(default, rename = "permissionDecision")]
    pub decision: Option<String>,
    #[serde(default, rename = "permissionDecisionReason")]
    pub reason: Option<String>,
    #[serde(default, rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,
}

pub(crate) fn gate_json_to_result(
    mut json: GateHookJson,
    hook_name: &str,
) -> Result<HookRunnerResult, String> {
    let mut updated = json.updated_input.take();
    if let Some(specific) = json.hook_specific_output.take() {
        if specific
            .event
            .as_deref()
            .is_some_and(|event| event != "PreToolUse")
        {
            return Err("pre-tool output has a different hook event".into());
        }
        if let Some(decision) = specific.decision {
            // An explicit deny always wins over a simultaneous allow/rewrite.
            if json.decision != "deny" {
                json.decision = decision;
            }
        }
        json.reason = json.reason.or(specific.reason);
        updated = specific.updated_input.or(updated);
    }
    if json.decision.is_empty() {
        if updated.is_some() {
            json.decision = "allow".to_owned();
        } else {
            return Err("pre-tool output is missing a decision".into());
        }
    }
    let decision = gate_json_to_decision(json, hook_name)?;
    match (decision, updated) {
        (HookDecision::Allow, Some(value)) if value.is_object() => {
            Ok(HookRunnerResult::Rewrite(value))
        }
        (HookDecision::Allow, Some(_)) => Err("updatedInput must be an object".into()),
        (decision, _) => Ok(HookRunnerResult::Decision(decision)),
    }
}

/// Interpret a [`GateHookJson`] as a [`HookDecision`]. An unknown decision value
/// is an error so typos surface instead of failing open.
pub(crate) fn gate_json_to_decision(
    json: GateHookJson,
    hook_name: &str,
) -> Result<HookDecision, String> {
    match json.decision.as_str() {
        "deny" => Ok(HookDecision::Deny {
            reason: json
                .reason
                .unwrap_or_else(|| format!("denied by hook '{hook_name}'")),
            hook_name: hook_name.to_string(),
        }),
        "allow" => Ok(HookDecision::Allow),
        other => Err(format!(
            "unknown decision value '{other}' from hook '{hook_name}'"
        )),
    }
}

/// JSON from `Stop`/`SubagentStop` gate hooks. All fields optional; one output
/// can combine several signals.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct StopHookJson {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default, rename = "continue")]
    pub continue_: Option<bool>,
    #[serde(default, rename = "stopReason")]
    pub stop_reason: Option<String>,
    #[serde(default, rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<StopHookSpecificOutputJson>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StopHookSpecificOutputJson {
    #[serde(default, rename = "additionalContext")]
    pub additional_context: Option<String>,
}

/// Interpret a [`StopHookJson`] as a [`StopHookOutcome`].
///
/// `decision: "block"` requires a reason (a missing one falls back to a generic
/// message). `decision: "approve"` is a no-op; any other value is an error so
/// typos surface.
pub(crate) fn stop_json_to_outcome(
    json: StopHookJson,
    hook_name: &str,
) -> Result<StopHookOutcome, String> {
    let block_reason = match json.decision.as_deref() {
        Some("block") => Some(
            json.reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| format!("Blocked by stop hook '{hook_name}'")),
        ),
        Some("approve") | None => None,
        Some(other) => {
            return Err(format!(
                "unknown decision value '{other}' from hook '{hook_name}'"
            ));
        }
    };
    Ok(StopHookOutcome {
        block_reason,
        additional_context: json
            .hook_specific_output
            .and_then(|output| output.additional_context)
            .filter(|context| !context.trim().is_empty()),
        force_stop: (json.continue_ == Some(false)).then_some(crate::result::StopOverride {
            reason: json.stop_reason,
        }),
    })
}

/// Each runner returns the result, wall-clock duration, and optional HTTP
/// metadata for enriched scrollback logging.
pub type HookRunOutput = (HookRunnerResult, Duration, Option<HttpInfo>);

pub async fn run_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    mode: GateKind,
) -> HookRunOutput {
    match spec.handler_type {
        crate::config::HandlerType::Command => {
            let (result, elapsed) = command::run_command_hook(spec, envelope, ctx, mode).await;
            (result, elapsed, None)
        }
        crate::config::HandlerType::Http => http::run_http_hook(spec, envelope, ctx, mode).await,
    }
}

#[cfg(test)]
mod rewrite_tests {
    use super::*;
    use serde_json::json;
    fn parse(value: serde_json::Value) -> Result<HookRunnerResult, String> {
        gate_json_to_result(serde_json::from_value(value).unwrap(), "fixture")
    }
    #[test]
    fn supported_rewrite_protocols_and_deny_precedence() {
        for value in [
            json!({"decision":"allow","updatedInput":{"x":1}}),
            json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"x":1}}}),
        ] {
            assert!(
                matches!(parse(value),Ok(HookRunnerResult::Rewrite(value)) if value == json!({"x":1}))
            );
        }
        assert!(matches!(
            parse(
                json!({"decision":"deny","updatedInput":{"x":1},"hookSpecificOutput":{"permissionDecision":"allow"}})
            ),
            Ok(HookRunnerResult::Decision(HookDecision::Deny { .. }))
        ));
        assert!(parse(json!({"decision":"allow","updatedInput":[]})).is_err());
        assert!(
            parse(json!({"hookSpecificOutput":{"hookEventName":"Stop","updatedInput":{}}}))
                .is_err()
        );
    }
}
