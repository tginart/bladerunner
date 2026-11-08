use crate::app::actions::Action;
use crate::slash::ModeSupport;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// `/hub`: agent hubs. `/hub open [name] [#channel]` opens the hub browser in
/// fullscreen mode. Every other subcommand, and `/hub open` in minimal mode,
/// goes to the shell.
pub struct HubCommand;

impl SlashCommand for HubCommand {
    fn name(&self) -> &str {
        "hub"
    }

    fn description(&self) -> &str {
        "Agent hubs: channels and DMs shared by agent threads"
    }

    fn usage(&self) -> &str {
        "/hub [list | new <name> | join <name> | leave | open [name] [#channel] | channel <#name> | post <#channel|@handle> <text> | archive <name>]"
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
            let (hub, channel) = parse_open_args(rest);
            return CommandResult::Action(Action::OpenHubBrowser { hub, channel });
        }
        if args.is_empty() {
            CommandResult::PassThrough("/hub".to_string())
        } else {
            CommandResult::PassThrough(format!("/hub {args}"))
        }
    }
}

/// Split `/hub open` arguments into a hub name and a `#channel`, in any order.
fn parse_open_args(rest: &str) -> (Option<String>, Option<String>) {
    let mut hub = None;
    let mut channel = None;
    for word in rest.split_whitespace() {
        if word.starts_with('#') {
            channel.get_or_insert_with(|| word.to_string());
        } else {
            hub.get_or_insert_with(|| word.to_string());
        }
    }
    (hub, channel)
}

#[cfg(test)]
mod tests {
    use super::parse_open_args;

    #[test]
    fn open_args_take_a_hub_name_and_a_channel_in_any_order() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(parse_open_args(""), (None, None));
        assert_eq!(parse_open_args("#build"), (None, some("#build")));
        assert_eq!(parse_open_args("team"), (some("team"), None));
        assert_eq!(
            parse_open_args(" team #build "),
            (some("team"), some("#build"))
        );
        assert_eq!(
            parse_open_args("#build team"),
            (some("team"), some("#build"))
        );
    }
}
