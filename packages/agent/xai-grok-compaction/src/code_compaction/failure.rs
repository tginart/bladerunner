//! Deterministic-vs-transient failure classification for compaction
//! LLM calls.
//!
//! The *policy* lives here (shared across harnesses); the per-harness error
//! types and their wrapping (e.g. grok-build's `SamplingError` →
//! `CompactFailure(acp::Error)`) stay in thin host wrappers that delegate the
//! status/message decisions to these functions.

/// Whether a compaction-call failure is worth retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Retrying the same payload will hit the same failure — the retry loop
    /// should bail without sleeping or re-issuing.
    Deterministic,
    /// Failure may resolve on retry (network blips, 5xx, rate limits).
    Transient,
}

impl FailureKind {
    /// `true` for [`FailureKind::Deterministic`].
    pub fn is_deterministic(self) -> bool {
        matches!(self, Self::Deterministic)
    }
}

/// True when an error message indicates a context-window overflow. Backends report
/// this inconsistently with no stable error code, so we match the message text; it's
/// deterministic (re-sending the same payload always fails), so callers must not retry.
pub fn is_context_length_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("too long for this model")
        || m.contains("prompt is too long")
        || m.contains("maximum prompt length")
        || m.contains("maximum context length")
        // Request-body byte caps on the provider side.
        || m.contains("maximum allowed number of bytes")
        || (m.contains("current message") && m.contains("exceeds budget"))
        || is_anchored(&m, "request too large")
        || has_size_slug(&m)
        || has_rendered_413_phrase(&m)
        || has_input_length_pair(&m)
}

/// True when `message` renders an HTTP 429. A TPM rate limit reads "Request too
/// large for model ... tokens per min" and its `Retry-After` is not part of the
/// rendered text, so callers that only see the text use this to keep a 429
/// retryable instead of treating its size wording as a context overflow.
pub(crate) fn is_rendered_http_429(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("status 429") || m.contains("429 too many requests")
}

/// Classify an HTTP API failure (status + message) for the compaction retry
/// loop.
///
/// 4xx responses other than 408 (timeout) and 429 (rate limit) are
/// deterministic; a context-length overflow message is deterministic
/// regardless of status (backends sometimes dress it as a synthesized 500).
/// Everything else (5xx, 408, 429) is transient.
pub fn classify_http_status(status: u16, message: &str) -> FailureKind {
    // A 429 is a rate limit even when its text reads like a size error.
    if status == 429 {
        return FailureKind::Transient;
    }
    if is_context_length_error(message)
        || ((400..500).contains(&status) && status != 408 && status != 429)
    {
        FailureKind::Deterministic
    } else {
        FailureKind::Transient
    }
}

/// Classify a provider-style stream error event (`ResponseError` /
/// `ResponseFailed.error`) for the compaction retry loop.
///
/// `code` is the structured `code` field on the event (typically a numeric
/// HTTP status as a string, but some providers also use error-type strings like
/// `"invalid_request_error"`). `message` is the human-readable detail.
///
/// Numeric codes are classified by HTTP-status range. The
/// `invalid_request_error` marker, which can appear in either field, always
/// maps to `Deterministic` (schema violations cannot be fixed by re-sending
/// the same payload). The check order is semantic — marker, then numeric
/// code, then context-length message, then default-to-transient.
pub fn classify_stream_event_error(code: Option<&str>, message: &str) -> FailureKind {
    if matches!(code, Some("invalid_request_error")) || message.contains("invalid_request_error") {
        return FailureKind::Deterministic;
    }

    if let Some(status_code) = code.and_then(|c| c.parse::<u16>().ok())
        && (400..500).contains(&status_code)
        && status_code != 408
        && status_code != 429
    {
        return FailureKind::Deterministic;
    }

    // Size overflow arrives here with no parseable code (`code="none"`); the
    // message is the only signal that re-sending cannot help.
    if is_context_length_error(message) {
        return FailureKind::Deterministic;
    }

    FailureKind::Transient
}

/// True when `needle` starts the message or follows a ": " separator. Renderers
/// put error codes and reason phrases there, so request content that echoes the
/// needle in mid-sentence does not match.
fn is_anchored(m: &str, needle: &str) -> bool {
    m.split(": ").any(|segment| segment.starts_with(needle))
}

/// Provider size-error slugs, anchored like rendered error codes.
fn has_size_slug(m: &str) -> bool {
    [
        "context_length_exceeded",
        "exceed_context_size_error",
        "payload_too_large",
        "request_too_large",
    ]
    .iter()
    .any(|slug| is_anchored(m, slug))
}

/// A 413 reason phrase directly after its status code, in the legacy,
/// RFC 9110 and RFC 2616 spellings. The adjacency keeps echoed prose and stray
/// digit runs from matching.
fn has_rendered_413_phrase(m: &str) -> bool {
    m.contains("413 payload too large")
        || m.contains("413 content too large")
        || m.contains("413 request entity too large")
}

/// "Input length (N tokens) exceeds the maximum allowed length (M tokens)".
/// Both halves must match, so a field-length validation error does not
/// classify as a size overflow.
fn has_input_length_pair(m: &str) -> bool {
    m.contains("input length") && m.contains("exceeds the maximum allowed length")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det_status(status: u16) -> bool {
        classify_http_status(status, "test").is_deterministic()
    }

    #[test]
    fn http_4xx_is_deterministic_except_408_and_429() {
        assert!(det_status(400));
        assert!(det_status(401));
        assert!(det_status(403));
        assert!(det_status(404));
        assert!(det_status(413));
        assert!(!det_status(408));
        assert!(!det_status(429));
        assert!(!det_status(500));
        assert!(!det_status(502));
        assert!(!det_status(503));
    }

    #[test]
    fn http_429_with_size_wording_stays_transient() {
        assert!(
            !classify_http_status(
                429,
                "Request too large for model: Limit 30000, Requested 50000 tokens per min"
            )
            .is_deterministic()
        );
    }

    #[test]
    fn rendered_429_detection() {
        assert!(is_rendered_http_429(
            "compact failed: API error (status 429 Too Many Requests): Request too large"
        ));
        assert!(is_rendered_http_429("upstream said 429 Too Many Requests"));
        assert!(!is_rendered_http_429(
            "API error (status 413 Payload Too Large): Request too large"
        ));
    }

    #[test]
    fn http_500_with_context_length_message_is_deterministic() {
        // The sampler synthesizes status=500 from a streamed size overflow, so
        // status alone reads transient; the message must still short-circuit.
        assert!(
            classify_http_status(
                500,
                "API error (status 500 Internal Server Error): \
                 The prompt is too long for this model's context window."
            )
            .is_deterministic()
        );
    }

    #[test]
    fn stream_event_invalid_request_error_marker_is_deterministic() {
        assert!(
            classify_stream_event_error(
                Some("invalid_request_error"),
                "messages.27.content.1: ..."
            )
            .is_deterministic()
        );
        assert!(
            classify_stream_event_error(
                Some("400"),
                "Provider returned invalid_request_error: messages.X..."
            )
            .is_deterministic()
        );
        assert!(
            classify_stream_event_error(None, "messages.X.content.Y: invalid_request_error: ...")
                .is_deterministic()
        );
    }

    #[test]
    fn stream_event_numeric_codes_match_http_classification() {
        let det = |c: &str| classify_stream_event_error(Some(c), "msg").is_deterministic();
        assert!(det("400"));
        assert!(det("401"));
        assert!(det("403"));
        assert!(det("404"));
        assert!(!det("408"));
        assert!(!det("429"));
        assert!(!det("500"));
        assert!(!det("503"));
    }

    #[test]
    fn stream_event_unknown_code_defaults_to_transient() {
        assert!(!classify_stream_event_error(None, "msg").is_deterministic());
        assert!(!classify_stream_event_error(Some("error"), "msg").is_deterministic());
        assert!(!classify_stream_event_error(Some("overloaded_error"), "msg").is_deterministic());
    }

    #[test]
    fn stream_event_context_length_message_is_deterministic() {
        assert!(
            classify_stream_event_error(
                None,
                "The prompt is too long for this model's context window."
            )
            .is_deterministic()
        );
    }

    #[test]
    fn context_length_error_matches_known_messages() {
        for msg in [
            "The prompt is too long for this model's context window.",
            "prompt is too long: 250000 tokens > 200000 maximum",
            "exceeds the maximum prompt length",
            "This model's maximum context length is 128000 tokens",
            "error code: context_length_exceeded",
            "Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "compact failed: API error (status 400 Bad Request): invalid-argument: Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "Current message (600000) exceeds budget (500000)",
            "API error (status 413 Payload Too Large): Request failed (HTTP 413).",
            "API error (status 413 Payload Too Large): payload_too_large: Chat history exceeds the 800-message limit",
            "API error (status 400 Bad Request): exceed_context_size_error: request (300000 tokens) exceeds the model context size",
            "stream error (BAD_REQUEST): Input length (300000 tokens) exceeds the maximum allowed length (200000 tokens)",
            "API error (status 413 Content Too Large): Request failed (HTTP 413).",
            "upstream returned 413 Request Entity Too Large",
            "request exceeds the maximum allowed number of bytes (10485760)",
            "Request too large",
            "compact failed: 413: Request too large",
            "API error (status 429 Too Many Requests): Request too large for model",
            "request_too_large: request rejected",
        ] {
            assert!(is_context_length_error(msg), "should match: {msg}");
        }
        for msg in [
            "internal server error",
            "rate limited",
            "connection reset by peer",
            "Attached file content (300000 tokens) causes message to exceed budget",
            "compact index estimate 2.0 GB exceeds budget 1.0 GB",
            "API error (status 400 Bad Request): invalid tool schema",
            "metadata value exceeds the maximum allowed length (512 characters)",
            "invalid_request_error: field description says request too large sometimes",
            "invalid_request_error: user note says the payload too large banner is confusing",
            "invalid_request_error: user asked what context_length_exceeded means",
            "invalid_request_error: docs mention the request_too_large code",
            "invalid_request_error: line 413 of the doc mentions payload too large limits",
        ] {
            assert!(!is_context_length_error(msg), "should not match: {msg}");
        }
    }
}
