/// Backend for the local `web_search` tool.
///
/// `Disabled` removes the tool. `DuckDuckGo` needs no key and is the
/// default when no provider is configured. `Exa` and `Perplexity` are
/// used only when the user selects them explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum WebSearchConfig {
    #[default]
    Disabled,
    DuckDuckGo,
    Exa {
        api_key: String,
    },
    Perplexity {
        api_key: String,
    },
}

impl WebSearchConfig {
    /// Returns `true` unless the config is `Disabled`.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Short provider label for logs and status lines.
    pub fn provider_name(&self) -> &'static str {
        match self {
            Self::Disabled => "off",
            Self::DuckDuckGo => "duckduckgo",
            Self::Exa { .. } => "exa",
            Self::Perplexity { .. } => "perplexity",
        }
    }

    /// Return a copy safe for returning to clients, with any key replaced.
    pub fn redacted(&self) -> Self {
        let hidden = "***REDACTED***".to_string();
        match self {
            Self::Disabled => Self::Disabled,
            Self::DuckDuckGo => Self::DuckDuckGo,
            Self::Exa { .. } => Self::Exa { api_key: hidden },
            Self::Perplexity { .. } => Self::Perplexity { api_key: hidden },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        assert!(!WebSearchConfig::default().is_enabled());
    }

    #[test]
    fn redacted_hides_key() {
        let config = WebSearchConfig::Exa {
            api_key: "secret".to_string(),
        };
        assert_eq!(
            config.redacted(),
            WebSearchConfig::Exa {
                api_key: "***REDACTED***".to_string()
            }
        );
        assert_eq!(
            WebSearchConfig::DuckDuckGo.redacted(),
            WebSearchConfig::DuckDuckGo
        );
    }

    #[test]
    fn serde_roundtrip() {
        let config = WebSearchConfig::Perplexity {
            api_key: "key".to_string(),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(json, r#"{"provider":"perplexity","api_key":"key"}"#);
        let parsed: WebSearchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, config);
    }
}
