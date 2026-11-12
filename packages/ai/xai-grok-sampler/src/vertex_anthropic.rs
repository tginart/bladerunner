//! Anthropic Messages on Google-managed Vertex AI.
//!
//! Purpose: This module changes a typed Anthropic Messages request into the
//! Vertex `rawPredict` / `streamRawPredict` envelope.
//! Note: The Messages serializer, the SSE parser, and the Google ADC token
//! provider stay shared with the other transports. Only the URL path, the
//! headers, and the body envelope are specific to Vertex.

use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;

use xai_grok_sampling_types::{Result, SamplingError, messages::MessagesRequest};

/// Body `anthropic_version` for Vertex. The `anthropic-version` header is not
/// a substitute; Vertex reads only the body field.
pub const VERTEX_ANTHROPIC_VERSION: &str = "2026-10-21";

/// Exact Vertex hosts: global, and the `us` multi-region endpoint.
/// Regional hosts are `{region}-aiplatform.googleapis.com`.
const VERTEX_EXACT_HOSTS: [&str; 2] = [
    "aiplatform.googleapis.com",
    "aiplatform.us.rep.googleapis.com",
];
const VERTEX_REGIONAL_HOST_SUFFIX: &str = "-aiplatform.googleapis.com";

/// Shown when Google rejects the ADC credential. A Bladerunner `/login` does
/// not fix this, so the error must not trigger the re-login prompt.
pub const VERTEX_ADC_REJECTED_MESSAGE: &str = "Google Vertex AI rejected the Google ADC credential (401). Run `gcloud auth application-default login`, or check that the account has Vertex AI access to the project.";

/// How the response comes back: one SSE stream, or one JSON body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMode {
    Stream,
    Single,
}

/// Build the Vertex Anthropic publisher path for one model.
///
/// Purpose: This function makes the `publishers/anthropic` resource path.
/// Input: The configured base URL, the model id, and the response mode.
/// Output: The path with no leading slash, or a configuration error.
/// Method:
///     1. Make sure that the base URL is an HTTPS Vertex host. The caller
///        sends a Google ADC bearer to this host.
///     2. Read the project and the location from the same environment
///        variables as the Gemini Vertex route.
///     3. Use `streamRawPredict` for a stream and `rawPredict` if not.
/// Note: For `global`, the base URL must be `https://aiplatform.googleapis.com`.
/// For the `us` multi-region, use `https://aiplatform.us.rep.googleapis.com`.
/// For a region, use `https://{region}-aiplatform.googleapis.com`.
pub fn path(base_url: &str, model: &str, mode: ResponseMode) -> Result<String> {
    require_vertex_host(base_url)?;
    let endpoint = crate::google::GoogleEndpoint::from_config(base_url, model);
    let location_path = endpoint.vertex_location_path()?;
    Ok(publisher_path(&location_path, model, mode))
}

/// Build the request headers for one Vertex Anthropic request.
///
/// Purpose: This function makes the headers that Google receives.
/// Input: The client default headers, the ADC access token, and the response
/// mode.
/// Output: A new header map.
/// Method:
///     1. Remove the configured API keys, the `x-grok-*` headers, and the
///        `anthropic-version` header.
///     2. Add the ADC bearer, the JSON content type, and the `Accept` header.
/// Note: `anthropic-beta` and other provider headers pass through.
pub fn headers(default_headers: &HeaderMap, token: &str, mode: ResponseMode) -> HeaderMap {
    let mut headers = default_headers.clone();
    crate::client::remove_known_auth_headers(&mut headers);
    crate::client::strip_x_grok_headers(&mut headers);
    headers.remove(HeaderName::from_static("anthropic-version"));
    crate::google::apply_google_auth_headers(&mut headers, None, Some(token));
    if mode == ResponseMode::Single {
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    }
    headers
}

/// Strip a Messages request in place for the Vertex `rawPredict` body.
///
/// Purpose: This function removes the fields that Vertex does not accept.
/// Input: The typed Messages request, after the client applied its defaults.
/// Method:
///     1. Clear `model`, because the URL carries it. An empty model is not
///        serialized.
///     2. Remove `temperature`, `top_p` and `top_k`.
/// Note: Thinking blocks, signatures, `output_config.effort` and block order
/// do not change.
pub fn strip_request(request: &mut MessagesRequest) {
    request.model.clear();
    request.temperature = None;
    request.top_p = None;
    request.top_k = None;
}

/// The Vertex body: a stripped request plus `anthropic_version`.
///
/// It borrows the request, so serde writes it straight into the HTTP body
/// with no copy of the conversation.
#[derive(Serialize)]
pub struct VertexBody<'a> {
    anthropic_version: &'static str,
    #[serde(flatten)]
    request: &'a MessagesRequest,
}

impl<'a> VertexBody<'a> {
    pub fn new(request: &'a MessagesRequest) -> Self {
        Self {
            anthropic_version: VERTEX_ANTHROPIC_VERSION,
            request,
        }
    }
}

pub(crate) fn publisher_path(location_path: &str, model: &str, mode: ResponseMode) -> String {
    let method = match mode {
        ResponseMode::Stream => "streamRawPredict",
        ResponseMode::Single => "rawPredict",
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
        "Vertex Messages base_url must be https://aiplatform.googleapis.com, https://aiplatform.us.rep.googleapis.com, or https://{region}-aiplatform.googleapis.com",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_uses_anthropic_publisher_and_raw_predict_methods() {
        let location_path = "v1/projects/proj/locations/global";
        assert_eq!(
            publisher_path(location_path, "claude-fable-5-2", ResponseMode::Stream),
            "v1/projects/proj/locations/global/publishers/anthropic/models/claude-fable-5-2:streamRawPredict"
        );
        assert!(
            publisher_path(location_path, "m", ResponseMode::Single)
                .ends_with("/models/m:rawPredict")
        );
    }

    #[test]
    fn vertex_host_check_accepts_only_https_google_hosts() {
        assert!(require_vertex_host("https://aiplatform.googleapis.com").is_ok());
        assert!(require_vertex_host("https://us-east5-aiplatform.googleapis.com/").is_ok());
        assert!(require_vertex_host("https://aiplatform.us.rep.googleapis.com").is_ok());
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
        let headers = headers(&defaults, "adc-token", ResponseMode::Single);
        assert_eq!(headers["authorization"], "Bearer adc-token");
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("anthropic-version").is_none());
        assert!(headers.get("x-grok-conv-id").is_none());
        assert_eq!(headers["anthropic-beta"], "some-beta");
        assert_eq!(headers["accept"], "application/json");
        assert_eq!(
            super::headers(&defaults, "adc-token", ResponseMode::Stream)["accept"],
            "text/event-stream"
        );
    }

    #[test]
    fn body_moves_model_to_url_and_adds_version() {
        let mut request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-fable-5-2",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.5,
            "top_p": 0.9,
            "top_k": 40,
            "stream": true,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "xhigh"}
        }))
        .expect("valid Messages request");
        strip_request(&mut request);
        let body = serde_json::to_value(VertexBody::new(&request)).expect("body serializes");
        assert!(body.get("model").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("top_k").is_none());
        assert_eq!(body["anthropic_version"], VERTEX_ANTHROPIC_VERSION);
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }
}
