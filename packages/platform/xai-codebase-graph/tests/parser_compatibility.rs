/*
Purpose: Check parser and query compatibility for every source language.
Input: Small source files that define and call a function.
Output: Assertions for valid syntax and symbol extraction.
*/

use tree_sitter::Parser;
use xai_codebase_graph::{LanguageRegistry, build_scope_graph, extract_symbols_fast};

/*
Purpose: Verify that each grammar still supports both symbol extraction paths.
Input: A function definition and a call in each supported language.
Output: Assertions for the syntax tree, definition, and reference.
*/
#[test]
fn supported_languages_preserve_definitions_and_references() {
    let registry = LanguageRegistry::new();
    let fixtures = [
        ("rs", "fn greet() {}\nfn main() { greet(); }\n"),
        ("ts", "function greet(): void {}\ngreet();\n"),
        ("js", "function greet() {}\ngreet();\n"),
        ("py", "def greet():\n    pass\ngreet()\n"),
        (
            "go",
            "package main\nfunc greet() {}\nfunc main() { greet() }\n",
        ),
    ];

    for (extension, source) in fixtures {
        let config = registry.for_extension(extension).unwrap();
        let language = config.language();
        let query = config.compile_query().unwrap();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "syntax error for {extension}"
        );

        let source_bytes = source.as_bytes();
        let result = build_scope_graph(&query, tree.root_node(), source_bytes, &config);
        assert!(
            result
                .graph
                .find_definition("greet", source_bytes)
                .is_some(),
            "missing definition for {extension}"
        );
        assert!(
            !result
                .graph
                .find_references("greet", source_bytes)
                .is_empty(),
            "missing reference for {extension}"
        );

        let (definitions, references, _) =
            extract_symbols_fast(&query, tree.root_node(), source_bytes, &config);
        assert!(
            definitions.iter().any(|(name, _)| name.as_ref() == "greet"),
            "missing fast definition for {extension}"
        );
        assert!(
            references.iter().any(|(name, _)| name.as_ref() == "greet"),
            "missing fast reference for {extension}"
        );
    }
}
