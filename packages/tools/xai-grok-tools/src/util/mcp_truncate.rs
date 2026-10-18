//! Shared size-bounding for MCP/text tool output.
//!
//! Large payloads (e.g. Sentry attachment base64 resources) must not land
//! fully in chat state: they inflate the token estimate and trigger premature
//! auto-compact.
//!
//! # Configurable limit
//!
//! Default [`MCP_MAX_OUTPUT_BYTES`] (20_000). Effective limit (highest first):
//!
//! 1. [`TruncationCfg`](crate::types::resources::TruncationCfg) per-tool /
//!    MCP-specific (`mcp_max_output_bytes` — e.g. a winning repo-level
//!    `[mcp] max_output_bytes`, seeded per session by the shell) / default,
//!    when present in resources
//! 2. Host-seeded effective limit via [`set_mcp_max_output_bytes`] (host
//!    resolves requirements > env > config > remote config > default once at
//!    bootstrap / remote-config refresh and stores the result)
//! 3. When host has not seeded (`0`): env
//!    [`ENV_GROK_MAX_MCP_OUTPUT_BYTES`] / [`ENV_MAX_MCP_OUTPUT_BYTES`]
//! 4. Built-in default

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use xai_tool_runtime::ToolCallContext;

use crate::types::output::{MCPOutputDetails, ToolOutput};
use crate::types::tool::ToolKind;
use crate::util::base64_images::{ExtractionResult, extract_base64_images};
use crate::util::query_tools::{QueryTools, examples_clause};
use crate::util::truncate::format_bytes;

/// Default inline limit for MCP tool output in chat state (bytes, not tokens).
pub const MCP_MAX_OUTPUT_BYTES: usize = 20_000;

/// Env override for the MCP inline output cap (bytes).
/// Some agents use `MAX_MCP_OUTPUT_TOKENS`; we bound by **bytes** because
/// truncation is byte-oriented (`truncate_str`).
pub const ENV_MAX_MCP_OUTPUT_BYTES: &str = "MAX_MCP_OUTPUT_BYTES";

/// Grok-native env override for the MCP inline output cap (bytes).
pub const ENV_GROK_MAX_MCP_OUTPUT_BYTES: &str = "GROK_MAX_MCP_OUTPUT_BYTES";

/// Process-wide effective limit. `0` = host has not seeded; fall through to
/// env / default. The shell writes the *fully resolved* stack here so free-
/// function tool dispatch (no live `Config`) sees the same value.
static EFFECTIVE_MCP_MAX_OUTPUT_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Host (shell) sets the fully-resolved MCP output cap in bytes.
///
/// Pass the already-resolved limit (requirements > env > config > remote config >
/// default). Pass `0` only in tests to clear and fall through to env / default.
pub fn set_mcp_max_output_bytes(bytes: usize) {
    EFFECTIVE_MCP_MAX_OUTPUT_BYTES.store(bytes, Ordering::Relaxed);
}

/// Parse a positive byte limit from an env var. Zero / unparseable → `None`.
fn parse_positive_bytes_env(name: &str) -> Option<usize> {
    let raw = std::env::var(name).ok()?;
    let n = raw.trim().parse::<u64>().ok()?;
    usize::try_from(n).ok().filter(|n| *n > 0)
}

/// Env tier: `GROK_MAX_MCP_OUTPUT_BYTES` then `MAX_MCP_OUTPUT_BYTES`.
///
/// Grok-native wins when both are set. Positive integers only. Used by the
/// shell resolver and as the standalone fallback when the host has not called
/// [`set_mcp_max_output_bytes`].
pub fn mcp_max_output_bytes_from_env() -> Option<usize> {
    parse_positive_bytes_env(ENV_GROK_MAX_MCP_OUTPUT_BYTES)
        .or_else(|| parse_positive_bytes_env(ENV_MAX_MCP_OUTPUT_BYTES))
}

/// Effective MCP inline output cap for this process.
///
/// Host-seeded value if set; otherwise env; otherwise [`MCP_MAX_OUTPUT_BYTES`].
pub fn mcp_max_output_bytes() -> usize {
    match EFFECTIVE_MCP_MAX_OUTPUT_BYTES.load(Ordering::Relaxed) {
        0 => mcp_max_output_bytes_from_env().unwrap_or(MCP_MAX_OUTPUT_BYTES),
        n => n,
    }
}

pub(crate) const LONG_LINE_BYTES: usize = 2_000;

/// How a truncated MCP payload is saved and described to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpDumpKind {
    LongLineJson,
    Json,
    LongLineText,
    Other,
}

impl McpDumpKind {
    pub(crate) fn classify(text: &str) -> Self {
        let trimmed = text.trim();
        let is_json = matches!(trimmed.as_bytes().first(), Some(b'{' | b'['))
            && serde_json::from_str::<serde::de::IgnoredAny>(trimmed).is_ok();
        let has_long_line = text.lines().map(str::len).max().unwrap_or(0) > LONG_LINE_BYTES;
        match (is_json, has_long_line) {
            (true, true) => Self::LongLineJson,
            (true, false) => Self::Json,
            (false, true) => Self::LongLineText,
            (false, false) => Self::Other,
        }
    }

    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::LongLineJson | Self::Json => "json",
            Self::LongLineText | Self::Other => "txt",
        }
    }

    pub(crate) fn steer(self, shell: &str, tools: QueryTools) -> String {
        match self {
            Self::LongLineJson => format!(
                " The full output is valid JSON with a very long line, so \
                 grep/read_file are ineffective on it — use `{shell}` to query the \
                 saved file{eg}.",
                eg = examples_clause(&tools.json_tools()),
            ),
            Self::Json => format!(
                " The full output is valid JSON saved to the file above; use \
                 `{shell}` to query it{eg}.",
                eg = examples_clause(&tools.json_tools()),
            ),
            Self::LongLineText => format!(
                " The full output has a very long line, so grep/read_file are \
                 ineffective on it — use `{shell}` to slice/search the saved \
                 file{eg}.",
                eg = examples_clause(&tools.text_tools()),
            ),
            Self::Other => String::new(),
        }
    }
}

/// Resolved settings for truncating one MCP payload (inline limit, dump dir,
/// shell tool name, call id). Build with [`McpTruncateContext::from_tool_ctx`].
pub struct McpTruncateContext {
    pub(crate) max_output_bytes: usize,
    pub(crate) session_folder: Option<PathBuf>,
    pub(crate) shell_tool: String,
    pub(crate) call_id: String,
}

impl McpTruncateContext {
    pub async fn from_tool_ctx(ctx: &ToolCallContext, tool_key: &str) -> Self {
        let call_id = ctx.call_id.as_str().to_string();
        let resolved_default = mcp_max_output_bytes();
        match crate::types::tool_metadata::shared_resources(ctx) {
            Ok(res) => {
                let guard = res.lock().await;
                let max_output_bytes = guard
                    .get::<crate::types::resources::TruncationCfg>()
                    .map(|cfg| cfg.0.mcp_max_output_bytes_for(tool_key, resolved_default))
                    .unwrap_or(resolved_default);
                let session_folder = guard
                    .get::<crate::types::resources::SessionFolder>()
                    .map(|f| f.0.clone());
                let shell_tool = guard
                    .get::<crate::types::template_renderer::TemplateRenderer>()
                    .and_then(|r| r.tool_for_kind(ToolKind::Execute))
                    .map(str::to_string)
                    .unwrap_or_else(|| "bash".to_string());
                Self {
                    max_output_bytes,
                    session_folder,
                    shell_tool,
                    call_id,
                }
            }
            Err(_) => Self {
                max_output_bytes: resolved_default,
                session_folder: None,
                shell_tool: "bash".to_string(),
                call_id,
            },
        }
    }
}

/// Map a `call_id` to safe filename chars so a `/` or `..` in a wire-supplied
/// id (only validated as non-empty) cannot escape the session `mcp/` dir.
fn sanitized_stem(call_id: &str) -> String {
    call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Purpose: Save full MCP text and keep a bounded inline preview.
/// Input: Full text, truncation settings, and an optional structured payload.
/// Output: Text contains the preview, fitting structured data, and a truncation notice.
async fn truncate_mcp_text(
    text: &mut String,
    trunc_ctx: &McpTruncateContext,
    structured: Option<&serde_json::Value>,
) {
    if text.len() <= trunc_ctx.max_output_bytes {
        return;
    }

    let total_bytes = text.len();
    let kind = McpDumpKind::classify(text.as_str());

    let output_file_path = trunc_ctx.session_folder.as_ref().map(|folder| {
        folder.join("mcp").join(format!(
            "{}.{}",
            sanitized_stem(&trunc_ctx.call_id),
            kind.extension()
        ))
    });

    let file_hint = if let Some(ref path) = output_file_path {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        match tokio::fs::write(path, text.as_bytes()).await {
            Ok(()) => format!(" Full output written to: {}.", path.to_string_lossy()),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to write full MCP output to file"
                );
                String::new()
            }
        }
    } else {
        String::new()
    };

    let structured_preview = structured_mcp_preview(text, structured, trunc_ctx.max_output_bytes);
    let preview_label = if structured_preview.is_some() {
        "showing"
    } else {
        "showing first"
    };
    let truncated = structured_preview.unwrap_or_else(|| {
        crate::util::truncate::truncate_str(text, trunc_ctx.max_output_bytes).to_owned()
    });
    let steer = if file_hint.is_empty() {
        String::new()
    } else {
        kind.steer(&trunc_ctx.shell_tool, QueryTools::detect())
    };
    *text = format!(
        "{}\n\n[MCP output truncated: {} {} of {}.{}{}]",
        truncated,
        preview_label,
        format_bytes(trunc_ctx.max_output_bytes as u64),
        format_bytes(total_bytes as u64),
        file_hint,
        steer,
    );
}

/// Purpose: Reserve preview bytes for structured JSON that the ordinary prefix would omit.
/// Input: Full text, an optional structured payload, and the inline byte limit.
/// Output: A bounded preview, or None when the ordinary prefix is sufficient.
/// Note: Oversized structured payloads follow the usual prefix and full-dump policy.
fn structured_mcp_preview(
    text: &str,
    structured: Option<&serde_json::Value>,
    max_output_bytes: usize,
) -> Option<String> {
    let prefix = crate::util::truncate::truncate_str(text, max_output_bytes);
    let payload = xai_tool_runtime::render_structured_content(structured, [prefix])?;
    if payload.len() > max_output_bytes {
        return None;
    }
    let prefix_bytes = max_output_bytes.saturating_sub(payload.len() + 1);
    let prefix = crate::util::truncate::truncate_str(text, prefix_bytes);
    if prefix.is_empty() {
        Some(payload)
    } else {
        Some(format!("{prefix}\n{payload}"))
    }
}

/// Bound the `MCP`/`Text` variants to the inline size limit, keeping a preview
/// and dumping the text that remains after any MCP image extract. Other
/// variants pass through. MCP data-URI images go into `extracted_images`
/// before the bound.
pub async fn truncate_tool_output(
    mut output: ToolOutput,
    trunc_ctx: &McpTruncateContext,
) -> ToolOutput {
    match &mut output {
        ToolOutput::MCP(mcp) => {
            let raw = match mcp.output_mut() {
                MCPOutputDetails::OkayOutput(t) | MCPOutputDetails::Error(t) => std::mem::take(t),
            };
            let ExtractionResult { mut text, images } = extract_base64_images(raw);
            mcp.extracted_images = images;
            let structured = mcp.structured_content.take();
            truncate_mcp_text(&mut text, trunc_ctx, structured.as_ref()).await;
            match mcp.output_mut() {
                MCPOutputDetails::OkayOutput(t) | MCPOutputDetails::Error(t) => {
                    *t = text;
                }
            }
        }
        ToolOutput::Text(text_out) => {
            truncate_mcp_text(&mut text_out.text, trunc_ctx, None).await;
        }
        _ => {}
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::base64_images::IMAGE_CONTENT_PLACEHOLDER;

    /// Purpose: Keep structured previews within tiny and multibyte byte budgets.
    /// Input: A small payload, a large payload, and text with early or late JSON copies.
    /// Output: Only a missing payload that fits the cap receives reserved bytes.
    #[test]
    fn structured_preview_respects_existing_copy_and_utf8() {
        let payload = serde_json::json!({"id": "图"});
        let json = payload.to_string();
        let text = "é".repeat(100);
        let preview = structured_mcp_preview(&text, Some(&payload), 16).unwrap();
        assert!(preview.len() <= 16);
        assert!(preview.ends_with(&json));
        assert_eq!(
            structured_mcp_preview(&text, Some(&payload), json.len()),
            Some(json.clone())
        );
        assert_eq!(structured_mcp_preview(&text, Some(&payload), 1), None);
        assert_eq!(structured_mcp_preview(&text, Some(&payload), 0), None);
        let early_copy = format!("{json}\n{text}");
        assert_eq!(
            structured_mcp_preview(&early_copy, Some(&payload), 32),
            None
        );
    }

    /// Purpose: Preserve the original dump and image while the preview reserves structured IDs.
    /// Input: A large MCP result with an image and a small structured payload.
    /// Output: Bounded text retains the IDs, the full dump stays complete, and metadata is consumed.
    #[tokio::test]
    async fn structured_content_preview_preserves_dump_and_image() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with_folder(dir.path().to_path_buf(), 128);
        let payload = serde_json::json!({"document_id": "doc-123"});
        let image_payload = "A".repeat(2048);
        let full = format!(
            "data:image/png;base64,{image_payload} {}\n{payload}",
            "x".repeat(512)
        );
        let expected_dump = extract_base64_images(full.clone()).text;
        let mut mcp = crate::types::output::MCPOutput::okay_output("t".into(), "s".into(), full);
        mcp.structured_content = Some(payload.clone());
        let output = truncate_tool_output(ToolOutput::MCP(mcp), &cfg).await;
        let ToolOutput::MCP(mcp) = output else {
            panic!("expected MCP");
        };
        assert!(mcp.structured_content.is_none());
        assert_eq!(mcp.extracted_images.len(), 1);
        assert_eq!(mcp.extracted_images[0].data, image_payload);
        let MCPOutputDetails::OkayOutput(text) = mcp.output() else {
            panic!("expected success");
        };
        let body = text.split("\n\n[MCP output truncated:").next().unwrap();
        assert!(body.len() <= cfg.max_output_bytes);
        assert!(body.contains(&payload.to_string()));
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("mcp/call-test.txt"))
                .await
                .unwrap(),
            expected_dump
        );
    }

    /// Purpose: Keep oversized structured data bounded after the inner dispatch metadata is consumed.
    /// Input: A payload larger than tiny output caps, without a session directory.
    /// Output: The usual text prefix stays bounded and serialized output has no hidden full payload.
    #[tokio::test]
    async fn oversized_structured_content_stays_bounded_without_dump() {
        let payload = serde_json::json!({"data": "x".repeat(512)});
        for limit in [0, 1, 2, 16] {
            let cfg = McpTruncateContext {
                max_output_bytes: limit,
                session_folder: None,
                shell_tool: "bash".into(),
                call_id: "call-test".into(),
            };
            let mut mcp = crate::types::output::MCPOutput::okay_output(
                "t".into(),
                "s".into(),
                format!("summary\n{payload}"),
            );
            mcp.structured_content = Some(payload.clone());
            let output = truncate_tool_output(ToolOutput::MCP(mcp), &cfg).await;
            let serialized = serde_json::to_value(&output).unwrap();
            assert!(serialized.get("structured_content").is_none());
            assert!(!serialized.to_string().contains(&"x".repeat(512)));
            let ToolOutput::MCP(mcp) = output else {
                panic!("expected MCP");
            };
            let MCPOutputDetails::OkayOutput(text) = mcp.output() else {
                panic!("expected success");
            };
            assert!(
                text.split("\n\n[MCP output truncated:")
                    .next()
                    .unwrap()
                    .len()
                    <= limit
            );
        }
    }

    /// Purpose: Retain one early JSON copy without unnecessary preview changes.
    /// Input: A payload already present before the output cap.
    /// Output: The ordinary prefix remains unchanged and contains one payload copy.
    #[tokio::test]
    async fn structured_content_in_retained_prefix_is_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with_folder(dir.path().to_path_buf(), 128);
        let payload = serde_json::json!({"document_id": "doc-123"});
        let full = format!("{payload}\n{}", "x".repeat(512));
        let mut mcp =
            crate::types::output::MCPOutput::okay_output("t".into(), "s".into(), full.clone());
        mcp.structured_content = Some(payload.clone());
        let output = truncate_tool_output(ToolOutput::MCP(mcp), &cfg).await;
        let ToolOutput::MCP(mcp) = output else {
            panic!("expected MCP");
        };
        assert!(mcp.structured_content.is_none());
        let MCPOutputDetails::OkayOutput(text) = mcp.output() else {
            panic!("expected success");
        };
        let body = text.split("\n\n[MCP output truncated:").next().unwrap();
        assert_eq!(body, &full[..128]);
        assert_eq!(body.matches(&payload.to_string()).count(), 1);
    }

    fn cfg_with_folder(folder: PathBuf, max: usize) -> McpTruncateContext {
        McpTruncateContext {
            max_output_bytes: max,
            session_folder: Some(folder),
            shell_tool: "bash".to_string(),
            call_id: "call-test".to_string(),
        }
    }

    /// Serialize tests that mutate the process-global effective limit / env.
    fn with_mcp_limit_lock<R>(f: impl FnOnce() -> R) -> R {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        f()
    }

    #[test]
    fn host_set_overrides_env_fallback() {
        with_mcp_limit_lock(|| {
            let prev = EFFECTIVE_MCP_MAX_OUTPUT_BYTES.load(Ordering::Relaxed);
            // Clear host seed; with no env, effective limit is the built-in default.
            set_mcp_max_output_bytes(0);
            let prev_max = std::env::var(ENV_MAX_MCP_OUTPUT_BYTES).ok();
            let prev_grok = std::env::var(ENV_GROK_MAX_MCP_OUTPUT_BYTES).ok();
            unsafe {
                std::env::remove_var(ENV_MAX_MCP_OUTPUT_BYTES);
                std::env::remove_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES);
            }
            assert_eq!(
                mcp_max_output_bytes(),
                MCP_MAX_OUTPUT_BYTES,
                "unset host + unset env → built-in default"
            );

            set_mcp_max_output_bytes(10_000);
            assert_eq!(mcp_max_output_bytes(), 10_000, "host seed wins over env");

            set_mcp_max_output_bytes(0);
            assert_eq!(
                mcp_max_output_bytes(),
                MCP_MAX_OUTPUT_BYTES,
                "cleared host falls through to default"
            );

            unsafe {
                match prev_max {
                    Some(v) => std::env::set_var(ENV_MAX_MCP_OUTPUT_BYTES, v),
                    None => std::env::remove_var(ENV_MAX_MCP_OUTPUT_BYTES),
                }
                match prev_grok {
                    Some(v) => std::env::set_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES, v),
                    None => std::env::remove_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES),
                }
            }
            set_mcp_max_output_bytes(prev);
        });
    }

    #[test]
    fn env_parser_rejects_zero_and_junk() {
        with_mcp_limit_lock(|| {
            let prev_max = std::env::var(ENV_MAX_MCP_OUTPUT_BYTES).ok();
            let prev_grok = std::env::var(ENV_GROK_MAX_MCP_OUTPUT_BYTES).ok();
            unsafe {
                std::env::remove_var(ENV_MAX_MCP_OUTPUT_BYTES);
                std::env::remove_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES);
            }
            assert_eq!(mcp_max_output_bytes_from_env(), None);

            unsafe { std::env::set_var(ENV_MAX_MCP_OUTPUT_BYTES, "0") };
            assert_eq!(mcp_max_output_bytes_from_env(), None);

            unsafe { std::env::set_var(ENV_MAX_MCP_OUTPUT_BYTES, "not-a-number") };
            assert_eq!(mcp_max_output_bytes_from_env(), None);

            unsafe { std::env::set_var(ENV_MAX_MCP_OUTPUT_BYTES, "12345") };
            assert_eq!(mcp_max_output_bytes_from_env(), Some(12_345));

            // GROK_* wins over MAX_* when both set.
            unsafe { std::env::set_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES, "99999") };
            assert_eq!(mcp_max_output_bytes_from_env(), Some(99_999));

            unsafe { std::env::remove_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES) };
            assert_eq!(mcp_max_output_bytes_from_env(), Some(12_345));

            unsafe {
                match prev_max {
                    Some(v) => std::env::set_var(ENV_MAX_MCP_OUTPUT_BYTES, v),
                    None => std::env::remove_var(ENV_MAX_MCP_OUTPUT_BYTES),
                }
                match prev_grok {
                    Some(v) => std::env::set_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES, v),
                    None => std::env::remove_var(ENV_GROK_MAX_MCP_OUTPUT_BYTES),
                }
            }
        });
    }

    #[tokio::test]
    async fn text_over_limit_truncates_and_dumps_full_payload() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with_folder(dir.path().to_path_buf(), 100);
        let full = "x".repeat(5_000);

        let out = truncate_tool_output(ToolOutput::Text(full.clone().into()), &cfg).await;

        let ToolOutput::Text(t) = out else {
            panic!("expected Text");
        };
        assert!(t.text.len() < full.len());
        assert!(t.text.starts_with(&"x".repeat(100)), "preview prefix kept");
        assert!(t.text.contains("[MCP output truncated:"));
        assert!(t.text.contains("Full output written to:"));

        let dump = dir.path().join("mcp").join("call-test.txt");
        assert_eq!(tokio::fs::read_to_string(&dump).await.unwrap(), full);
    }

    #[tokio::test]
    async fn boundary_exact_limit_untouched_one_over_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with_folder(dir.path().to_path_buf(), 100);

        let at = truncate_tool_output(ToolOutput::Text("a".repeat(100).into()), &cfg).await;
        let ToolOutput::Text(t) = at else {
            panic!("expected Text")
        };
        assert_eq!(t.text, "a".repeat(100), "exactly at limit is untouched");
        assert!(
            !dir.path().join("mcp").exists(),
            "no dump when not truncated"
        );

        let over = truncate_tool_output(ToolOutput::Text("b".repeat(101).into()), &cfg).await;
        let ToolOutput::Text(t) = over else {
            panic!("expected Text")
        };
        assert!(
            t.text.contains("[MCP output truncated:"),
            "one over truncates"
        );
    }

    #[tokio::test]
    async fn traversal_in_call_id_cannot_escape_session_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = McpTruncateContext {
            max_output_bytes: 100,
            session_folder: Some(dir.path().to_path_buf()),
            shell_tool: "bash".to_string(),
            call_id: "../../evil".to_string(),
        };

        let out = truncate_tool_output(ToolOutput::Text("x".repeat(5_000).into()), &cfg).await;

        let ToolOutput::Text(t) = out else {
            panic!("expected Text");
        };
        let mcp_dir = dir.path().join("mcp");
        assert!(!t.text.contains(".."), "no traversal sequence in pointer");
        let entries: Vec<_> = std::fs::read_dir(&mcp_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one dump file");
        assert!(entries[0].starts_with(&mcp_dir), "dump stayed inside mcp/");
    }

    #[tokio::test]
    async fn non_text_variant_passes_through() {
        let cfg = McpTruncateContext {
            max_output_bytes: 1,
            session_folder: None,
            shell_tool: "bash".to_string(),
            call_id: "call-test".to_string(),
        };

        let out = truncate_tool_output(
            ToolOutput::SearchTool(crate::types::output::SearchToolOutput {
                result_count: 1,
                content: "anything".to_string(),
            }),
            &cfg,
        )
        .await;

        let ToolOutput::SearchTool(s) = out else {
            panic!("expected SearchTool");
        };
        assert_eq!(s.content, "anything", "passthrough leaves content intact");
    }

    #[tokio::test]
    async fn mcp_data_uri_image_survives_truncate_intact() {
        let dir = tempfile::tempdir().unwrap();
        let max = 20_000;
        let cfg = cfg_with_folder(dir.path().to_path_buf(), max);

        let payload = "A".repeat(100_000);
        let prose = "x".repeat(30_000);
        let full = format!("before data:image/png;base64,{payload} after\n{prose}");
        assert!(
            full.len() > max,
            "fixture must exceed inline cap (got {})",
            full.len()
        );

        let out = truncate_tool_output(
            ToolOutput::MCP(crate::types::output::MCPOutput::okay_output(
                "browser_screenshot".into(),
                "browser-use".into(),
                full,
            )),
            &cfg,
        )
        .await;

        let ToolOutput::MCP(mcp) = out else {
            panic!("expected MCP");
        };
        assert_eq!(mcp.extracted_images.len(), 1);
        assert_eq!(mcp.extracted_images[0].mime_type, "image/png");
        assert_eq!(mcp.extracted_images[0].data, payload);
        assert_eq!(mcp.extracted_images[0].data.len(), 100_000);

        let MCPOutputDetails::OkayOutput(text) = mcp.output() else {
            panic!("expected OkayOutput");
        };
        assert!(
            text.contains(IMAGE_CONTENT_PLACEHOLDER),
            "data URI replaced with placeholder"
        );
        assert!(
            !text.contains("data:image"),
            "no data URI left for session mid-chop extraction"
        );
        assert!(
            !text.contains(&payload),
            "full payload must not remain inline"
        );
        // Truncation footer is appended after the bound, so allow overhead.
        let body_end = text
            .find("\n\n[MCP output truncated:")
            .unwrap_or(text.len());
        assert!(
            body_end <= max,
            "inline body must stay within cap: body={body_end} max={max}"
        );
        assert!(
            text.contains("[MCP output truncated:"),
            "large leftover prose still triggers truncate"
        );

        // Dump is post-extraction (full leftover text), not the raw data URI.
        let dump = dir.path().join("mcp").join("call-test.txt");
        let dump_text = tokio::fs::read_to_string(&dump).await.unwrap();
        assert!(
            dump_text.contains(IMAGE_CONTENT_PLACEHOLDER),
            "dump should keep the placeholder"
        );
        assert!(
            !dump_text.contains("data:image"),
            "dump must not re-embed the data URI"
        );
        assert!(
            !dump_text.contains(&payload),
            "dump must not contain the raw base64 payload"
        );
        assert!(
            dump_text.contains(&prose),
            "dump keeps full post-extraction prose (not the truncated preview)"
        );
    }

    #[tokio::test]
    async fn mcp_image_only_under_cap_still_extracts() {
        let cfg = McpTruncateContext {
            max_output_bytes: 20_000,
            session_folder: None,
            shell_tool: "bash".to_string(),
            call_id: "call-img".to_string(),
        };
        let payload = "B".repeat(50_000);
        let full = format!("data:image/jpeg;base64,{payload}");

        let out = truncate_tool_output(
            ToolOutput::MCP(crate::types::output::MCPOutput::okay_output(
                "shot".into(),
                "srv".into(),
                full,
            )),
            &cfg,
        )
        .await;

        let ToolOutput::MCP(mcp) = out else {
            panic!("expected MCP");
        };
        assert_eq!(mcp.extracted_images.len(), 1);
        assert_eq!(mcp.extracted_images[0].mime_type, "image/jpeg");
        assert_eq!(mcp.extracted_images[0].data, payload);

        let MCPOutputDetails::OkayOutput(text) = mcp.output() else {
            panic!("expected OkayOutput");
        };
        assert_eq!(text, IMAGE_CONTENT_PLACEHOLDER);
        assert!(!text.contains("[MCP output truncated:"));
    }

    #[tokio::test]
    async fn mcp_multi_image_extract_then_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let max = 20_000;
        let cfg = cfg_with_folder(dir.path().to_path_buf(), max);

        let p1 = "A".repeat(40_000);
        let p2 = "B".repeat(50_000);
        let prose = "z".repeat(25_000);
        let full =
            format!("one data:image/png;base64,{p1} two data:image/jpeg;base64,{p2} end\n{prose}");

        let out = truncate_tool_output(
            ToolOutput::MCP(crate::types::output::MCPOutput::okay_output(
                "multi".into(),
                "srv".into(),
                full,
            )),
            &cfg,
        )
        .await;

        let ToolOutput::MCP(mcp) = out else {
            panic!("expected MCP");
        };
        assert_eq!(mcp.extracted_images.len(), 2);
        assert_eq!(mcp.extracted_images[0].mime_type, "image/png");
        assert_eq!(mcp.extracted_images[0].data, p1);
        assert_eq!(mcp.extracted_images[1].mime_type, "image/jpeg");
        assert_eq!(mcp.extracted_images[1].data, p2);

        let MCPOutputDetails::OkayOutput(text) = mcp.output() else {
            panic!("expected OkayOutput");
        };
        assert_eq!(text.matches(IMAGE_CONTENT_PLACEHOLDER).count(), 2);
        assert!(!text.contains("data:image"));
        assert!(!text.contains(&p1));
        assert!(!text.contains(&p2));
        assert!(text.contains("[MCP output truncated:"));

        let dump = tokio::fs::read_to_string(dir.path().join("mcp").join("call-test.txt"))
            .await
            .unwrap();
        assert_eq!(dump.matches(IMAGE_CONTENT_PLACEHOLDER).count(), 2);
        assert!(!dump.contains("data:image"));
    }

    #[tokio::test]
    async fn mcp_error_path_extracts_before_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let max = 20_000;
        let cfg = cfg_with_folder(dir.path().to_path_buf(), max);

        let payload = "C".repeat(80_000);
        let prose = "e".repeat(30_000);
        // Space terminator so LF-wrapped prose is not absorbed into the payload.
        let full = format!("err data:image/png;base64,{payload} done\n{prose}");

        let out = truncate_tool_output(
            ToolOutput::MCP(crate::types::output::MCPOutput::errored(
                "failing_tool".into(),
                "srv".into(),
                full,
            )),
            &cfg,
        )
        .await;

        let ToolOutput::MCP(mcp) = out else {
            panic!("expected MCP");
        };
        assert!(mcp.is_error);
        assert_eq!(mcp.extracted_images.len(), 1);
        assert_eq!(mcp.extracted_images[0].mime_type, "image/png");
        assert_eq!(mcp.extracted_images[0].data, payload);

        let MCPOutputDetails::Error(text) = mcp.output() else {
            panic!("expected Error");
        };
        assert!(text.contains(IMAGE_CONTENT_PLACEHOLDER));
        assert!(!text.contains("data:image"));
        assert!(!text.contains(&payload));
        assert!(text.contains("[MCP output truncated:"));
    }
}
