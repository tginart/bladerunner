//! Open a GitHub pull request in the current session.
use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::{ModeSupport, Remedy};

pub struct PullRequestCommand;

impl SlashCommand for PullRequestCommand {
    fn name(&self) -> &str {
        "pr"
    }
    fn description(&self) -> &str {
        "Read a GitHub pull request and ask AI about its changes"
    }
    fn usage(&self) -> &str {
        "/pr [number | URL | open | minimize | close | refresh]"
    }
    fn takes_args(&self) -> bool {
        true
    }
    fn session_scoped(&self) -> bool {
        true
    }
    fn mode_support(&self) -> ModeSupport {
        ModeSupport::FullscreenOnly(Remedy::SwitchMode {
            why: "the pull request viewer needs fullscreen",
        })
    }
    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        use crate::views::pull_request::summary::Control;
        let control = match args.trim() {
            "open" => Some(Control::Expand),
            "minimize" => Some(Control::Minimize),
            "close" => Some(Control::Close),
            "refresh" => Some(Control::Refresh),
            _ => None,
        };
        if let Some(control) = control {
            return CommandResult::Action(Action::PullRequestControl(control));
        }
        match crate::views::pull_request::github::target(args) {
            Ok(target) => CommandResult::Action(Action::OpenPullRequest(target)),
            Err(error) => CommandResult::Error(error.to_string()),
        }
    }
}
