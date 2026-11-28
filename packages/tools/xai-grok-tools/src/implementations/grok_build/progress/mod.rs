//! Purpose: Progress bars for background commands and subagents.
//!
//! An agent fills a bar in one of three ways:
//! - A progress pattern on a background command ([`rule`], [`pipeline`]). The
//!   output of the command fills the bar automatically.
//! - Progress checkpoints on a subagent spawn ([`checkpoint_prompt_block`]).
//!   The subagent reports each checkpoint that it completes.
//! - The `update_progress` tool ([`UpdateProgressTool`]). The agent sets the
//!   value directly.
//!
//! All three send a `TaskProgress` notification. The client shows the value on
//! the row of the task.
//!
//! The whole feature is present only when the `update_progress` tool is in the
//! toolset. The command tool and the subagent tool remove their progress
//! parameters when it is absent ([`is_progress_enabled`]).

pub mod pipeline;
pub mod rule;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::SubagentDepthCounter;
use crate::notification::types::TaskProgress;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{NotificationHandle, Terminal};
use crate::types::template_renderer::TemplateRenderer;
use crate::types::tool::{ToolKind, ToolNamespace};
use rule::{PERCENT_TOTAL, ProgressValue};

/// Canonical tool id.
pub const UPDATE_PROGRESS_TOOL_NAME: &str = "update_progress";
/// Minimum and maximum number of checkpoints on one subagent spawn.
pub const MIN_CHECKPOINTS: usize = 2;
pub const MAX_CHECKPOINTS: usize = 20;
/// Maximum length of one checkpoint name, in characters.
const MAX_CHECKPOINT_CHARS: usize = 120;

/// Purpose: Tell if the progress feature is available to this agent.
/// Input: The template renderer of the finalized toolset.
/// Output: True when the `update_progress` tool is in the toolset.
pub fn is_progress_enabled(renderer: &TemplateRenderer) -> bool {
    renderer.tool_for_kind(ToolKind::ProgressUpdate).is_some()
}

/// Purpose: Remove properties from a tool input schema.
/// Input: The JSON schema of the tool input and the property names.
/// Output: A copy of the schema without those properties.
pub fn schema_without_properties(schema: &serde_json::Value, names: &[&str]) -> serde_json::Value {
    let mut schema = schema.clone();
    let Some(object) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(properties) = object.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for name in names {
            properties.remove(*name);
        }
    }
    if let Some(required) = object.get_mut("required").and_then(|r| r.as_array_mut()) {
        required.retain(|v| !v.as_str().is_some_and(|s| names.contains(&s)));
    }
    schema
}

/// Purpose: Clean the checkpoint names from a subagent spawn.
/// Input: The names from the tool input.
/// Output: The trimmed, non-empty names, or a message for the model.
pub fn validated_checkpoints(raw: &[String]) -> Result<Vec<String>, String> {
    let names: Vec<String> = raw
        .iter()
        .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(MAX_CHECKPOINT_CHARS).collect())
        .collect();
    if names.is_empty() {
        return Ok(names);
    }
    if names.len() < MIN_CHECKPOINTS || names.len() > MAX_CHECKPOINTS {
        return Err(format!(
            "progress_checkpoints needs {MIN_CHECKPOINTS} to {MAX_CHECKPOINTS} checkpoints, \
             got {}",
            names.len()
        ));
    }
    Ok(names)
}

/// Purpose: Make the text that tells a subagent about its checkpoints.
/// Input: The checkpoint names and the name of the progress tool.
/// Output: A block to append to the prompt of the subagent.
pub fn checkpoint_prompt_block(checkpoints: &[String], tool_name: &str) -> String {
    let total = checkpoints.len();
    let mut block = String::from("\n\n<progress_checkpoints>\n");
    block.push_str(
        "The agent that started you shows the user a progress bar for this task. \
         These are its checkpoints, in order:\n",
    );
    for (index, name) in checkpoints.iter().enumerate() {
        block.push_str(&format!("{}. {name}\n", index + 1));
    }
    block.push_str(&format!(
        "Each time you complete a checkpoint, call {tool_name} with completed set to the \
         number of checkpoints done so far and total set to {total}. Do not pass a task id. \
         Do not mention the checkpoints in your final answer.\n</progress_checkpoints>"
    ));
    block
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct UpdateProgressInput {
    #[schemars(
        description = "Units of work done so far, in the same unit as total (for example 3 when 3 of 8 files are done). With no total, a percentage from 0 to 100."
    )]
    pub completed: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Total units of work. Omit to give completed as a percentage. You can raise it later if the work grows."
    )]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Task id of a background command or subagent that you started. Omit inside a subagent to report your own progress."
    )]
    pub task_id: Option<String>,
}

impl UpdateProgressInput {
    /// Short title for the tool row in a UI, such as `Progress 3/8`.
    pub fn title(&self) -> String {
        match progress_value(self) {
            Ok(value) => format!("Progress {}", format_value(value)),
            Err(_) => "Progress".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateProgressOutput {
    pub text: String,
}

impl xai_tool_runtime::ToolOutput for UpdateProgressOutput {
    fn model_output(&self) -> Vec<xai_tool_runtime::ContentBlock> {
        vec![xai_tool_runtime::ContentBlock::Text {
            text: self.text.clone(),
        }]
    }
}

impl From<UpdateProgressOutput> for crate::types::output::ToolOutput {
    fn from(output: UpdateProgressOutput) -> Self {
        crate::types::output::ToolOutput::Text(output.text.into())
    }
}

#[derive(Debug, Default)]
pub struct UpdateProgressTool;

impl crate::types::tool_metadata::ToolMetadata for UpdateProgressTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ProgressUpdate
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Set the progress bar that the user sees on the row of a background command or a subagent.

A progress bar is optional. Set one when the work has a size you can count (N files, N tests, N steps) and takes long enough for the user to watch. Do not set one for short or open-ended work, and do not invent numbers.

- For a background command or subagent that you started, pass its task id as `task_id`.
- Inside a subagent, omit `task_id` to report your own progress to the agent that started you.
- `completed` and `total` count the same unit, for example 3 of 8 files. Omit `total` to give `completed` as a percentage.

Call this when the count changes, not after every small action. The bar stays where you last set it.${%- if tools.by_kind.execute %} When the output of a command already shows its progress, give ${{ tools.by_kind.execute }} a progress pattern instead: the bar then fills with no further calls.${%- endif %}"#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

impl xai_tool_runtime::Tool for UpdateProgressTool {
    type Args = UpdateProgressInput;
    type Output = UpdateProgressOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(UPDATE_PROGRESS_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            UPDATE_PROGRESS_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    /// Purpose: Send one progress value to the client.
    /// Method:
    ///     1. Validate the numbers.
    ///     2. With a task id, make sure that the task exists.
    ///     3. With no task id, make sure that this agent is a subagent.
    ///     4. Send the `TaskProgress` notification.
    #[tracing::instrument(name = "new_tool.update_progress", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: UpdateProgressInput,
    ) -> Result<UpdateProgressOutput, xai_tool_runtime::ToolError> {
        use xai_tool_runtime::ToolError;
        let value = progress_value(&input).map_err(ToolError::invalid_arguments)?;
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let (terminal, subagents, depth, notification_handle) = {
            let res = resources.lock().await;
            (
                res.get::<Terminal>().map(|t| t.0.clone()),
                res.get::<SubagentBackendResource>().cloned(),
                res.get::<SubagentDepthCounter>().map(|d| d.0).unwrap_or(0),
                res.require::<NotificationHandle>()?.0.clone(),
            )
        };

        let task_id = input
            .task_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        match task_id {
            Some(id) => {
                let is_command = match &terminal {
                    Some(terminal) => terminal.get_task(id).await.is_some(),
                    None => false,
                };
                let is_subagent = match (&subagents, is_command) {
                    (Some(backend), false) => backend.backend().query(id, false, None).await.is_some(),
                    _ => false,
                };
                if !is_command && !is_subagent {
                    return Err(ToolError::invalid_arguments(format!(
                        "No background command or subagent has the task id {id:?}."
                    )));
                }
            }
            None if depth == 0 => {
                return Err(ToolError::invalid_arguments(
                    "task_id is required here. Only a subagent can omit it, to report its own \
                     progress.",
                ));
            }
            None => {}
        }

        notification_handle.send_task_progress(TaskProgress {
            task_id: task_id.map(str::to_string),
            completed: value.completed,
            total: value.total,
        });
        Ok(UpdateProgressOutput {
            text: format!("Progress bar set to {}.", format_value(value)),
        })
    }
}

/// Purpose: Convert the tool input to a progress value.
/// Input: The tool input.
/// Output: The value, or a message for the model.
/// Note: With no total, `completed` is a percentage.
fn progress_value(input: &UpdateProgressInput) -> Result<ProgressValue, String> {
    let completed = input.completed;
    if !completed.is_finite() || completed < 0.0 {
        return Err("completed must be a number that is 0 or larger".to_string());
    }
    let total = match input.total {
        None => PERCENT_TOTAL,
        Some(total) if total.is_finite() && total.round() >= 1.0 => total.round() as u64,
        Some(_) => return Err("total must be 1 or larger".to_string()),
    };
    ProgressValue::new(completed.round() as u64, total)
        .ok_or_else(|| "total must be 1 or larger".to_string())
}

/// `45%` for a percentage, `3/8` for a count.
fn format_value(value: ProgressValue) -> String {
    if value.total == PERCENT_TOTAL {
        format!("{}%", value.completed)
    } else {
        format!("{}/{}", value.completed, value.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::ToolNotificationHandle;
    use crate::notification::types::ToolNotification;
    use crate::types::resources::Resources;
    use std::sync::Arc;

    fn input(completed: f64, total: Option<f64>, task_id: Option<&str>) -> UpdateProgressInput {
        UpdateProgressInput {
            completed,
            total,
            task_id: task_id.map(str::to_string),
        }
    }

    fn context(resources: Resources) -> xai_tool_runtime::ToolCallContext {
        let shared: crate::types::resources::SharedResources =
            Arc::new(tokio::sync::Mutex::new(resources));
        let mut ctx = xai_tool_runtime::ToolCallContext::default();
        ctx.extensions.insert(shared);
        ctx
    }

    #[test]
    fn value_without_total_is_a_percentage() {
        let value = progress_value(&input(45.4, None, None)).unwrap();
        assert_eq!((value.completed, value.total), (45, 100));
        assert_eq!(format_value(value), "45%");
        // More than 100% does not overflow the bar.
        let value = progress_value(&input(250.0, None, None)).unwrap();
        assert_eq!(value.completed, 100);
    }

    #[test]
    fn value_with_total_is_a_count() {
        let value = progress_value(&input(3.0, Some(8.0), None)).unwrap();
        assert_eq!(format_value(value), "3/8");
        assert_eq!(input(3.0, Some(8.0), None).title(), "Progress 3/8");
    }

    #[test]
    fn bad_numbers_are_rejected() {
        assert!(progress_value(&input(-1.0, None, None)).is_err());
        assert!(progress_value(&input(f64::NAN, None, None)).is_err());
        assert!(progress_value(&input(1.0, Some(0.0), None)).is_err());
    }

    #[test]
    fn checkpoints_are_trimmed_and_bounded() {
        let names = validated_checkpoints(&[" Read  the spec ".into(), "".into(), "Fix".into()])
            .unwrap();
        assert_eq!(names, ["Read the spec", "Fix"]);
        assert!(validated_checkpoints(&[]).unwrap().is_empty());
        assert!(validated_checkpoints(&["only one".into()]).is_err());
        let too_many: Vec<String> = (0..=MAX_CHECKPOINTS).map(|i| format!("step {i}")).collect();
        assert!(validated_checkpoints(&too_many).is_err());
    }

    #[test]
    fn checkpoint_block_names_the_tool_and_the_total() {
        let block = checkpoint_prompt_block(&["Read".into(), "Fix".into()], "update_progress");
        assert!(block.contains("1. Read\n2. Fix\n"));
        assert!(block.contains("call update_progress"));
        assert!(block.contains("total set to 2"));
    }

    #[test]
    fn schema_strip_removes_properties_and_required_entries() {
        let schema = serde_json::json!({
            "properties": {"command": {}, "progress_pattern": {}, "progress_total": {}},
            "required": ["command", "progress_pattern"],
        });
        let stripped = schema_without_properties(&schema, &["progress_pattern", "progress_total"]);
        assert_eq!(
            stripped,
            serde_json::json!({"properties": {"command": {}}, "required": ["command"]})
        );
    }

    #[tokio::test]
    async fn subagent_reports_its_own_progress_without_a_task_id() {
        let (handle, mut rx) = ToolNotificationHandle::channel();
        let mut resources = Resources::new();
        resources.insert(NotificationHandle(handle));
        resources.insert(SubagentDepthCounter(1));

        let output = xai_tool_runtime::Tool::run(
            &UpdateProgressTool,
            context(resources),
            input(2.0, Some(5.0), None),
        )
        .await
        .unwrap();

        assert_eq!(output.text, "Progress bar set to 2/5.");
        assert_eq!(
            rx.recv().await,
            Some(ToolNotification::TaskProgress(TaskProgress {
                task_id: None,
                completed: 2,
                total: 5,
            }))
        );
    }

    #[tokio::test]
    async fn top_level_agent_must_name_a_task() {
        let (handle, mut rx) = ToolNotificationHandle::channel();
        let mut resources = Resources::new();
        resources.insert(NotificationHandle(handle));

        let error = xai_tool_runtime::Tool::run(
            &UpdateProgressTool,
            context(resources),
            input(10.0, None, None),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("task_id is required"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_task_id_is_rejected() {
        let (handle, mut rx) = ToolNotificationHandle::channel();
        let mut resources = Resources::new();
        resources.insert(NotificationHandle(handle));

        let error = xai_tool_runtime::Tool::run(
            &UpdateProgressTool,
            context(resources),
            input(10.0, None, Some("nope")),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("nope"));
        assert!(rx.try_recv().is_err());
    }
}
