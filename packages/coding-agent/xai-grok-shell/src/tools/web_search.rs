//! Web search provider selection.
//!
//! Purpose: This module turns the user's `[web_search] provider` setting into
//! the `WebSearchSelection` of a session.
//! Method:
//!     1. `off` removes web search. This includes Codex native search.
//!     2. `exa` and `perplexity` apply only when the user selects them and a
//!        key is available. The stored key has priority over the env var.
//!     3. `auto` (the default) permits only provider-hosted search. Codex
//!        models get native search. Other models get no web search.
//! Note: A detected env var does not select a provider. Only an explicit
//! setting or `--web-search` flag does.

use serde::{Deserialize, Serialize};
use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::implementations::WebSearchConfig;
use xai_grok_tools::implementations::web_search::client::WebSearchClient;

const EXA_ENV_VARS: &[&str] = &["EXA_API_KEY"];
const PERPLEXITY_ENV_VARS: &[&str] = &["PERPLEXITY_API_KEY", "PPLX_API_KEY"];

/// The `[web_search] provider` value in config.toml.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchProvider {
    /// Codex native search on Codex models, no web search otherwise.
    #[default]
    Auto,
    /// No web search.
    Off,
    /// Exa search API (needs a key).
    Exa,
    /// Perplexity search API (needs a key).
    Perplexity,
}

/// The `[web_search]` table in config.toml.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchSettings {
    #[serde(default)]
    pub provider: WebSearchProvider,
}

impl WebSearchSettings {
    /// Read `[web_search]` from the raw config. A bad value gives the default.
    pub fn from_raw_config(raw_config: &toml::Value) -> Self {
        let Some(table) = raw_config.get("web_search") else {
            return Self::default();
        };
        table.clone().try_into().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "invalid [web_search] config; using defaults");
            Self::default()
        })
    }
}

impl WebSearchProvider {
    pub const ALL: [Self; 4] = [Self::Auto, Self::Off, Self::Exa, Self::Perplexity];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Exa => "exa",
            Self::Perplexity => "perplexity",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str().eq_ignore_ascii_case(value.trim()))
    }

    pub fn needs_api_key(self) -> bool {
        matches!(self, Self::Exa | Self::Perplexity)
    }

    /// Standard env var names checked for this provider's key.
    pub fn key_env_vars(self) -> &'static [&'static str] {
        match self {
            Self::Exa => EXA_ENV_VARS,
            Self::Perplexity => PERPLEXITY_ENV_VARS,
            Self::Auto | Self::Off => &[],
        }
    }
}

/// Where a provider key came from. Holds names only, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    Stored,
    Env(&'static str),
    Missing,
}

/// The live web search choice of one session.
///
/// The session keeps it as a tool bridge resource and reads it on each
/// request, so `/web-search` changes a running session without a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchSelection {
    /// Client config for the local `web_search` tool.
    pub local: WebSearchConfig,
    /// Whether provider-hosted search (Codex native) can run.
    pub allows_hosted: bool,
}

impl WebSearchSelection {
    /// No local search and no hosted search.
    pub fn off() -> Self {
        Self {
            local: WebSearchConfig::Disabled,
            allows_hosted: false,
        }
    }

    /// Build the selection for `provider` from a stored key and an env key.
    ///
    /// Pure. A key-based provider without a key gives no search. It does not
    /// fall back to another provider.
    pub fn resolve(
        provider: WebSearchProvider,
        stored_key: Option<String>,
        env_key: Option<String>,
    ) -> Self {
        Self {
            local: resolve_web_search_config(provider, stored_key.or(env_key)),
            allows_hosted: provider == WebSearchProvider::Auto,
        }
    }
}

/// Read the key for `provider` from auth.json and env, then resolve.
pub fn load_web_search_selection(provider: WebSearchProvider) -> WebSearchSelection {
    let stored_key = if provider.needs_api_key() {
        crate::auth::read_web_search_api_key(&xai_grok_config::grok_home(), provider.as_str())
    } else {
        None
    };
    let env_key = first_env_key(provider).map(|(_, value)| value);
    WebSearchSelection::resolve(provider, stored_key, env_key)
}

/*
Purpose: Install a web search selection on the tool bridge of a session.
Input: The tool bridge. The new selection.
Output: None. The bridge resources change.
Method:
    1. If the selection has a local config, insert a new `WebSearchClient`.
    2. If not, remove the old `WebSearchClient`.
    3. Insert the selection. The session reads it on each request.
*/
pub async fn install_web_search_selection(bridge: &ToolBridge, selection: WebSearchSelection) {
    let resources = bridge.shared_resources().await;
    let mut resources = resources.lock().await;
    match WebSearchClient::new(&selection.local) {
        Ok(client) => resources.insert(client),
        Err(_) => {
            resources.remove::<WebSearchClient>();
        }
    }
    resources.insert(selection);
}

/// Report where `provider`'s key would come from, without the key itself.
pub fn key_source(provider: WebSearchProvider) -> KeySource {
    if !provider.needs_api_key() {
        return KeySource::Missing;
    }
    let home = xai_grok_config::grok_home();
    if crate::auth::read_web_search_api_key(&home, provider.as_str()).is_some() {
        return KeySource::Stored;
    }
    match first_env_key(provider) {
        Some((name, _)) => KeySource::Env(name),
        None => KeySource::Missing,
    }
}

/// Pure. Map a provider and its key to the local tool config.
fn resolve_web_search_config(provider: WebSearchProvider, key: Option<String>) -> WebSearchConfig {
    match (provider, key) {
        (WebSearchProvider::Off | WebSearchProvider::Auto, _) => WebSearchConfig::Disabled,
        (WebSearchProvider::Exa, Some(api_key)) => WebSearchConfig::Exa { api_key },
        (WebSearchProvider::Perplexity, Some(api_key)) => WebSearchConfig::Perplexity { api_key },
        (provider, None) => {
            tracing::warn!(
                provider = provider.as_str(),
                "web search provider has no API key; web search is off"
            );
            WebSearchConfig::Disabled
        }
    }
}

fn first_env_key(provider: WebSearchProvider) -> Option<(&'static str, String)> {
    for name in provider.key_env_vars() {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return Some((name, value.trim().to_owned()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_allows_only_hosted_search() {
        let key = || Some("k".to_string());
        assert_eq!(
            WebSearchSelection::resolve(WebSearchProvider::Auto, key(), key()),
            WebSearchSelection {
                local: WebSearchConfig::Disabled,
                allows_hosted: true,
            }
        );
        assert_eq!(
            WebSearchSelection::resolve(WebSearchProvider::Off, key(), key()),
            WebSearchSelection::off()
        );
    }

    #[test]
    fn stored_key_beats_env_key() {
        assert_eq!(
            WebSearchSelection::resolve(
                WebSearchProvider::Exa,
                Some("stored".into()),
                Some("env".into())
            ),
            WebSearchSelection {
                local: WebSearchConfig::Exa {
                    api_key: "stored".into()
                },
                allows_hosted: false,
            }
        );
        assert_eq!(
            WebSearchSelection::resolve(WebSearchProvider::Perplexity, None, Some("env".into()))
                .local,
            WebSearchConfig::Perplexity {
                api_key: "env".into()
            }
        );
    }

    #[test]
    fn missing_key_turns_search_off() {
        assert_eq!(
            WebSearchSelection::resolve(WebSearchProvider::Perplexity, None, None),
            WebSearchSelection::off()
        );
    }

    #[tokio::test]
    async fn install_swaps_and_removes_the_client() {
        let bridge = ToolBridge::for_test();
        let perplexity =
            WebSearchSelection::resolve(WebSearchProvider::Perplexity, Some("k".into()), None);
        install_web_search_selection(&bridge, perplexity.clone()).await;
        let client = bridge.read_resource::<WebSearchClient>().await.unwrap();
        assert_eq!(client.provider_name(), "Perplexity");
        assert_eq!(
            bridge.read_resource::<WebSearchSelection>().await,
            Some(perplexity)
        );
        install_web_search_selection(&bridge, WebSearchSelection::off()).await;
        assert!(bridge.read_resource::<WebSearchClient>().await.is_none());
    }

    #[test]
    fn settings_parse_from_toml() {
        let settings: WebSearchSettings = toml::from_str("provider = \"perplexity\"").unwrap();
        assert_eq!(settings.provider, WebSearchProvider::Perplexity);
        let settings: WebSearchSettings = toml::from_str("").unwrap();
        assert_eq!(settings.provider, WebSearchProvider::Auto);
        assert_eq!(
            WebSearchProvider::parse(" EXA "),
            Some(WebSearchProvider::Exa)
        );
        assert_eq!(WebSearchProvider::parse("bing"), None);
        let raw: toml::Value = toml::from_str("[web_search]\nprovider = \"exa\"").unwrap();
        assert_eq!(
            WebSearchSettings::from_raw_config(&raw).provider,
            WebSearchProvider::Exa
        );
        let raw: toml::Value = toml::from_str("[web_search]\nprovider = \"bing\"").unwrap();
        assert_eq!(
            WebSearchSettings::from_raw_config(&raw),
            WebSearchSettings::default()
        );
    }
}
