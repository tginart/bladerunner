//! `/sovereign` and `/tailnet` — start, stop and check models on your own hardware.
//!
//! - `/sovereign`                 — list sovereigns and whether each one serves
//! - `/sovereign up [name]`       — start a sovereign and wait until it serves
//! - `/sovereign down [name]`     — stop a sovereign
//! - `/sovereign status [name]`   — show whether one sovereign serves
//! - `/tailnet`                   — show which machines are on the tailnet
//!
//! Adding a sovereign stays on the command line (`bladerunner sovereign add`).
//! Set `[sovereign] show_commands = false` in config.toml to unlist both commands.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};
use crate::sovereign_cmd::SovereignTask;

const VERBS: &[(&str, &str)] = &[
    ("up", "Start a sovereign and wait until it serves"),
    ("down", "Stop a sovereign"),
    ("status", "Show whether a sovereign serves"),
];

pub struct SovereignCommand;

impl SlashCommand for SovereignCommand {
    fn name(&self) -> &str {
        "sovereign"
    }

    fn description(&self) -> &str {
        "Start, stop or check a model served from your own hardware"
    }

    fn usage(&self) -> &str {
        "/sovereign [up|down|status] [name]"
    }

    fn visible(&self, _ctx: &AppCtx) -> bool {
        crate::sovereign_cmd::commands_listed()
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[up|down|status] [name]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let (verb, rest) = split_first_token(args_query);
        // A complete verb needs no menu, so that Enter runs the command.
        if !rest.is_empty() || VERBS.iter().any(|(v, _)| *v == verb) {
            return None;
        }
        let items = VERBS
            .iter()
            .filter(|(v, _)| v.starts_with(verb))
            .map(|(v, description)| ArgItem::new(*v, *v, format!("{v} "), *description))
            .collect();
        Some(items)
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match parse_task(args) {
            Ok(task) => CommandResult::Action(Action::Sovereign(task)),
            Err(usage) => CommandResult::Error(usage),
        }
    }
}

pub struct TailnetCommand;

impl SlashCommand for TailnetCommand {
    fn name(&self) -> &str {
        "tailnet"
    }

    fn description(&self) -> &str {
        "Show which machines are on your tailnet"
    }

    fn usage(&self) -> &str {
        "/tailnet"
    }

    fn visible(&self, _ctx: &AppCtx) -> bool {
        crate::sovereign_cmd::commands_listed()
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::Sovereign(SovereignTask::Tailnet))
    }
}

/// `up dgx-spark` -> `Up(Some("dgx-spark"))`. An empty line lists every sovereign.
fn parse_task(args: &str) -> Result<SovereignTask, String> {
    let (verb, rest) = split_first_token(args);
    let name = (!rest.is_empty()).then(|| rest.to_string());
    match verb {
        "" | "list" => Ok(SovereignTask::Status(None)),
        "up" | "start" => Ok(SovereignTask::Up(name)),
        "down" | "stop" => Ok(SovereignTask::Down(name)),
        "status" => Ok(SovereignTask::Status(name)),
        other => Err(format!(
            "Unknown /sovereign command `{other}`. Use: /sovereign [up|down|status] [name]"
        )),
    }
}

fn split_first_token(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest.trim()),
        None => (text, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verbs_and_optional_name() {
        assert_eq!(parse_task("up dgx-spark"), Ok(SovereignTask::Up(Some("dgx-spark".into()))));
        assert_eq!(parse_task("down"), Ok(SovereignTask::Down(None)));
        assert_eq!(parse_task(""), Ok(SovereignTask::Status(None)));
        assert!(parse_task("explode").is_err());
    }
}
