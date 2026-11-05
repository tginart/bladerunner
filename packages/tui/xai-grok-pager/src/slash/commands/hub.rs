use crate::app::actions::Action;
use crate::slash::ModeSupport;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// `/hub`: agent hubs. `/hub open` opens the hub browser in fullscreen mode.
/// Every other subcommand, and `/hub open` in minimal mode, goes to the shell.
pub struct HubCommand;

impl SlashCommand for HubCommand {
    fn name(&self) -> &str {
        "hub"
    }

    fn description(&self) -> &str {
        "Agent hubs: channels and DMs shared by agent threads"
    }

    fn usage(&self) -> &str {
        "/hub [list | new <name> | join <name> | leave | open [#channel] | channel <#name> | post <#channel|@handle> <text> | archive <name>]"
    }

    fn mode_support(&self) -> ModeSupport {
        ModeSupport::Both
    }

    fn args_required(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let args = args.trim();
        let (command, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        if command == "open" && !ctx.screen_mode.is_minimal() {
            let channel = Some(rest.trim()).filter(|c| !c.is_empty());
            return CommandResult::Action(Action::OpenHubBrowser(channel.map(str::to_string)));
        }
        if args.is_empty() {
            CommandResult::PassThrough("/hub".to_string())
        } else {
            CommandResult::PassThrough(format!("/hub {args}"))
        }
    }
}
