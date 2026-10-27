use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::{ModeSupport, Remedy};

/// `/shots`: reopen the side-by-side columns of the latest `/n-shot` run.
///
/// Separate from `/workflows` so the view stays reachable in builds that hide
/// the general workflow commands.
pub struct ShotsCommand;

impl SlashCommand for ShotsCommand {
    fn name(&self) -> &str {
        "shots"
    }

    fn description(&self) -> &str {
        "Show the latest /n-shot run side by side"
    }

    fn usage(&self) -> &str {
        "/shots"
    }

    fn mode_support(&self) -> ModeSupport {
        ModeSupport::FullscreenOnly(Remedy::SwitchMode {
            why: "the n-shot columns need fullscreen",
        })
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenShots)
    }
}
