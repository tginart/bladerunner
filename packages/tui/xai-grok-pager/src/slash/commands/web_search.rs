//! `/web-search` -- show or change the web search provider.
//!
//! Purpose: This command reads and saves `[web_search] provider` and the Exa
//! and Perplexity API keys. The shell does the work and reports the status.
//! Note: A key in an env var never selects a provider. The user must pick the
//! provider with this command or with `--web-search`.

use crate::app::actions::{Action, WebSearchSettingsRequest};
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

const PROVIDERS: [&str; 4] = ["auto", "off", "exa", "perplexity"];
const KEY_PROVIDERS: [&str; 2] = ["exa", "perplexity"];
const STATUS_WORDS: [&str; 3] = ["settings", "status", "show"];
const CLEAR_WORDS: [&str; 3] = ["clear", "remove", "logout"];

pub const USAGE: &str = "Usage:\n  \
    /web-search                      show the current provider and keys\n  \
    /web-search auto                 Codex native search on Codex models, off otherwise\n  \
    /web-search off                  turn web search off\n  \
    /web-search exa|perplexity       use that provider (key from auth.json or env)\n  \
    /web-search exa|perplexity <key> save the key and use that provider\n  \
    /web-search exa|perplexity clear remove the saved key";

pub struct WebSearchCommand;

impl SlashCommand for WebSearchCommand {
    fn name(&self) -> &str {
        "web-search"
    }

    fn description(&self) -> &str {
        "Choose the web search provider (auto, off, Exa, Perplexity)"
    }

    fn usage(&self) -> &str {
        "/web-search [auto|off|exa|perplexity] [api_key|clear]"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match parse_args(args) {
            Ok(request) => CommandResult::Action(Action::WebSearchSettings(request)),
            Err(message) => CommandResult::Error(message),
        }
    }
}

/*
Purpose: Turn the `/web-search` arguments into a request for the shell.
Input: The text after `/web-search`.
Output: The request, or an error message with the usage.
Method:
    1. No argument or a status word gives an empty request (status only).
    2. A provider alone selects that provider.
    3. A key provider with `clear` removes the saved key.
    4. A key provider with other text saves the text as the key and selects it.
*/
fn parse_args(args: &str) -> Result<WebSearchSettingsRequest, String> {
    let mut words = args.split_whitespace();
    let Some(first) = words.next() else {
        return Ok(WebSearchSettingsRequest::default());
    };
    let first = first.to_ascii_lowercase();
    let rest: Vec<&str> = words.collect();
    if STATUS_WORDS.contains(&first.as_str()) && rest.is_empty() {
        return Ok(WebSearchSettingsRequest::default());
    }
    if !PROVIDERS.contains(&first.as_str()) {
        return Err(format!("Unknown web search provider '{first}'.\n{USAGE}"));
    }
    let is_key_provider = KEY_PROVIDERS.contains(&first.as_str());
    match rest.as_slice() {
        [] => Ok(WebSearchSettingsRequest {
            provider: Some(first),
            ..Default::default()
        }),
        [word] if is_key_provider && CLEAR_WORDS.contains(&word.to_ascii_lowercase().as_str()) => {
            Ok(WebSearchSettingsRequest {
                key_provider: Some(first),
                ..Default::default()
            })
        }
        [key] if is_key_provider => Ok(WebSearchSettingsRequest {
            provider: Some(first.clone()),
            key_provider: Some(first),
            api_key: (*key).to_string(),
        }),
        _ if is_key_provider => Err(format!("An API key has no spaces.\n{USAGE}")),
        _ => Err(format!("'{first}' takes no other arguments.\n{USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        provider: Option<&str>,
        key_provider: Option<&str>,
        key: &str,
    ) -> WebSearchSettingsRequest {
        WebSearchSettingsRequest {
            provider: provider.map(str::to_string),
            key_provider: key_provider.map(str::to_string),
            api_key: key.to_string(),
        }
    }

    #[test]
    fn empty_and_status_words_show_status() {
        for args in ["", "  ", "settings", "Status"] {
            assert_eq!(
                parse_args(args),
                Ok(WebSearchSettingsRequest::default()),
                "{args:?}"
            );
        }
    }

    #[test]
    fn provider_alone_selects_it() {
        assert_eq!(parse_args("OFF"), Ok(request(Some("off"), None, "")));
        assert_eq!(parse_args("exa"), Ok(request(Some("exa"), None, "")));
    }

    #[test]
    fn key_saves_and_selects() {
        assert_eq!(
            parse_args("perplexity pplx-123"),
            Ok(request(Some("perplexity"), Some("perplexity"), "pplx-123"))
        );
    }

    #[test]
    fn clear_removes_key_only() {
        assert_eq!(parse_args("exa clear"), Ok(request(None, Some("exa"), "")));
    }

    #[test]
    fn bad_input_is_an_error() {
        assert!(
            parse_args("bing")
                .unwrap_err()
                .contains("Unknown web search provider")
        );
        assert!(
            parse_args("auto key")
                .unwrap_err()
                .contains("takes no other arguments")
        );
        assert!(parse_args("exa a b").unwrap_err().contains("no spaces"));
    }
}
