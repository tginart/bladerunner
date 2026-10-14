//! `/login <provider>` -- log in or re-authenticate with your account.
//!
//! Optional argument: `kimi` / `kimi-code` starts Kimi Code device OAuth;
//! `openai` / `codex` starts OpenAI Codex browser OAuth;
//! `palantir-aip` / `palantir` / `foundry` / `aip` signs in to Palantir AIP:
//! `[oauth|token] [hostname] [client-id]` picks browser OAuth (default) or a
//! pasted personal token, and saves a hostname / client ID given here;
//! `claude` / `anthropic` starts the Anthropic Claude subscription OAuth;
//! `github` / `copilot` starts the GitHub Copilot device OAuth;
//! `radius` starts Radius browser PKCE OAuth.
//! `amazon-bedrock` / `bedrock` prints the safe CLI setup commands.
//! OpenCode Go uses a Console-issued API key rather than a portable OAuth
//! login, so `/login opencode-go` redirects to `/providers opencode-go`.
//! The provider argument is required. xAI routes remain disabled in the UI.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Log in to OpenAI Codex or Palantir AIP"
    }

    fn usage(&self) -> &str {
        "/login [codex|palantir-aip [token] [hostname] [client-id]]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[codex|palantir-aip]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        if !args_query.trim().is_empty() {
            return None;
        }
        Some(vec![
            ArgItem::new(
                "OpenAI Codex",
                "codex openai openai-codex chatgpt",
                "codex",
                "Sign in with OpenAI Codex",
            ),
            ArgItem::new(
                "Palantir AIP",
                "palantir-aip palantir foundry aip oauth",
                "palantir-aip",
                "Sign in to Palantir AIP in the browser (OAuth)",
            ),
            ArgItem::new(
                "Palantir AIP (personal token)",
                "palantir-aip palantir foundry aip token",
                "palantir-aip token",
                "Sign in to Palantir AIP with a Foundry personal token",
            ),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let mut words = args.split_whitespace();
        let arg = words.next().unwrap_or_default().to_ascii_lowercase();
        let rest: Vec<&str> = words.collect();
        if !rest.is_empty() && !is_palantir_target(&arg) {
            return CommandResult::Error(format!("`/login {arg}` takes no further arguments."));
        }
        if matches!(arg.as_str(), "kimi" | "kimi-code") {
            CommandResult::Action(Action::LoginKimi)
        } else if matches!(
            arg.as_str(),
            "openai" | "openai-codex" | "codex" | "chatgpt"
        ) {
            CommandResult::Action(Action::LoginOpenAiCodex)
        } else if is_palantir_target(&arg) {
            match parse_palantir_login(&rest) {
                Ok(request) => CommandResult::Action(Action::LoginPalantirAip(request)),
                Err(message) => CommandResult::Error(message),
            }
        } else if matches!(arg.as_str(), "xai" | "grok") {
            CommandResult::Error("Grok / xAI login is disabled in Blade Runner.".into())
        } else if matches!(arg.as_str(), "claude" | "anthropic" | "anthropic-claude") {
            CommandResult::Action(Action::LoginAnthropicClaude)
        } else if matches!(arg.as_str(), "github" | "github-copilot" | "copilot") {
            CommandResult::Action(Action::LoginGitHubCopilot)
        } else if matches!(arg.as_str(), "radius") {
            CommandResult::Action(Action::LoginRadius)
        } else if matches!(arg.as_str(), "amazon-bedrock" | "bedrock") {
            CommandResult::Error(
                "Amazon Bedrock supports three auth modes:\n  \
                 • Bearer token: run `bladerunner login --bedrock` in an interactive terminal.\n  \
                 • AWS profile: run `bladerunner login --bedrock --profile <name>`.\n  \
                 • Existing AWS credential chain: run `bladerunner login --bedrock --chain`.\n\
                 Amazon Bedrock 支持 Bearer token、AWS profile 或现有 AWS 凭证链；\
                 请用以上命令安全写入 Bedrock scope，不会复制 AWS access/secret key。"
                    .into(),
            )
        } else if matches!(arg.as_str(), "opencode-go" | "opencodego") {
            CommandResult::Error(
                "OpenCode Go subscriptions use a Console-issued API key, not portable OAuth. \
                 Subscribe at https://opencode.ai/go, then run \
                 `/providers opencode-go <api_key>` (or set OPENCODE_API_KEY)."
                    .into(),
            )
        } else if matches!(arg.as_str(), "nexus" | "providers" | "provider") {
            // Nexus is BYOK (API key), not OAuth — redirect instead of erroring.
            CommandResult::Error(
                "Nexus 用 API key,不走 OAuth —— 请用 `/nexus <key>`(TUI 内)或 \
                 `bladerunner nexus <key>`(命令行,登录前即可用)。空敲 `/nexus` 查看引导。"
                    .into(),
            )
        } else if arg.is_empty() {
            CommandResult::Error(
                "Missing login target. Use `/login codex` or `/login palantir-aip`.".into(),
            )
        } else {
            CommandResult::Error(format!(
                "Unknown login target '{arg}'. Use `/login codex` or `/login palantir-aip`."
            ))
        }
    }
}

fn is_palantir_target(arg: &str) -> bool {
    matches!(arg, "palantir-aip" | "palantir" | "foundry" | "aip")
}

/// `[oauth|token] [hostname] [client-id]` after `/login palantir-aip`. The
/// client ID keeps its case; the hostname is normalized by the agent.
fn parse_palantir_login(
    words: &[&str],
) -> Result<xai_grok_shell::auth::palantir_aip::LoginRequest, String> {
    let mut request = xai_grok_shell::auth::palantir_aip::LoginRequest::default();
    let mut words = words;
    match words.first().map(|w| w.to_ascii_lowercase()) {
        Some(method) if method == "token" => {
            request.token = true;
            words = &words[1..];
        }
        Some(method) if method == "oauth" => words = &words[1..],
        _ => {}
    }
    match words {
        [] => {}
        [hostname] => request.hostname = Some((*hostname).to_owned()),
        [hostname, client_id] if !request.token => {
            request.hostname = Some((*hostname).to_owned());
            request.client_id = Some((*client_id).to_owned());
        }
        _ => {
            return Err(
                "Usage: `/login palantir-aip [hostname] [client-id]` (browser OAuth) or \
                 `/login palantir-aip token [hostname]` (personal token)."
                    .into(),
            );
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    static EMPTY_BUNDLE: crate::app::bundle::BundleState = crate::app::bundle::BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn dummy_exec_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn login_nexus_redirects_to_nexus_command() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match LoginCommand.run(&mut ctx, "nexus") {
            CommandResult::Error(msg) => assert!(msg.contains("/nexus"), "msg: {msg}"),
            other => panic!("expected redirect Error, got {other:?}"),
        }
    }

    #[test]
    fn hidden_opencode_go_route_remains_available_for_compatibility() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        assert!(!LoginCommand.description().contains("opencode-go"));
        assert!(!LoginCommand.usage().contains("opencode-go"));
        match LoginCommand.run(&mut ctx, "opencode-go") {
            CommandResult::Error(message) => {
                assert!(message.contains("/providers opencode-go <api_key>"));
                assert!(message.contains("OPENCODE_API_KEY"));
                assert!(message.contains("https://opencode.ai/go"));
            }
            other => panic!("expected API-key redirect, got {other:?}"),
        }
    }

    #[test]
    fn login_picker_exposes_supported_accounts() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);

        let picker = LoginCommand
            .suggest_args(
                &crate::slash::command::AppCtx {
                    models: &models,
                    cwd: std::path::Path::new("."),
                    has_session_announcements: false,
                    billing_surface_visible: true,
                    usage_command_visible: true,
                    workflows_available: false,
                    screen_mode: crate::app::ScreenMode::Inline,
                    current_title: None,
                },
                "",
            )
            .expect("/login argument completion exposes the provider choices");
        assert_eq!(picker.len(), 3);
        assert_eq!(picker[0].insert_text, "codex");
        assert_eq!(picker[1].insert_text, "palantir-aip");
        assert_eq!(picker[2].insert_text, "palantir-aip token");

        match LoginCommand.run(&mut ctx, "") {
            CommandResult::Error(message) => {
                assert!(message.contains("/login codex"));
                assert!(message.contains("/login palantir-aip"));
            }
            other => panic!("bare /login must require a target, got {other:?}"),
        }

        assert!(matches!(
            LoginCommand.run(&mut ctx, "xai"),
            CommandResult::Error(_)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "grok"),
            CommandResult::Error(_)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "codex"),
            CommandResult::Action(Action::LoginOpenAiCodex)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "kimi"),
            CommandResult::Action(Action::LoginKimi)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "openai"),
            CommandResult::Action(Action::LoginOpenAiCodex)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "claude"),
            CommandResult::Action(Action::LoginAnthropicClaude)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "copilot"),
            CommandResult::Action(Action::LoginGitHubCopilot)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "radius"),
            CommandResult::Action(Action::LoginRadius)
        ));
    }

    #[test]
    fn palantir_login_parses_method_hostname_and_client_id() {
        use xai_grok_shell::auth::palantir_aip::LoginRequest;
        assert_eq!(parse_palantir_login(&[]).unwrap(), LoginRequest::default());
        assert_eq!(
            parse_palantir_login(&["acme.palantirfoundry.com", "AbC123"]).unwrap(),
            LoginRequest {
                token: false,
                hostname: Some("acme.palantirfoundry.com".into()),
                client_id: Some("AbC123".into()),
            },
            "the client ID keeps its case"
        );
        assert_eq!(
            parse_palantir_login(&["OAuth", "acme.palantirfoundry.com"]).unwrap(),
            LoginRequest {
                hostname: Some("acme.palantirfoundry.com".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            parse_palantir_login(&["token", "acme.palantirfoundry.com"]).unwrap(),
            LoginRequest {
                token: true,
                hostname: Some("acme.palantirfoundry.com".into()),
                client_id: None,
            }
        );
        assert!(parse_palantir_login(&["token", "acme.example", "client"]).is_err());
        assert!(parse_palantir_login(&["a", "b", "c"]).is_err());
    }
}
