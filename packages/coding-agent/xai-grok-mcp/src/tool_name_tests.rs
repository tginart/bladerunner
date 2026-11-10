use super::{
    MCP_QUALIFIED_NAME_MAX_CHARS, McpToolAdmissionError, PROVIDER_TOOL_NAME_MAX_CHARS,
    parse_mcp_qualified_name, parse_mcp_tool_name, qualify_mcp_tool_name, validate_tool_name,
};

#[test]
fn qualified_mcp_name_parser_accepts_structurally_valid_tool_ids() {
    for (name, expected) in [
        ("linear__list_issues", ("linear", "list_issues")),
        ("123__lookup", ("123", "lookup")),
        ("auth__2fa_enable", ("auth", "2fa_enable")),
        ("server:scope__tool", ("server:scope", "tool")),
    ] {
        let (id, server, tool) = parse_mcp_qualified_name(name).expect("valid qualified ID");
        assert_eq!(id.as_str(), name);
        assert_eq!((server, tool), expected);
        assert_eq!(
            parse_mcp_tool_name(name),
            Some((expected.0.to_owned(), expected.1.to_owned()))
        );
    }
}

#[test]
fn qualified_mcp_name_parser_rejects_malformed_names() {
    for name in [
        "server__part__tool",
        "server__tool__part",
        "foo___bar",
        "foo____bar",
        "__tool",
        "server__",
        "server",
        "",
        "server__bad.tool",
    ] {
        assert!(
            parse_mcp_qualified_name(name).is_none(),
            "unexpectedly accepted {name:?}"
        );
    }
}

#[test]
fn qualify_rejects_invalid_segments() {
    // The parser accepts a digit-leading server; admission does not, because
    // the catalog key would then start with a digit.
    assert!(parse_mcp_qualified_name("123__lookup").is_some());
    assert!(matches!(
        qualify_mcp_tool_name("123", "lookup"),
        Err(McpToolAdmissionError::InvalidServerName { .. })
    ));
    assert!(matches!(
        qualify_mcp_tool_name("server:scope", "tool"),
        Err(McpToolAdmissionError::InvalidServerName { .. })
    ));
    assert!(matches!(
        qualify_mcp_tool_name("auth", "bad.tool"),
        Err(McpToolAdmissionError::InvalidToolName { .. })
    ));
    assert!(matches!(
        qualify_mcp_tool_name("server", ""),
        Err(McpToolAdmissionError::InvalidToolName { .. })
    ));
    assert!(matches!(
        qualify_mcp_tool_name("foo_", "bar"),
        Err(McpToolAdmissionError::InvalidOrAmbiguousQualifiedName { .. })
    ));
    assert_eq!(
        qualify_mcp_tool_name("auth", "2fa_enable").as_deref(),
        Ok("auth__2fa_enable")
    );
}

#[test]
fn qualify_enforces_the_catalog_cap_not_the_provider_limit() {
    let server_62 = format!("a{}", "b".repeat(61));
    let over_provider = format!("{server_62}__b");
    assert_eq!(over_provider.len(), PROVIDER_TOOL_NAME_MAX_CHARS + 1);
    assert!(validate_tool_name(&over_provider).is_err());
    assert_eq!(
        qualify_mcp_tool_name(&server_62, "b").as_deref(),
        Ok(over_provider.as_str())
    );

    let tool = "b".repeat(MCP_QUALIFIED_NAME_MAX_CHARS - 3);
    assert_eq!(
        qualify_mcp_tool_name("a", &tool).map(|q| q.len()),
        Ok(MCP_QUALIFIED_NAME_MAX_CHARS)
    );
    assert!(matches!(
        qualify_mcp_tool_name("a", &format!("{tool}x")),
        Err(McpToolAdmissionError::QualifiedNameTooLong { len, .. })
            if len == MCP_QUALIFIED_NAME_MAX_CHARS + 1
    ));
}

#[test]
fn validate_tool_name_keeps_the_provider_rules() {
    assert!(validate_tool_name("read_file").is_ok());
    assert!(validate_tool_name(&"a".repeat(PROVIDER_TOOL_NAME_MAX_CHARS)).is_ok());
    assert!(validate_tool_name(&"a".repeat(PROVIDER_TOOL_NAME_MAX_CHARS + 1)).is_err());
    assert!(validate_tool_name("2fa").is_err());
    assert!(validate_tool_name("bad.name").is_err());
    assert!(validate_tool_name("").is_err());
}
