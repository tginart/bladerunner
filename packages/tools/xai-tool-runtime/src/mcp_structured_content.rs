//! Purpose: Preserve MCP structured results without a second copy of their JSON.
//! Input: A structured payload and text parts from an MCP result.
//! Output: Optional compact JSON for the model.

use serde::Deserialize;
use serde_json::Value;

/// Purpose: Return JSON only when the content does not already contain the structured payload.
/// Input: An optional payload and the text parts visible to the model.
/// Output: Compact JSON, or None for an absent, null, or duplicate payload.
/// Method: Compare JSON documents at each matching delimiter with the payload.
/// Note: Callers choose the position of the extra JSON text.
pub fn render_structured_content<'a>(
    structured: Option<&Value>,
    parts: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let structured = structured.filter(|v| !v.is_null())?;
    let is_inlined = parts.into_iter().any(|text| match structured {
        Value::Object(_) => contains_document(text, '{', structured),
        Value::Array(_) => contains_document(text, '[', structured),
        Value::String(s) if s == text => true,
        _ => serde_json::from_str::<Value>(text).ok().as_ref() == Some(structured),
    });
    (!is_inlined).then(|| structured.to_string())
}

/// Purpose: Find a matching JSON document after prose or other JSON documents.
/// Input: Text, the document's opening delimiter, and the expected document.
/// Output: True when any candidate document equals the expected document.
fn contains_document(text: &str, open: char, expected: &Value) -> bool {
    for (start, _) in text.match_indices(open) {
        let document = Value::deserialize(&mut serde_json::Deserializer::from_str(&text[start..]));
        if document.as_ref().ok() == Some(expected) {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[path = "mcp_structured_content_tests.rs"]
mod tests;
