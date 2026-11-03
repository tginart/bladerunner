//! `/hooks` and `/plugins` -- open the hooks/plugins modal.
//!
//! These commands open the tabbed modal. All hook/plugin management
//! (install, uninstall, trust, etc.) is done through the modal's UI — no
//! subcommands are passed through to the shell via the slash command.
//! The one exception is `/hooks mute` / `/hooks unmute`, a pager-side
//! display setting that hides hook results while the hooks still run.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};
use crate::views::extensions_modal::ExtensionsTab;
use xai_grok_telemetry::events::ExtensionsModalTrigger;

/// Open the hooks/plugins modal on the Hooks tab.
pub struct HooksCommand;

impl SlashCommand for HooksCommand {
    fn name(&self) -> &str {
        "hooks"
    }

    fn description(&self) -> &str {
        "View hooks"
    }

    fn usage(&self) -> &str {
        "/hooks [mute|unmute]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[mute|unmute]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let muted = crate::appearance::cache::load_hooks_muted();
        let item = |name: &str, description: &str, active: bool| ArgItem {
            display: name.to_string(),
            match_text: name.to_string(),
            insert_text: name.to_string(),
            description: if active {
                format!("{description} (active)")
            } else {
                description.to_string()
            },
            locked: false,
            action_id: None,
            hidden: false,
        };
        Some(vec![
            item("mute", "Run hooks silently: hide their results", muted),
            item("unmute", "Show hook results in the transcript", !muted),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim() {
            "" => CommandResult::Action(Action::OpenExtensionsModal {
                tab: ExtensionsTab::Hooks,
                trigger: ExtensionsModalTrigger::SlashCommand,
            }),
            "mute" => CommandResult::Action(Action::SetHooksMuted(true)),
            "unmute" => CommandResult::Action(Action::SetHooksMuted(false)),
            other => CommandResult::Error(format!(
                "Unknown /hooks argument \"{other}\". Usage: /hooks [mute|unmute]"
            )),
        }
    }
}

/// Open the hooks/plugins modal on the Plugins tab.
pub struct PluginsCommand;

impl SlashCommand for PluginsCommand {
    fn name(&self) -> &str {
        "plugins"
    }

    fn description(&self) -> &str {
        "View plugins"
    }

    fn usage(&self) -> &str {
        "/plugins"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Plugins,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}

/// Open the hooks/plugins modal on the Marketplace tab.
pub struct MarketplaceCommand;

impl SlashCommand for MarketplaceCommand {
    fn name(&self) -> &str {
        "marketplace"
    }

    fn description(&self) -> &str {
        "View marketplace"
    }

    fn usage(&self) -> &str {
        "/marketplace"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Marketplace,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}

/// Open the hooks/plugins modal on the Skills tab.
pub struct SkillsCommand;

impl SlashCommand for SkillsCommand {
    fn name(&self) -> &str {
        "skills"
    }

    fn description(&self) -> &str {
        "View skills"
    }

    fn usage(&self) -> &str {
        "/skills"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Skills,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    fn run_hooks(args: &str) -> CommandResult {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Fullscreen,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        };
        HooksCommand.run(&mut ctx, args)
    }

    #[test]
    fn hooks_args_open_the_modal_or_set_the_mute_setting() {
        assert!(matches!(
            run_hooks(""),
            CommandResult::Action(Action::OpenExtensionsModal {
                tab: ExtensionsTab::Hooks,
                ..
            })
        ));
        assert!(matches!(
            run_hooks("mute"),
            CommandResult::Action(Action::SetHooksMuted(true))
        ));
        assert!(matches!(
            run_hooks(" unmute "),
            CommandResult::Action(Action::SetHooksMuted(false))
        ));
        assert!(matches!(run_hooks("silence"), CommandResult::Error(_)));
    }
}
