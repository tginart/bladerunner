//! Web search provider selection.
//!
//! Purpose: This module turns the user's `[web_search] provider` setting into
//! the `WebSearchConfig` for the local `web_search` tool.
//! Method:
//!     1. `off` removes web search. This includes Codex native search.
//!     2. `exa` and `perplexity` apply only when the user selects them and a
//!        key is available. The stored key has priority over the env var.
//!     3. `auto` (the default) gives DuckDuckGo. On Codex models the sampler
//!        uses the native hosted search and hides the local tool.
//! Note: A detected env var does not select a provider. Only an explicit
//! setting or `--web-search` flag does.

use serde::{Deserialize, Serialize};
use xai_grok_tools::implementations::WebSearchConfig;

const EXA_ENV_VARS: &[&str] = &["EXA_API_KEY"];
const PERPLEXITY_ENV_VARS: &[&str] = &["PERPLEXITY_API_KEY", "PPLX_API_KEY"];

/// The `[web_search] provider` value in config.toml.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchProvider {
    /// Codex native search on Codex models, DuckDuckGo otherwise.
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

/// Build the tool config for `provider` from a stored key and an env key.
///
/// Pure. A key-based provider without a key falls back to DuckDuckGo, so the
/// user keeps a working search after a bad setup.
pub fn resolve_web_search_config(
    provider: WebSearchProvider,
    stored_key: Option<String>,
    env_key: Option<String>,
) -> WebSearchConfig {
    let key = stored_key.or(env_key);
    match (provider, key) {
        (WebSearchProvider::Off, _) => WebSearchConfig::Disabled,
        (WebSearchProvider::Auto, _) => WebSearchConfig::DuckDuckGo,
        (WebSearchProvider::Exa, Some(api_key)) => WebSearchConfig::Exa { api_key },
        (WebSearchProvider::Perplexity, Some(api_key)) => WebSearchConfig::Perplexity { api_key },
        (provider, None) => {
            tracing::warn!(
                provider = provider.as_str(),
                "web search provider has no API key; using DuckDuckGo"
            );
            WebSearchConfig::DuckDuckGo
        }
    }
}

/// Read the key for `provider` from auth.json and env, then resolve.
pub fn load_web_search_config(provider: WebSearchProvider) -> WebSearchConfig {
    let stored_key = if provider.needs_api_key() {
        crate::auth::read_web_search_api_key(&xai_grok_config::grok_home(), provider.as_str())
    } else {
        None
    };
    let env_key = first_env_key(provider).map(|(_, value)| value);
    resolve_web_search_config(provider, stored_key, env_key)
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

/// Codex hosted search is used only in `auto` mode, so an explicit provider
/// or `off` always wins over it.
pub fn allows_hosted_search(config: &WebSearchConfig) -> bool {
    matches!(config, WebSearchConfig::DuckDuckGo)
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
    fn auto_and_off_ignore_keys() {
        let key = || Some("k".to_string());
        assert_eq!(
            resolve_web_search_config(WebSearchProvider::Auto, key(), key()),
            WebSearchConfig::DuckDuckGo
        );
        assert_eq!(
            resolve_web_search_config(WebSearchProvider::Off, key(), key()),
            WebSearchConfig::Disabled
        );
    }

    #[test]
    fn stored_key_beats_env_key() {
        assert_eq!(
            resolve_web_search_config(
                WebSearchProvider::Exa,
                Some("stored".into()),
                Some("env".into())
            ),
            WebSearchConfig::Exa {
                api_key: "stored".into()
            }
        );
        assert_eq!(
            resolve_web_search_config(WebSearchProvider::Perplexity, None, Some("env".into())),
            WebSearchConfig::Perplexity {
                api_key: "env".into()
            }
        );
    }

    #[test]
    fn missing_key_falls_back_to_duckduckgo() {
        assert_eq!(
            resolve_web_search_config(WebSearchProvider::Exa, None, None),
            WebSearchConfig::DuckDuckGo
        );
    }

    #[test]
    fn hosted_search_only_in_auto_mode() {
        assert!(allows_hosted_search(&WebSearchConfig::DuckDuckGo));
        assert!(!allows_hosted_search(&WebSearchConfig::Disabled));
        assert!(!allows_hosted_search(&WebSearchConfig::Exa {
            api_key: "k".into()
        }));
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
