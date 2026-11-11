//! Anthropic Messages on Google-managed Vertex AI.
//!
//! Purpose: This module changes a typed Anthropic Messages request into the
//! Vertex `rawPredict` / `streamRawPredict` envelope.
//! Note: The Messages serializer, the SSE parser, and the Google ADC token
//! provider stay shared with the other transports. Only the URL path, the
//! headers, and the body envelope are specific to Vertex.

use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use xai_grok_sampling_types::{Result, SamplingError, messages::MessagesRequest};

/// Body `anthropic_version` for Vertex. The `anthropic-version` header is not
/// a substitute; Vertex reads only the body field.
pub const VERTEX_ANTHROPIC_VERSION: &str = "2026-10-21";

/// Sampling knobs that the pinned API version rejects on current Claude models.
const STRIPPED_SAMPLING_FIELDS: [&str; 3] = ["temperature", "top_p", "top_k"];

/// Body fields that Vertex carries in the URL or does not accept.
const STRIPPED_ROUTE_FIELDS: [&str; 1] = ["model"];

/// Exact Vertex hosts: global, and the `us` / `eu` multi-region endpoints.
/// Regional hosts are `{region}-aiplatform.googleapis.com`.
const VERTEX_EXACT_HOSTS: [&str; 3] = [
    "aiplatform.googleapis.com",
    "aiplatform.us.rep.googleapis.com",
    "aiplatform.eu.rep.googleapis.com",
];
const VERTEX_REGIONAL_HOST_SUFFIX: &str = "-aiplatform.googleapis.com";

/// Build the Vertex Anthropic publisher path for one model.
///
/// Purpose: This function makes the `publishers/anthropic` resource path.
/// Input: The configured base URL, the model id, and the stream flag.
/// Output: The path with no leading slash, or a configuration error.
/// Method:
///     1. Make sure that the base URL is an HTTPS Vertex host. The caller
///        sends a Google ADC bearer to this host.
///     2. Read the project and the location from the same environment
///        variables as the Gemini Vertex route.
///     3. Use `streamRawPredict` for streams and `rawPredict` if not.
/// Note: For `global`, the base URL must be `https://aiplatform.googleapis.com`.
/// For the `us` or `eu` multi-region, use `https://aiplatform.{us|eu}.rep.googleapis.com`.
/// For a region, use `https://{region}-aiplatform.googleapis.com`.
pub fn path(base_url: &str, model: &str, stream: bool) -> Result<String> {
    require_vertex_host(base_url)?;
    let endpoint = crate::google::GoogleEndpoint::from_config(base_url, model);
    let location_path = endpoint.vertex_location_path()?;
    Ok(publisher_path(&location_path, model, stream))
}

/// Build the request headers for one Vertex Anthropic request.
///
/// Purpose: This function makes the headers that Google receives.
/// Input: The client default headers, the ADC access token, and the stream
/// flag.
/// Output: A new header map.
/// Method:
///     1. Remove the configured API keys, the `x-grok-*` headers, and the
///        `anthropic-version` header.
///     2. Add the ADC bearer, the JSON content type, and the `Accept` header.
/// Note: `anthropic-beta` and other provider headers pass through.
pub fn headers(default_headers: &HeaderMap, token: &str, stream: bool) -> HeaderMap {
    let mut headers = default_headers.clone();
    crate::client::remove_known_auth_headers(&mut headers);
    crate::client::strip_x_grok_headers(&mut headers);
    headers.remove(HeaderName::from_static("anthropic-version"));
    crate::google::apply_google_auth_headers(&mut headers, None, Some(token));
    if !stream {
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    }
    headers
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

fn publisher_path(location_path: &str, model: &str, stream: bool) -> String {
    let method = if stream {
        "streamRawPredict"
    } else {
        "rawPredict"
    };
    format!("{location_path}/publishers/anthropic/models/{model}:{method}")
}

fn require_vertex_host(base_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(base_url).map_err(|_| {
        SamplingError::InvalidConfiguration("Vertex Messages base_url is not a valid URL")
    })?;
    let host = url.host_str().unwrap_or_default();
    let is_vertex_host =
        VERTEX_EXACT_HOSTS.contains(&host) || host.ends_with(VERTEX_REGIONAL_HOST_SUFFIX);
    if url.scheme() == "https" && is_vertex_host {
        return Ok(());
    }
    Err(SamplingError::InvalidConfiguration(
        "Vertex Messages base_url must be https://aiplatform.googleapis.com, https://aiplatform.{us|eu}.rep.googleapis.com, or https://{region}-aiplatform.googleapis.com",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_uses_anthropic_publisher_and_raw_predict_methods() {
        let location_path = "v1/projects/proj/locations/global";
        assert_eq!(
            publisher_path(location_path, "claude-fable-5-2", true),
            "v1/projects/proj/locations/global/publishers/anthropic/models/claude-fable-5-2:streamRawPredict"
        );
        assert!(publisher_path(location_path, "m", false).ends_with("/models/m:rawPredict"));
    }

    #[test]
    fn vertex_host_check_accepts_only_https_google_hosts() {
        assert!(require_vertex_host("https://aiplatform.googleapis.com").is_ok());
        assert!(require_vertex_host("https://us-east5-aiplatform.googleapis.com/").is_ok());
        assert!(require_vertex_host("https://aiplatform.us.rep.googleapis.com").is_ok());
        assert!(require_vertex_host("https://aiplatform.eu.rep.googleapis.com").is_ok());
        for base in [
            "http://aiplatform.googleapis.com",
            "https://api.anthropic.com",
            "https://evil.example/aiplatform.googleapis.com",
            "https://aiplatform.googleapis.com.evil.example",
            "https://aiplatform.evil.rep.googleapis.com.example",
            "not a url",
        ] {
            assert!(
                require_vertex_host(base).is_err(),
                "{base} must be rejected"
            );
        }
    }

    #[test]
    fn headers_drop_configured_credentials_and_add_adc_bearer() {
        let mut defaults = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer configured-key"),
            ("x-api-key", "sk-ant-configured"),
            ("anthropic-version", "2023-06-01"),
            ("x-grok-conv-id", "conv"),
            ("anthropic-beta", "some-beta"),
        ] {
            defaults.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let headers = headers(&defaults, "adc-token", false);
        assert_eq!(headers["authorization"], "Bearer adc-token");
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("anthropic-version").is_none());
        assert!(headers.get("x-grok-conv-id").is_none());
        assert_eq!(headers["anthropic-beta"], "some-beta");
        assert_eq!(headers["accept"], "application/json");
        assert_eq!(
            super::headers(&defaults, "adc-token", true)["accept"],
            "text/event-stream"
        );
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
