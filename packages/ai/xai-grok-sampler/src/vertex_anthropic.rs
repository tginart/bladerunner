//! Anthropic Messages on Google-managed Vertex AI.
//!
//! Purpose: This module changes a typed Anthropic Messages request into the
//! Vertex `rawPredict` / `streamRawPredict` envelope.
//! Note: The Messages serializer, the SSE parser, and the Google ADC token
//! provider stay shared with the other transports. Only the URL and the body
//! envelope are specific to Vertex.

use serde_json::Value;

use xai_grok_sampling_types::{Result, SamplingError, messages::MessagesRequest};

/// Body `anthropic_version` for Vertex. The `anthropic-version` header is not
/// a substitute; Vertex reads only the body field.
pub const VERTEX_ANTHROPIC_VERSION: &str = "2026-10-21";

/// Sampling knobs that the pinned API version rejects on current Claude models.
const STRIPPED_SAMPLING_FIELDS: [&str; 3] = ["temperature", "top_p", "top_k"];

/// Body fields that Vertex carries in the URL or does not accept.
const STRIPPED_ROUTE_FIELDS: [&str; 1] = ["model"];

/// Build the Vertex Anthropic publisher URL for one model.
///
/// Purpose: This function makes the `publishers/anthropic` resource URL.
/// Input: The configured base URL, the model id, and the stream flag.
/// Output: The full request URL, or a configuration error.
/// Method:
///     1. Read the project and location from the same environment variables
///        as the native Google transport.
///     2. Use `streamRawPredict` for streams and `rawPredict` if not.
/// Note: For `global`, the base URL must be `https://aiplatform.googleapis.com`.
/// For a region, use `https://{region}-aiplatform.googleapis.com`.
pub fn url(base_url: &str, model: &str, stream: bool) -> Result<String> {
    let endpoint = crate::google::GoogleEndpoint::from_config(base_url, model);
    let project = endpoint
        .project
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or(SamplingError::InvalidConfiguration(
            "Vertex Messages requires GOOGLE_CLOUD_PROJECT or GCLOUD_PROJECT",
        ))?;
    let location = endpoint
        .location
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or(SamplingError::InvalidConfiguration(
            "Vertex Messages requires GOOGLE_CLOUD_LOCATION (for example `global`)",
        ))?;
    Ok(url_for(
        &endpoint.base_url,
        project,
        location,
        model,
        stream,
    ))
}

/// Serialize a Messages request into the Vertex `rawPredict` body.
///
/// Purpose: This function makes the JSON body that Vertex accepts.
/// Input: The typed Messages request, after the client applied its defaults.
/// Output: A JSON object.
/// Method:
///     1. Serialize the request with the shared Messages serializer.
///     2. Remove `model`, because the URL carries it.
///     3. Remove `temperature`, `top_p` and `top_k`.
///     4. Insert `anthropic_version`.
/// Note: Thinking blocks, signatures, `output_config.effort` and block order
/// pass through unchanged.
pub fn request_body(request: &MessagesRequest) -> Result<Value> {
    let mut body = serde_json::to_value(request).map_err(SamplingError::Serialization)?;
    let object = body
        .as_object_mut()
        .expect("MessagesRequest always serializes to a JSON object");
    for field in STRIPPED_ROUTE_FIELDS
        .iter()
        .chain(&STRIPPED_SAMPLING_FIELDS)
    {
        object.remove(*field);
    }
    object.insert(
        "anthropic_version".to_string(),
        Value::String(VERTEX_ANTHROPIC_VERSION.to_string()),
    );
    Ok(body)
}

fn url_for(base_url: &str, project: &str, location: &str, model: &str, stream: bool) -> String {
    let method = if stream {
        "streamRawPredict"
    } else {
        "rawPredict"
    };
    let base = base_url.trim_end_matches('/');
    format!(
        "{base}/v1/projects/{project}/locations/{location}/publishers/anthropic/models/{model}:{method}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_uses_anthropic_publisher_and_raw_predict_methods() {
        let base = "https://aiplatform.googleapis.com/";
        assert_eq!(
            url_for(base, "proj", "global", "claude-fable-5-2", true),
            "https://aiplatform.googleapis.com/v1/projects/proj/locations/global/publishers/anthropic/models/claude-fable-5-2:streamRawPredict"
        );
        assert!(url_for(base, "proj", "global", "m", false).ends_with("/models/m:rawPredict"));
    }

    #[test]
    fn body_moves_model_to_url_and_adds_version() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-fable-5-2",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.5,
            "top_p": 0.9,
            "stream": true,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "xhigh"}
        }))
        .expect("valid Messages request");
        let body = request_body(&request).expect("body serializes");
        assert!(body.get("model").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("top_k").is_none());
        assert_eq!(body["anthropic_version"], VERTEX_ANTHROPIC_VERSION);
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }
}
