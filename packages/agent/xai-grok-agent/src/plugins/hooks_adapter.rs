//! Plugin hooks adapter: pre-filter plugin hook JSON, then feed it to
//! `xai-grok-hooks`' parser and inject plugin env vars. Not a second engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xai_grok_hooks::config::{HookProvenance, HookSpec, parse_hook_file};
use xai_grok_hooks::discovery::HookRegistry;
use xai_grok_hooks::event::HookEventName;

use super::manifest::substitute_env_vars;
use super::registry::{LoadedPlugin, PluginRegistry};

/// Read, pre-filter, parse, and env-inject a plugin's hooks file.
pub fn parse_plugin_hooks(
    hooks_path: &Path,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<HookSpec>, Vec<String>) {
    let content = match std::fs::read_to_string(hooks_path) {
        Ok(c) => c,
        Err(e) => {
            return (
                vec![],
                vec![format!(
                    "plugin {plugin_name}: failed to read hooks file {}: {e}",
                    hooks_path.display()
                )],
            );
        }
    };

    let (specs, warnings) =
        process_hooks_content(&content, hooks_path, plugin_name, plugin_root, plugin_data);
    tracing::debug!(
        plugin = plugin_name,
        hooks_count = specs.len(),
        warnings = warnings.len(),
        "plugin hooks loaded from file"
    );
    (specs, warnings)
}

/// Like [`parse_plugin_hooks`] for an inline manifest hooks value (no file I/O).
pub fn parse_plugin_hooks_from_value(
    value: &serde_json::Value,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<HookSpec>, Vec<String>) {
    let content = serde_json::to_string(value).unwrap_or_default();
    // Use a synthetic path for parse_hook_file's source_dir (resolves relative commands).
    let synthetic_path = Path::new(plugin_root).join("plugin.json");
    let (specs, warnings) = process_hooks_content(
        &content,
        &synthetic_path,
        plugin_name,
        plugin_root,
        plugin_data,
    );
    tracing::debug!(
        plugin = plugin_name,
        hooks_count = specs.len(),
        warnings = warnings.len(),
        "plugin hooks loaded from manifest inline"
    );
    (specs, warnings)
}

/// Parses the hooks file and inline manifest hooks of `plugin`, logging each parse warning once.
pub(crate) fn load_plugin_hook_specs(plugin: &LoadedPlugin) -> Vec<HookSpec> {
    let root = plugin.root_str();
    let data = plugin.data_dir_str();
    let parsed = [
        plugin
            .hooks_path
            .as_deref()
            .map(|path| parse_plugin_hooks(path, &plugin.name, &root, &data)),
        plugin
            .inline_hooks
            .as_ref()
            .map(|value| parse_plugin_hooks_from_value(value, &plugin.name, &root, &data)),
    ];
    let mut specs = Vec::new();
    for (plugin_specs, warnings) in parsed.into_iter().flatten() {
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
        specs.extend(plugin_specs);
    }
    specs
}

/// Hooks of each active (enabled and trusted) plugin in `registry`, parsed when the registry was built.
pub fn active_plugin_hook_specs(registry: &PluginRegistry) -> Vec<HookSpec> {
    registry
        .active_plugins()
        .into_iter()
        .flat_map(|plugin| plugin.hook_specs.iter().cloned())
        .collect()
}

/// Where a new session's plugin hooks come from.
pub enum PluginHookSource<'a> {
    /// The plugin registry a top-level session was spawned with.
    Registry(Option<&'a PluginRegistry>),
    /// The parent registry a subagent inherits.
    /// A subagent's own plugin registry is the process-wide snapshot. That snapshot can hold another directory's plugins.
    Parent,
}

/// Returns `base` with its plugin hooks taken from `source`, or `None` when no hook remains.
pub fn with_plugin_hooks(
    base: Option<Arc<HookRegistry>>,
    source: PluginHookSource<'_>,
) -> Option<Arc<HookRegistry>> {
    match source {
        PluginHookSource::Registry(plugins) => replace_plugin_hooks(
            base,
            plugins.map(active_plugin_hook_specs).unwrap_or_default(),
        ),
        PluginHookSource::Parent => base,
    }
}

/// Returns `base` with its plugin-layer hooks replaced by `specs`, or `None` when no hook remains.
/// Returns `base` unchanged when neither side has a plugin hook.
pub fn replace_plugin_hooks(
    base: Option<Arc<HookRegistry>>,
    specs: Vec<HookSpec>,
) -> Option<Arc<HookRegistry>> {
    if specs.is_empty()
        && !base
            .as_deref()
            .is_some_and(|registry| registry.has_layer(HookProvenance::Plugin))
    {
        return base;
    }
    let mut registry = base.map(Arc::unwrap_or_clone).unwrap_or_default();
    registry.remove_layer(HookProvenance::Plugin);
    registry.append_specs(specs);
    (!registry.is_empty()).then(|| Arc::new(registry))
}

/// Shared processing pipeline for plugin hooks (file-based or inline).
///
/// Pre-filters unsupported events, parses via `parse_hook_file()`,
/// injects plugin env vars, and namespaces hook names.
fn process_hooks_content(
    content: &str,
    source_path: &Path,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<HookSpec>, Vec<String>) {
    let (filtered_content, skipped_events) = prefilter_unsupported_events(content);
    let mut warnings: Vec<String> = Vec::new();

    for event in &skipped_events {
        tracing::info!(
            plugin = plugin_name,
            event = event,
            "skipping unsupported hook event from plugin"
        );
        warnings.push(format!(
            "plugin {plugin_name}: skipped unsupported event '{event}'"
        ));
    }

    let (mut specs, parse_errors) = parse_hook_file(&filtered_content, source_path);

    for err in &parse_errors {
        let msg = format!("plugin {plugin_name}: {err}");
        tracing::warn!("{msg}");
        warnings.push(msg);
    }

    // Native `GROK_PLUGIN_*` vars plus their vendor-compat aliases.
    let plugin_env: HashMap<String, String> = HashMap::from([
        ("GROK_PLUGIN_ROOT".to_string(), plugin_root.to_string()),
        ("CLAUDE_PLUGIN_ROOT".to_string(), plugin_root.to_string()),
        ("GROK_PLUGIN_DATA".to_string(), plugin_data.to_string()),
        ("CLAUDE_PLUGIN_DATA".to_string(), plugin_data.to_string()),
    ]);

    for spec in &mut specs {
        // Plugin-owned keys always win over user-declared `env`, or a plugin
        // author could repoint the plugin root and break the contract.
        for (k, v) in &plugin_env {
            spec.extra_env.insert(k.clone(), v.clone());
        }
        spec.layer = xai_grok_hooks::config::HookProvenance::Plugin;
        spec.name = format!(
            "{}{}/{}",
            xai_grok_hooks::config::PLUGIN_HOOK_PREFIX,
            plugin_name,
            spec.name
        );
        // Resolve plugin path placeholders at load time (mirrors managed_mcp)
        // so the command works regardless of the runner's spawn branch.
        if let Some(cmd) = &spec.command {
            let cmd_str = cmd.to_string_lossy();
            let substituted = substitute_env_vars(&cmd_str, plugin_root, plugin_data);
            let expanded = xai_grok_config::expand_env_vars_in_string(&substituted);
            if expanded != cmd_str {
                spec.command = Some(PathBuf::from(expanded));
            }
        }
    }

    (specs, warnings)
}

/// Drop `hooks` event keys the parser wouldn't accept, returning the filtered
/// JSON and the removed names. Not needed for correctness (the parser is lenient)
/// but surfaces the drops to the plugin author as warnings. A key is supported
/// exactly when [`HookEventName::parse_key`] accepts it, so there is no allowlist
/// to drift.
fn prefilter_unsupported_events(json_content: &str) -> (String, Vec<String>) {
    let mut value: serde_json::Value = match serde_json::from_str(json_content) {
        Ok(v) => v,
        // Invalid JSON: let parse_hook_file report it.
        Err(_) => return (json_content.to_string(), vec![]),
    };

    let mut skipped = Vec::new();

    if let Some(hooks_obj) = value.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        let keys_to_remove: Vec<String> = hooks_obj
            .keys()
            .filter(|key| HookEventName::parse_key(key).is_none())
            .cloned()
            .collect();

        for key in keys_to_remove {
            hooks_obj.remove(&key);
            skipped.push(key);
        }
    }

    (
        serde_json::to_string(&value).unwrap_or_else(|_| json_content.to_string()),
        skipped,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefilter_removes_unsupported_events() {
        let json = r#"{
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "echo start"}]}],
                "CustomEvent": [{"hooks": [{"type": "command", "command": "echo custom"}]}],
                "UnknownHook": [{"hooks": [{"type": "command", "command": "echo unknown"}]}],
                "PostToolUse": [{"hooks": [{"type": "command", "command": "echo post"}]}]
            }
        }"#;

        let (filtered, skipped) = prefilter_unsupported_events(json);

        assert_eq!(skipped.len(), 2);
        assert!(skipped.contains(&"CustomEvent".to_string()));
        assert!(skipped.contains(&"UnknownHook".to_string()));

        let parsed: serde_json::Value = serde_json::from_str(&filtered).unwrap();
        let hooks = parsed["hooks"].as_object().unwrap();
        assert!(hooks.contains_key("SessionStart"));
        assert!(hooks.contains_key("PostToolUse"));
        assert!(!hooks.contains_key("CustomEvent"));
        assert!(!hooks.contains_key("UnknownHook"));
    }

    #[test]
    fn prefilter_preserves_all_supported_events() {
        let json = r#"{
            "hooks": {
                "SessionStart": [],
                "PreToolUse": [],
                "PostToolUse": [],
                "SessionEnd": []
            }
        }"#;

        let (_, skipped) = prefilter_unsupported_events(json);
        assert!(skipped.is_empty());
    }

    #[test]
    fn prefilter_handles_snake_case_events() {
        let json = r#"{
            "hooks": {
                "session_start": [],
                "pre_tool_use": [],
                "unknown_event": []
            }
        }"#;

        let (_, skipped) = prefilter_unsupported_events(json);
        assert_eq!(skipped.len(), 1);
        assert!(skipped.contains(&"unknown_event".to_string()));
    }

    #[test]
    fn prefilter_handles_invalid_json() {
        let json = "not valid json{";
        let (filtered, skipped) = prefilter_unsupported_events(json);
        assert_eq!(filtered, json); // returned as-is
        assert!(skipped.is_empty());
    }

    #[test]
    fn prefilter_handles_no_hooks_key() {
        let json = r#"{"settings": {}}"#;
        let (_, skipped) = prefilter_unsupported_events(json);
        assert!(skipped.is_empty());
    }

    #[test]
    fn parse_plugin_hooks_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let hooks_file = hooks_dir.join("hooks.json");
        std::fs::write(
            &hooks_file,
            r#"{
                "hooks": {
                    "SessionStart": [
                        {
                            "hooks": [
                                {"type": "command", "command": "echo plugin-hook"}
                            ]
                        }
                    ],
                    "FutureEvent": [
                        {
                            "hooks": [
                                {"type": "command", "command": "echo unsupported"}
                            ]
                        }
                    ]
                }
            }"#,
        )
        .unwrap();

        let (specs, warnings) =
            parse_plugin_hooks(&hooks_file, "my-plugin", "/path/to/plugin", "/path/to/data");

        // Should have 1 spec from SessionStart, FutureEvent was filtered
        assert_eq!(specs.len(), 1);
        assert!(specs[0].name.starts_with("plugin/my-plugin/"));
        assert_eq!(
            specs[0].extra_env.get("GROK_PLUGIN_ROOT").unwrap(),
            "/path/to/plugin"
        );
        assert_eq!(
            specs[0].extra_env.get("CLAUDE_PLUGIN_ROOT").unwrap(),
            "/path/to/plugin"
        );
        assert_eq!(
            specs[0].extra_env.get("GROK_PLUGIN_DATA").unwrap(),
            "/path/to/data"
        );

        // Should have a warning about FutureEvent
        assert!(warnings.iter().any(|w| w.contains("FutureEvent")));
    }

    #[test]
    fn parse_inline_hooks_from_value() {
        let value = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            {"type": "command", "command": "echo inline-hook"}
                        ]
                    }
                ]
            }
        });

        let (specs, warnings) = parse_plugin_hooks_from_value(
            &value,
            "inline-plugin",
            "/path/to/plugin",
            "/path/to/data",
        );

        assert_eq!(specs.len(), 1);
        assert!(specs[0].name.starts_with("plugin/inline-plugin/"));
        assert_eq!(
            specs[0].extra_env.get("GROK_PLUGIN_ROOT").unwrap(),
            "/path/to/plugin"
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn parse_inline_hooks_filters_unsupported_events() {
        let value = serde_json::json!({
            "hooks": {
                "PostToolUse": [
                    {"hooks": [{"type": "command", "command": "echo post"}]}
                ],
                "FutureEvent": [
                    {"hooks": [{"type": "command", "command": "echo future"}]}
                ]
            }
        });

        let (specs, warnings) =
            parse_plugin_hooks_from_value(&value, "filter-test", "/root", "/data");

        // PostToolUse is supported, FutureEvent is not
        assert_eq!(specs.len(), 1);
        assert!(warnings.iter().any(|w| w.contains("FutureEvent")));
    }

    /// Regression: plugin path placeholders must resolve at load time, else the
    /// runner's pre-spawn env check refuses to run the hook.
    #[test]
    fn parse_plugin_hooks_substitutes_plugin_root_in_command() {
        let value = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"hooks": [
                        {"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/hooks/pre.sh"},
                        {"type": "command", "command": "${GROK_PLUGIN_ROOT}/hooks/alias.sh"},
                        {"type": "command", "command": "${CLAUDE_PLUGIN_DATA}/cache/post.sh"}
                    ]}
                ]
            }
        });

        let (specs, warnings) = parse_plugin_hooks_from_value(
            &value,
            "gb1183-plugin",
            "/opt/plugins/gb1183",
            "/var/plugins/gb1183",
        );

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(specs.len(), 3);

        let commands: Vec<String> = specs
            .iter()
            .map(|s| s.command.as_ref().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(commands.contains(&"/opt/plugins/gb1183/hooks/pre.sh".to_string()));
        assert!(commands.contains(&"/opt/plugins/gb1183/hooks/alias.sh".to_string()));
        assert!(commands.contains(&"/var/plugins/gb1183/cache/post.sh".to_string()));

        // None of the resolved commands should still contain the literal
        // `${...}` placeholder.
        for cmd in &commands {
            assert!(
                !cmd.contains("${"),
                "command still contains placeholder: {cmd}"
            );
        }

        // `command_raw` must stay unmodified: it's the display form and rewriting
        // it would leak `extra_env`-resolved secrets.
        let raws: Vec<&str> = specs
            .iter()
            .map(|s| s.command_raw.as_deref().unwrap_or(""))
            .collect();
        assert!(
            raws.contains(&"${CLAUDE_PLUGIN_ROOT}/hooks/pre.sh"),
            "command_raw must preserve the source string verbatim, got {raws:?}"
        );
        assert!(
            raws.contains(&"${GROK_PLUGIN_ROOT}/hooks/alias.sh"),
            "command_raw must preserve the source string verbatim, got {raws:?}"
        );
        assert!(
            raws.contains(&"${CLAUDE_PLUGIN_DATA}/cache/post.sh"),
            "command_raw must preserve the source string verbatim, got {raws:?}"
        );
    }

    #[test]
    fn parse_inline_hooks_handles_empty_value() {
        let value = serde_json::json!({});
        let (specs, warnings) = parse_plugin_hooks_from_value(&value, "empty", "/root", "/data");
        assert!(specs.is_empty());
        assert!(warnings.is_empty());
    }

    /// Regression: generic env vars (`${HOME}`) resolve at load time, and plugin
    /// placeholders resolve exactly once (no leftover `$`, no double-expansion).
    #[test]
    fn parse_plugin_hooks_resolves_plugin_root_exactly_once() {
        let value = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"hooks": [
                        {"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/x.sh"}
                    ]}
                ]
            }
        });

        let (specs, warnings) = parse_plugin_hooks_from_value(
            &value,
            "no-double-expand",
            "/the/plugin/root",
            "/the/plugin/data",
        );

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(specs.len(), 1);
        let cmd = specs[0]
            .command
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(cmd, "/the/plugin/root/x.sh");
        assert!(
            !cmd.contains('$'),
            "command must not contain leftover $: {cmd}"
        );
    }

    /// User-declared `env` is kept, but the four plugin-owned keys always win.
    #[test]
    fn parse_plugin_hooks_user_env_merged_with_plugin_precedence() {
        // All four keys, so a one-key regression can't pass.
        let value = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"hooks": [
                        {
                            "type": "command",
                            "command": "echo hi",
                            "env": {
                                "FOO": "bar",
                                "CLAUDE_PLUGIN_ROOT": "/user/wins?",
                                "GROK_PLUGIN_ROOT": "/user/wins?",
                                "CLAUDE_PLUGIN_DATA": "/user/wins?",
                                "GROK_PLUGIN_DATA": "/user/wins?"
                            }
                        }
                    ]}
                ]
            }
        });

        let (specs, warnings) = parse_plugin_hooks_from_value(
            &value,
            "user-env-plugin",
            "/actual/plugin/root",
            "/actual/plugin/data",
        );

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(specs.len(), 1);

        // User-declared key the plugin doesn't own: preserved verbatim.
        assert_eq!(
            specs[0].extra_env.get("FOO").map(String::as_str),
            Some("bar"),
            "user-declared env keys must survive plugin merge"
        );

        // All four plugin-owned keys: plugin wins over the user's attempt.
        for (key, expected) in [
            ("CLAUDE_PLUGIN_ROOT", "/actual/plugin/root"),
            ("GROK_PLUGIN_ROOT", "/actual/plugin/root"),
            ("CLAUDE_PLUGIN_DATA", "/actual/plugin/data"),
            ("GROK_PLUGIN_DATA", "/actual/plugin/data"),
        ] {
            assert_eq!(
                specs[0].extra_env.get(key).map(String::as_str),
                Some(expected),
                "plugin-injected key {key} must override user-declared value"
            );
        }
    }

    #[test]
    fn parse_plugin_hooks_expands_generic_env_vars_in_command() {
        // Uniquely-named var so concurrent tests don't collide.
        let var = "GB1183_HOOKS_ADAPTER_TEST_HOME";
        // SAFETY: env writes are not thread-safe; this test is single-threaded.
        unsafe {
            std::env::set_var(var, "/expanded/home");
        }

        let cmd_braces = format!("${{{var}}}/helper.sh");
        let cmd_bare = format!("${var}/raw.sh");
        let value = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"hooks": [
                        {"type": "command", "command": cmd_braces},
                        {"type": "command", "command": cmd_bare},
                    ]}
                ]
            }
        });

        let (specs, warnings) =
            parse_plugin_hooks_from_value(&value, "env-expand", "/root", "/data");

        // SAFETY: env writes are not thread-safe; this test is single-threaded.
        unsafe {
            std::env::remove_var(var);
        }

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(specs.len(), 2);

        let commands: Vec<String> = specs
            .iter()
            .map(|s| s.command.as_ref().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            commands.contains(&"/expanded/home/helper.sh".to_string()),
            "missing brace-form expansion: {commands:?}"
        );
        assert!(
            commands.contains(&"/expanded/home/raw.sh".to_string()),
            "missing bare-form expansion: {commands:?}"
        );
        for cmd in &commands {
            assert!(!cmd.contains('$'), "command still contains $: {cmd}");
        }
    }
    const USER_HOOK: &str = "hooks:session_start[0].hooks[0]";
    const PARENT_PLUGIN_HOOK: &str = "plugin/parent-plugin/plugin:session_start[0].hooks[0]";
    const OTHER_PLUGIN_HOOK: &str = "plugin/other-plugin/plugin:session_start[0].hooks[0]";

    fn session_start_hooks() -> serde_json::Value {
        serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "true" }] }]
            }
        })
    }

    fn user_registry() -> Arc<HookRegistry> {
        let (specs, errors) = parse_hook_file(
            &session_start_hooks().to_string(),
            Path::new("/user/hooks.json"),
        );
        assert_eq!(0, errors.len());
        let mut registry = HookRegistry::default();
        registry.append_specs(specs);
        Arc::new(registry)
    }

    fn plugin_registry(name: &str) -> PluginRegistry {
        use crate::plugins::PluginScope;
        use crate::plugins::manifest::PathOrInline;

        let mut plugin =
            crate::plugins::registry::tests::make_discovered(name, PluginScope::User, true);
        plugin.manifest.hooks = Some(PathOrInline::Inline(session_start_hooks()));
        PluginRegistry::from_discovered(vec![plugin], &[], &[name.to_owned()])
    }

    fn hook_names(registry: Option<&HookRegistry>) -> Vec<String> {
        let mut names: Vec<String> = registry
            .map(|r| r.all_hooks().iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    fn own_registry(plugins: &PluginRegistry) -> PluginHookSource<'_> {
        PluginHookSource::Registry(Some(plugins))
    }

    #[test]
    fn plugin_hooks_join_discovered_hooks() {
        let plugins = plugin_registry("parent-plugin");

        let merged = with_plugin_hooks(Some(user_registry()), own_registry(&plugins));

        assert_eq!(
            vec![USER_HOOK.to_owned(), PARENT_PLUGIN_HOOK.to_owned()],
            hook_names(merged.as_deref())
        );
    }

    #[test]
    fn plugin_hooks_load_without_any_other_hook() {
        let plugins = plugin_registry("parent-plugin");

        let merged = with_plugin_hooks(None, own_registry(&plugins));

        assert_eq!(
            vec![PARENT_PLUGIN_HOOK.to_owned()],
            hook_names(merged.as_deref())
        );
    }

    #[test]
    fn disabled_plugin_contributes_no_hooks() {
        use crate::plugins::PluginScope;
        use crate::plugins::manifest::PathOrInline;

        let mut plugin = crate::plugins::registry::tests::make_discovered(
            "parent-plugin",
            PluginScope::User,
            true,
        );
        plugin.manifest.hooks = Some(PathOrInline::Inline(session_start_hooks()));
        let plugins =
            PluginRegistry::from_discovered(vec![plugin], &["parent-plugin".to_owned()], &[]);

        assert!(with_plugin_hooks(None, own_registry(&plugins)).is_none());
    }

    #[test]
    fn subagent_keeps_parent_plugin_hooks_over_its_own_registry() {
        let parent_plugins = plugin_registry("parent-plugin");
        let snapshot_plugins = plugin_registry("other-plugin");
        let parent = with_plugin_hooks(Some(user_registry()), own_registry(&parent_plugins));

        let child = with_plugin_hooks(parent.clone(), PluginHookSource::Parent);
        let from_snapshot = with_plugin_hooks(parent, own_registry(&snapshot_plugins));

        assert_eq!(
            vec![USER_HOOK.to_owned(), PARENT_PLUGIN_HOOK.to_owned()],
            hook_names(child.as_deref())
        );
        assert_eq!(
            vec![USER_HOOK.to_owned(), OTHER_PLUGIN_HOOK.to_owned()],
            hook_names(from_snapshot.as_deref())
        );
    }

    #[test]
    fn reapplied_plugin_registry_replaces_its_hooks() {
        let plugins = plugin_registry("parent-plugin");
        let first = with_plugin_hooks(Some(user_registry()), own_registry(&plugins));

        let second = with_plugin_hooks(first, own_registry(&plugins));

        assert_eq!(
            vec![USER_HOOK.to_owned(), PARENT_PLUGIN_HOOK.to_owned()],
            hook_names(second.as_deref())
        );
    }

    #[test]
    fn plugin_hooks_leave_with_the_plugin_registry() {
        let plugins = plugin_registry("parent-plugin");
        let with_plugins = with_plugin_hooks(Some(user_registry()), own_registry(&plugins));

        let without = with_plugin_hooks(with_plugins, PluginHookSource::Registry(None));

        assert_eq!(vec![USER_HOOK.to_owned()], hook_names(without.as_deref()));
    }

    #[test]
    fn registry_without_plugin_hooks_is_passed_through() {
        let base = user_registry();

        let merged = with_plugin_hooks(
            Some(Arc::clone(&base)),
            own_registry(&PluginRegistry::empty()),
        );

        assert!(merged.is_some_and(|merged| Arc::ptr_eq(&base, &merged)));
    }
    #[test]
    fn untrusted_plugin_contributes_no_hooks() {
        use crate::plugins::manifest::PathOrInline;
        let mut plugin = crate::plugins::registry::tests::make_discovered(
            "project-plugin",
            crate::plugins::PluginScope::Project,
            false,
        );
        plugin.manifest.hooks = Some(PathOrInline::Inline(session_start_hooks()));
        let plugins =
            PluginRegistry::from_discovered(vec![plugin], &[], &["project-plugin".to_owned()]);
        assert!(plugins.get("project-plugin").unwrap().hook_specs.is_empty());
        assert!(with_plugin_hooks(None, own_registry(&plugins)).is_none());
    }

    #[test]
    fn replacement_uses_provenance_instead_of_hook_name() {
        let mut user = Arc::unwrap_or_clone(user_registry()).into_specs();
        user[0].name = "plugin/user-defined-hook".to_owned();
        let mut base = HookRegistry::default();
        base.append_specs(user);
        let plugins = plugin_registry("parent-plugin");
        let added = with_plugin_hooks(Some(Arc::new(base)), own_registry(&plugins));
        let removed = with_plugin_hooks(added, PluginHookSource::Registry(None));
        assert_eq!(
            hook_names(removed.as_deref()),
            vec!["plugin/user-defined-hook".to_owned()]
        );
    }

    #[tokio::test]
    async fn fresh_session_start_executes_cached_plugin_hook() {
        use xai_grok_hooks::event::{HookEventEnvelope, HookPayload};
        use xai_grok_hooks::runner::RunContext;
        let dir = tempfile::tempdir().unwrap();
        let hook_file = dir.path().join("hooks.json");
        let mut hooks = session_start_hooks();
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"] =
            serde_json::json!("printf fresh > \"${GROK_PLUGIN_ROOT}/start-marker\"");
        std::fs::write(&hook_file, hooks.to_string()).unwrap();
        let mut plugin = crate::plugins::registry::tests::make_discovered(
            "fresh-plugin",
            crate::plugins::PluginScope::User,
            true,
        );
        plugin.root = dir.path().to_owned();
        plugin.canonical_root = plugin.root.clone();
        plugin.hooks_path = Some(hook_file.clone());
        let plugins =
            PluginRegistry::from_discovered(vec![plugin], &[], &["fresh-plugin".to_owned()]);
        // Cached specs must remain usable independently of later disk changes.
        std::fs::remove_file(hook_file).unwrap();
        let registry = with_plugin_hooks(None, own_registry(&plugins)).unwrap();
        let cwd = dir.path().to_str().unwrap();
        let event = HookEventEnvelope {
            hook_event_name: HookEventName::SessionStart,
            session_id: "fresh-plugin-session".into(),
            cwd: cwd.into(),
            workspace_root: cwd.into(),
            timestamp: "2026-10-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::SessionStart {
                source: "new".into(),
                model_id: None,
                agent_type: None,
            },
        };
        let context = RunContext {
            session_id: "fresh-plugin-session",
            workspace_root: cwd,
            process_scope: None,
        };
        let results = xai_grok_hooks::dispatcher::dispatch_non_blocking(
            &registry,
            HookEventName::SessionStart,
            &event,
            &context,
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            xai_grok_hooks::result::HookRunResult::Success { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("start-marker")).unwrap(),
            "fresh"
        );
    }
}
