//! `n_shot` tool: fork this conversation into parallel worktree attempts.
//!
//! The tool launches the built-in `n-shot` workflow through the same session
//! channel as the `workflow` tool. It exists separately so that products that
//! do not register the general `workflow` tool can still offer n-shot runs.

use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

mod decision;

use super::task::types::SubagentDepthCounter;
use super::workflow::{
    WorkflowLaunchAck, WorkflowLaunchHandle, WorkflowLaunchRequest, WorkflowToolInput,
};

pub use decision::{NShotOutcome, NShotResolve};

pub const N_SHOT_TOOL_NAME: &str = "n_shot";
/// Upper bound on shots per run. Keep in sync with `workflows/n_shot.rhai`.
pub const N_SHOT_MAX_SHOTS: usize = 10;
const N_SHOT_WORKFLOW_NAME: &str = "n-shot";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct NShotToolInput {
    #[schemars(
        description = "Prompt for every shot that does not set its own prompt. Write it as the user's request, as each shot already has this conversation."
    )]
    #[serde(default)]
    pub prompt: Option<String>,
    #[schemars(
        description = "Number of identical shots (1-10). Ignored when `shots` is set. Defaults to 2."
    )]
    #[serde(default)]
    pub n: Option<u64>,
    #[schemars(
        description = "Per-shot settings, one item per shot (at most 10). Use this to vary the prompt or model between shots."
    )]
    #[serde(default)]
    pub shots: Option<Vec<NShotSpec>>,
    #[schemars(
        description = "Set this instead of the fields above, after the user picked a shot or a combination, to record which shots were used. Do not set it together with `prompt`, `n`, or `shots`."
    )]
    #[serde(default)]
    pub resolve: Option<NShotResolve>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct NShotSpec {
    #[schemars(description = "Prompt for this shot. Defaults to the top-level `prompt`.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[schemars(description = "Model id for this shot. Defaults to the session model.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[schemars(description = "Short label shown in the shot's column, such as `minimal-fix`.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl NShotToolInput {
    /// Purpose: Check the input and make the `args` for the `n-shot` workflow.
    /// Input: The tool input.
    /// Output: The workflow args, or an error message for the model.
    pub fn to_workflow_args(&self) -> Result<serde_json::Value, String> {
        if self.resolve.is_some() {
            return Err("`resolve` cannot be combined with `prompt`, `n`, or `shots`".into());
        }
        let prompt = self
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let objective = |fallback: &str| prompt.unwrap_or(fallback).to_string();
        let Some(shots) = &self.shots else {
            let Some(prompt) = prompt else {
                return Err("set `prompt`, or set a prompt on every item of `shots`".into());
            };
            let n = self.n.unwrap_or(2);
            if n == 0 || n as usize > N_SHOT_MAX_SHOTS {
                return Err(format!("`n` must be from 1 to {N_SHOT_MAX_SHOTS}"));
            }
            return Ok(serde_json::json!({ "n": n, "prompt": prompt, "objective": prompt }));
        };
        if shots.is_empty() || shots.len() > N_SHOT_MAX_SHOTS {
            return Err(format!("`shots` must have 1 to {N_SHOT_MAX_SHOTS} items"));
        }
        let has_prompt =
            |shot: &NShotSpec| shot.prompt.as_deref().is_some_and(|p| !p.trim().is_empty());
        if prompt.is_none() && !shots.iter().all(has_prompt) {
            return Err("every shot needs a prompt when the top-level `prompt` is not set".into());
        }
        let first_prompt = shots.iter().find_map(|s| s.prompt.as_deref()).unwrap_or("");
        Ok(serde_json::json!({
            "prompt": prompt,
            "shots": shots,
            "objective": objective(first_prompt),
        }))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct NShotToolOutput {
    #[schemars(description = "The session-unique display name of the run, such as n-shot-2.")]
    pub name: String,
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for NShotToolOutput {}

#[derive(Debug, Default)]
pub struct NShotTool;

impl crate::types::tool_metadata::ToolMetadata for NShotTool {
    fn kind(&self) -> ToolKind {
        ToolKind::NShot
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Fork this conversation into parallel attempts at the same task. Each shot starts from a copy of this conversation and works in its own git worktree, so the shots cannot touch each other or the user's working tree. The call returns immediately. The shots run in the background, the user watches them side by side, and when all finish you get a report with each shot's final message, worktree path, and changed files.

Use it when the user asks for several independent attempts, or to compare approaches or models. Pass `prompt` and `n` for identical shots, or `shots` to vary the prompt or model per shot (at most 10). Do not poll or wait for the result.

When the report arrives, compare the shots for the user and recommend one. Do not change the user's working tree until the user picks a shot or a combination.

After you apply the user's choice, call this tool again with only `resolve` to record it: the `report_path` from the completion message, the `outcome` (`single`, `hybrid`, or `none`), the labels of the `shots` used, and a short `rationale`."##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl xai_tool_runtime::Tool for NShotTool {
    type Args = NShotToolInput;
    type Output = NShotToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(N_SHOT_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            N_SHOT_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "new_tool.n_shot", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: NShotToolInput,
    ) -> Result<NShotToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        if let Some(resolve) = input.resolve.clone() {
            if input.prompt.is_some() || input.n.is_some() || input.shots.is_some() {
                return Err(xai_tool_runtime::ToolError::custom(
                    "n_shot_invalid_input",
                    "`resolve` cannot be combined with `prompt`, `n`, or `shots`",
                ));
            }
            let cwd = {
                let resources = shared_resources(&ctx)?;
                let res = resources.lock().await;
                res.get::<crate::types::resources::Cwd>()
                    .map(|c| c.0.clone())
                    .unwrap_or_default()
            };
            let message = decision::record(resolve, cwd).await.map_err(|detail| {
                xai_tool_runtime::ToolError::custom("n_shot_invalid_input", detail)
            })?;
            return Ok(NShotToolOutput {
                name: "n-shot decision".to_string(),
                message,
            });
        }
        let args = input.to_workflow_args().map_err(|detail| {
            xai_tool_runtime::ToolError::custom("n_shot_invalid_input", detail)
        })?;
        let resources = shared_resources(&ctx)?;
        let (depth, sender) = {
            let res = resources.lock().await;
            let depth = res.get::<SubagentDepthCounter>().map(|d| d.0).unwrap_or(0);
            let sender = res.get::<WorkflowLaunchHandle>().map(|h| h.0.clone());
            (depth, sender)
        };
        if depth > 0 {
            return Err(xai_tool_runtime::ToolError::custom(
                "n_shot_depth_exceeded",
                "n_shot can only be used from a top-level session",
            ));
        }
        let sender = sender.ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "n_shot_not_available",
                "Background workflows are not available in this session",
            )
        })?;
        let request = WorkflowLaunchRequest {
            input: WorkflowToolInput {
                agent_budget: None,
                name: Some(N_SHOT_WORKFLOW_NAME.to_string()),
                script: None,
                script_path: None,
                args: Some(args),
                resume_from_run_id: None,
                validate_only: false,
            },
        };
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<WorkflowLaunchAck>();
        sender.send((request, ack_tx)).map_err(|_| {
            xai_tool_runtime::ToolError::custom(
                "n_shot_channel_closed",
                "The launch channel closed; the session may be shutting down",
            )
        })?;
        match ack_rx.await {
            Ok(WorkflowLaunchAck::Started { name, .. }) => Ok(NShotToolOutput {
                message: format!(
                    "n-shot run '{name}' started in the background. The user can watch the \
                     shots side by side. The comparison report arrives automatically when \
                     all shots finish."
                ),
                name,
            }),
            Ok(WorkflowLaunchAck::Validated { name, .. }) => Ok(NShotToolOutput {
                message: "Validated only; nothing was launched.".to_string(),
                name,
            }),
            Ok(WorkflowLaunchAck::Rejected { code, detail }) => {
                Err(xai_tool_runtime::ToolError::custom(code, detail))
            }
            Err(_) => Err(xai_tool_runtime::ToolError::custom(
                "n_shot_launch_no_ack",
                "The session dropped the launch channel before answering; the run may not \
                 have started.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: serde_json::Value) -> NShotToolInput {
        serde_json::from_value(value).expect("valid input")
    }

    #[test]
    fn identical_shots_default_to_two() {
        let args = input(serde_json::json!({ "prompt": "fix it" }))
            .to_workflow_args()
            .unwrap();
        assert_eq!(args["n"], 2);
        assert_eq!(args["prompt"], "fix it");
        assert_eq!(args["objective"], "fix it");
    }

    #[test]
    fn per_shot_settings_pass_through() {
        let args = input(serde_json::json!({ "shots": [
            { "prompt": "small fix", "model": "m1" },
            { "prompt": "big fix", "label": "big" },
        ] }))
        .to_workflow_args()
        .unwrap();
        assert_eq!(args["shots"][0]["model"], "m1");
        assert_eq!(args["shots"][1]["label"], "big");
        assert_eq!(args["objective"], "small fix");
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "prompt": "x", "n": 0 }),
            serde_json::json!({ "prompt": "x", "n": 11 }),
            serde_json::json!({ "shots": [] }),
            serde_json::json!({ "shots": [{ "model": "m" }] }),
        ] {
            assert!(input(bad.clone()).to_workflow_args().is_err(), "{bad}");
        }
    }
}
