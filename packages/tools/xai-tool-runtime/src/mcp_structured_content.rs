//! Purpose: Preserve MCP structured results without a second copy of their JSON.
//! Input: A structured payload and text parts from an MCP result.
//! Output: Optional compact JSON for the model.

use serde::Deserialize;
use serde_json::Value;

/// Purpose: Return JSON only when the content does not already contain the structured payload.
/// Input: An optional payload and the text parts visible to the model.
/// Output: Compact JSON, or None for an absent, null, or duplicate payload.
/// Method: Compare the first JSON document in each text part with the payload.
/// Note: Callers choose the position of the extra JSON text.
pub fn render_structured_content<'a>(
    structured: Option<&Value>,
    parts: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let structured = structured.filter(|v| !v.is_null())?;
    let is_inlined = parts.into_iter().any(|text| {
        let carried = match structured {
            Value::Object(_) => document_at(text, '{'),
            Value::Array(_) => document_at(text, '['),
            Value::String(s) if s == text => return true,
            _ => serde_json::from_str(text).ok(),
        };
        carried.as_ref() == Some(structured)
    });
    (!is_inlined).then(|| structured.to_string())
}

fn document_at(text: &str, open: char) -> Option<Value> {
    let start = text.find(open)?;
    Value::deserialize(&mut serde_json::Deserializer::from_str(&text[start..])).ok()
}

#[cfg(test)]
#[path = "mcp_structured_content_tests.rs"]
mod tests;
