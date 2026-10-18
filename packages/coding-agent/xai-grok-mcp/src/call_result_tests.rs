use super::{format_mcp_image, mcp_output_from_call_result};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use xai_grok_tools::types::output::MCPOutputDetails;

// A server can return a summary and place the useful IDs only in structuredContent.
const SUMMARY: &str = "7 product folders, 2 custom folders";

fn folders() -> serde_json::Value {
    serde_json::json!({"folders": [{"id": "p1", "name": "Alpha"}, {"id": "c1", "name": "Custom One"}]})
}

fn call_tool_result(
    is_error: bool,
    texts: &[&str],
    structured: Option<serde_json::Value>,
) -> CallToolResult {
    let content = texts.iter().map(|t| ContentBlock::text(*t)).collect();
    let mut result = if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    result.structured_content = structured;
    result
}

fn okay_text(result: CallToolResult) -> String {
    let out = mcp_output_from_call_result("t".into(), "s".into(), result, false);
    match out.output() {
        MCPOutputDetails::OkayOutput(text) => text.clone(),
        MCPOutputDetails::Error(e) => panic!("expected success output, got error: {e}"),
    }
}

fn error_text(result: CallToolResult) -> String {
    let out = mcp_output_from_call_result("t".into(), "s".into(), result, false);
    assert!(out.is_error);
    match out.output() {
        MCPOutputDetails::Error(text) => text.clone(),
        MCPOutputDetails::OkayOutput(text) => panic!("expected error output, got: {text}"),
    }
}

#[test]
fn structured_content_is_appended_when_content_is_only_a_summary() {
    let payload = folders();
    let text = okay_text(call_tool_result(false, &[SUMMARY], Some(payload.clone())));
    assert_eq!(format!("{SUMMARY}\n{payload}"), text);
}

/// Dedupe rules live in the helper's tests; this pins the no-second-copy path end to end.
#[test]
fn structured_content_inlined_as_text_is_not_duplicated() {
    let payload = folders();
    let inlined = payload.to_string();
    let text = okay_text(call_tool_result(false, &[SUMMARY, &inlined], Some(payload)));
    assert_eq!(format!("{SUMMARY}\n{inlined}"), text);
}

#[test]
fn structured_content_is_appended_on_error_results() {
    let structured = serde_json::json!({"code": "NOT_FOUND", "folder_id": "p9"});
    let text = error_text(call_tool_result(
        true,
        &["lookup failed"],
        Some(structured.clone()),
    ));
    assert_eq!(format!("lookup failed\n{structured}"), text);
}

/// Otherwise an image or resource block would reach the model as a data URI inside the error.
#[test]
fn error_results_drop_image_and_resource_blocks() {
    let image = ResourceContents::blob("BBBB", "file:///b.png").with_mime_type("image/png");
    let text = error_text(CallToolResult::error(vec![
        ContentBlock::text("lookup failed"),
        ContentBlock::image("AAAA", "image/png"),
        ContentBlock::resource(image),
    ]));
    assert_eq!("lookup failed", text);
}

#[test]
fn blob_resources_render_as_image_only_with_an_image_mime_type() {
    let image = ResourceContents::blob("AAAA", "file:///a.png").with_mime_type("image/png");
    let pdf = ResourceContents::blob("BBBB", "file:///b.pdf").with_mime_type("application/pdf");
    let untyped = ResourceContents::blob("CCCC", "file:///c.bin");
    let text = okay_text(CallToolResult::success(vec![
        ContentBlock::resource(image),
        ContentBlock::resource(pdf),
        ContentBlock::resource(untyped),
    ]));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(3, lines.len(), "{text}");
    assert_eq!(Some(&"data:image/png;base64,AAAA"), lines.first());
    for (line, blob) in lines.iter().skip(1).zip(["BBBB", "CCCC"]) {
        assert!(
            line.starts_with('{') && line.contains(&format!(r#""blob":"{blob}""#)),
            "non-image blob stays JSON: {line}"
        );
    }
}

#[test]
fn format_mcp_image_default_emits_only_data_uri() {
    let out = format_mcp_image("image/png", "AAAA", false);
    assert_eq!("data:image/png;base64,AAAA", out);
    assert!(!out.contains("<mcp_image_base64"));
}

#[test]
fn format_mcp_image_expose_emits_data_uri_and_raw_block() {
    let out = format_mcp_image("image/png", "AAAA", true);
    assert!(out.contains("data:image/png;base64,AAAA"));
    assert!(out.contains("<mcp_image_base64 mime=\"image/png\">\nAAAA\n</mcp_image_base64>"));
}

#[test]
fn format_mcp_image_expose_raw_block_has_no_data_prefix() {
    let out = format_mcp_image("image/jpeg", "ZZZZ", true);
    assert_eq!(1, out.matches("data:image/").count());
}

/// Purpose: Keep native MCP structured IDs after hub serialization and the output cap.
/// Input: Large success and error results with absent or late inline payload copies.
/// Output: The capped body contains the payload without a session dump.
#[tokio::test]
async fn native_structured_content_survives_output_cap() {
    use xai_grok_tools::types::context::TruncationConfig;
    use xai_grok_tools::types::output::ToolOutput;
    use xai_grok_tools::types::resources::{Resources, TruncationCfg};
    use xai_grok_tools::util::mcp_truncate::{McpTruncateContext, truncate_tool_output};

    let limit = 128;
    let payload = serde_json::json!({"document_id": "doc-123"});
    let mut config = TruncationConfig::default();
    config
        .per_tool_max_output_bytes
        .insert("use_tool".into(), limit);
    let mut resources = Resources::new();
    resources.insert(TruncationCfg(config));
    let mut ctx = xai_tool_runtime::ToolCallContext::default();
    ctx.extensions.insert(resources.into_shared());
    let trunc_ctx = McpTruncateContext::from_tool_ctx(&ctx, "use_tool").await;
    for is_error in [false, true] {
        for is_inlined in [false, true] {
            let mut content = "x".repeat(512);
            if is_inlined {
                content.push_str(&format!("\n{payload}"));
            }
            let output = mcp_output_from_call_result(
                "t".into(),
                "s".into(),
                call_tool_result(is_error, &[&content], Some(payload.clone())),
                false,
            );
            let serialized = serde_json::to_value(ToolOutput::MCP(output)).unwrap();
            let output = serde_json::from_value(serialized).unwrap();
            let ToolOutput::MCP(mcp) = truncate_tool_output(output, &trunc_ctx).await else {
                panic!("expected MCP");
            };
            assert_eq!(mcp.is_error, is_error);
            let text = match mcp.output() {
                MCPOutputDetails::OkayOutput(text) | MCPOutputDetails::Error(text) => text,
            };
            let body = text.split("\n\n[MCP output truncated:").next().unwrap();
            assert!(body.len() <= limit, "{body}");
            assert!(body.contains(&payload.to_string()), "{body}");
        }
    }
}
