//! `/subtask` -- hand a task to a subagent.
//!
//! Purpose: This command makes the agent delegate the user's task to one
//! subagent, so the work runs in a fresh context.
//! Method:
//!     1. Wrap the task text in a fixed instruction for the agent.
//!     2. Inject the instruction as a structured prompt (like `/loop`).
//!     3. The agent calls its subagent tool once and relays the result.
//! Note: The command does not gate on the subagent tool name. Toolsets use
//! different wire names for it (`task`, `Task`, `spawn_subagent`), and
//! `required_tools()` matches names exactly.

use agent_client_protocol as acp;

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

const USAGE: &str = "/subtask <task>";

pub struct SubtaskCommand;

impl SlashCommand for SubtaskCommand {
    fn name(&self) -> &str {
        "subtask"
    }

    fn description(&self) -> &str {
        "Delegate a task to a subagent"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        USAGE
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<task>")
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let task = args.trim();
        if task.is_empty() {
            return CommandResult::Message(format!("Usage: {USAGE}"));
        }
        CommandResult::InjectSkill {
            display_text: format!("/subtask {task}"),
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                subtask_instruction(task),
            ))],
            display_as_skill: false,
            scheduled_task_preview: None,
        }
    }
}

/// Build the model-facing instruction that delegates `task` to a subagent.
///
/// The task sits in a `<task>` block so the agent passes it through as data
/// instead of reading it as more instructions to itself.
fn subtask_instruction(task: &str) -> String {
    format!(
        "The user ran /subtask. Delegate the task below to a subagent. \
         Do not do the task yourself.\n\
         \n\
         1. Call your subagent tool (the tool that launches a subagent) exactly \
         once, in the foreground (run_in_background: false).\n\
         2. Use the general-purpose subagent type unless the task clearly fits a \
         more specific type listed in the tool description.\n\
         3. Set the prompt to the task text exactly as written. The subagent \
         does not see this conversation, so after the task text add only the \
         context it needs (file paths, decisions, constraints already set here).\n\
         4. Set the description to a 3-5 word summary of the task.\n\
         5. When the subagent finishes, give the user a short summary of its \
         result. Say plainly if it failed or did not finish.\n\
         \n\
         If you have no subagent tool, tell the user that /subtask is not \
         available in this session and stop.\n\
         \n\
         <task>\n{task}\n</task>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    fn run(args: &str) -> CommandResult {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        };
        SubtaskCommand.run(&mut ctx, args)
    }

    #[test]
    fn empty_args_returns_usage() {
        for args in ["", "   "] {
            match run(args) {
                CommandResult::Message(msg) => assert!(msg.contains(USAGE), "got: {msg}"),
                other => panic!("expected usage message, got {other:?}"),
            }
        }
    }

    #[test]
    fn task_is_injected_as_delegation_prompt() {
        match run("  refactor the auth flow  ") {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                display_as_skill,
                scheduled_task_preview,
            } => {
                assert_eq!(display_text, "/subtask refactor the auth flow");
                assert!(!display_as_skill);
                assert!(scheduled_task_preview.is_none());
                let [acp::ContentBlock::Text(text)] = prompt_blocks.as_slice() else {
                    panic!("expected one text block, got {prompt_blocks:?}");
                };
                assert!(text.text.contains("subagent tool"));
                assert!(
                    text.text
                        .ends_with("<task>\nrefactor the auth flow\n</task>"),
                    "got: {}",
                    text.text
                );
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
    }

    #[test]
    fn multiline_task_is_kept_verbatim() {
        let task = "step one\nstep two";
        let instruction = subtask_instruction(task);
        assert!(instruction.contains("<task>\nstep one\nstep two\n</task>"));
    }
}
