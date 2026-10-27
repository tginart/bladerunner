//! Types for the `search_tool`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for the `search_tool` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchToolInput {
    /// Keywords to match against tool names, server names, and descriptions.
    /// Include the server name and action for best results
    /// (e.g. "linear create issue", "slack read thread history").
    /// For an exact match, use a bare tool name, qualified name, or server and tool name
    /// (e.g. "save_issue", "linear__save_issue", "linear save_issue").
    pub query: String,
    /// Maximum number of results to return (default 5).
    #[serde(default = "default_limit")]
    pub limit: Option<u8>,
}

fn default_limit() -> Option<u8> {
    Some(5)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Purpose: Verify that model-visible query guidance includes exact and natural-language examples.
    /// Input: The generated search input schema.
    /// Output: Assertions for the query description.
    #[test]
    fn query_schema_documents_exact_and_natural_language_searches() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchToolInput)).unwrap();
        let description = schema["properties"]["query"]["description"]
            .as_str()
            .unwrap();

        for guidance in [
            "For an exact match",
            "\"save_issue\"",
            "\"linear__save_issue\"",
            "\"linear save_issue\"",
            "\"linear create issue\"",
            "\"slack read thread history\"",
        ] {
            assert!(
                description.contains(guidance),
                "missing guidance: {guidance}"
            );
        }
    }
}
