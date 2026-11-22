/*
Purpose: Open the session file browser through the fullscreen command registry.
Input: The command context for the current session.
Output: A browser action, or a screen-mode refusal from the registry.
*/

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::{ModeSupport, Remedy};

/// The session-scoped entry point for the file browser.
pub struct FilesCommand;

impl SlashCommand for FilesCommand {
    fn name(&self) -> &str {
        "files"
    }

    fn description(&self) -> &str {
        "Browse the file tree and preview text files"
    }

    fn usage(&self) -> &str {
        "/files"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn mode_support(&self) -> ModeSupport {
        ModeSupport::FullscreenOnly(Remedy::SwitchMode {
            why: "the file browser needs the fullscreen TUI",
        })
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenFileBrowser)
    }
}
