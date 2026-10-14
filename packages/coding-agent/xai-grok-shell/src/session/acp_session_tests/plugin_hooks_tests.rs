use super::support::*;
use super::*;
use xai_grok_agent::plugins::hooks_adapter::{PluginHookSource, with_plugin_hooks};
use xai_grok_agent::plugins::{PluginOrigin, PluginRegistry, PluginScope};
use xai_grok_hooks::event::{HookEventName, HookPayload};

fn plugins(
    root: &std::path::Path,
    name: &str,
    enabled: bool,
    trusted: bool,
) -> Arc<PluginRegistry> {
    use xai_grok_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
    let manifest = serde_json::from_value(serde_json::json!({
        "name": name,
        "hooks": {"hooks": {"SessionStart": [{"hooks": [{
            "type": "command", "command": "printf x >> \"${GROK_PLUGIN_ROOT}/marker\""
        }]}]}}
    }))
    .unwrap();
    let plugin = DiscoveredPlugin {
        manifest,
        id: PluginId::new(PluginScope::User, root, name),
        root: root.to_owned(),
        canonical_root: root.to_owned(),
        scope: PluginScope::User,
        origin: PluginOrigin::UserGrok,
        trusted,
        skill_dirs: vec![],
        command_dirs: vec![],
        agent_dirs: vec![],
        hooks_path: None,
        mcp_config_path: None,
        lsp_config_path: None,
        runtime_wasm: None,
        conflict: None,
    };
    let enabled = if enabled {
        vec![name.to_owned()]
    } else {
        vec![]
    };
    Arc::new(PluginRegistry::from_discovered(vec![plugin], &[], &enabled))
}

async fn actor(root: &std::path::Path, is_subagent: bool) -> Arc<SessionActor> {
    let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.session_info.cwd = root.to_str().unwrap().to_owned();
    actor.hook_resolved_workspace_root = actor.session_info.cwd.clone();
    actor.startup_hints.is_subagent = is_subagent;
    Arc::new(actor)
}

async fn start(actor: &SessionActor) {
    actor
        .dispatch_hook(
            HookEventName::SessionStart,
            HookPayload::SessionStart {
                source: "new".into(),
                model_id: None,
                agent_type: None,
            },
            None,
            None,
        )
        .await;
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn plugin_hooks_session_start_reload_and_remove() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let home = tempfile::tempdir().unwrap();
            let _home = xai_grok_test_support::EnvGuard::set(
                "BLADERUNNER_HOME",
                home.path().to_str().unwrap(),
            );
            let _auth = xai_grok_test_support::EnvGuard::set(
                "GROK_AUTH_PATH",
                home.path().join("auth.json").to_str().unwrap(),
            );
            let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
            let root = tempfile::tempdir().unwrap();
            let registry = plugins(root.path(), "startup-plugin", true, true);
            let session = actor(root.path(), false).await;
            *session.plugin_registry.borrow_mut() = Some(registry.clone());
            *session.hook_registry.borrow_mut() =
                with_plugin_hooks(None, PluginHookSource::Registry(Some(&registry)));
            start(&session).await;
            assert_eq!(
                std::fs::read_to_string(root.path().join("marker")).unwrap(),
                "x"
            );
            session.reload_hooks_impl().await;
            session
                .apply_plugin_registry_snapshot(Some(registry.clone()))
                .await;
            session.apply_plugin_registry_snapshot(Some(registry)).await;
            start(&session).await;
            assert_eq!(
                std::fs::read_to_string(root.path().join("marker")).unwrap(),
                "xx",
                "reload must execute each hook once"
            );
            session.apply_plugin_registry_snapshot(None).await;
            start(&session).await;
            assert_eq!(
                std::fs::read_to_string(root.path().join("marker")).unwrap(),
                "xx",
                "removed plugins must stop running"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn plugin_hooks_session_disabled_and_untrusted_stay_excluded() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let home = tempfile::tempdir().unwrap();
            let _home = xai_grok_test_support::EnvGuard::set(
                "BLADERUNNER_HOME",
                home.path().to_str().unwrap(),
            );
            let _auth = xai_grok_test_support::EnvGuard::set(
                "GROK_AUTH_PATH",
                home.path().join("auth.json").to_str().unwrap(),
            );
            let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
            let root = tempfile::tempdir().unwrap();
            let session = actor(root.path(), false).await;
            for (enabled, trusted) in [(false, true), (true, false)] {
                let registry = plugins(root.path(), "excluded-plugin", enabled, trusted);
                session.apply_plugin_registry_snapshot(Some(registry)).await;
                start(&session).await;
                assert!(!root.path().join("marker").exists());
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn plugin_hooks_child_reload_retains_parent_scope() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let home = tempfile::tempdir().unwrap();
            let _home = xai_grok_test_support::EnvGuard::set(
                "BLADERUNNER_HOME",
                home.path().to_str().unwrap(),
            );
            let _auth = xai_grok_test_support::EnvGuard::set(
                "GROK_AUTH_PATH",
                home.path().join("auth.json").to_str().unwrap(),
            );
            let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
            let parent_root = tempfile::tempdir().unwrap();
            let other_root = tempfile::tempdir().unwrap();
            let parent = plugins(parent_root.path(), "parent-plugin", true, true);
            let unrelated = plugins(other_root.path(), "unrelated-plugin", true, true);
            let inherited = with_plugin_hooks(None, PluginHookSource::Registry(Some(&parent)));
            let child = actor(parent_root.path(), true).await;
            *child.hook_registry.borrow_mut() =
                with_plugin_hooks(inherited, PluginHookSource::Parent);
            *child.plugin_registry.borrow_mut() = Some(unrelated.clone());
            start(&child).await;
            child.reload_hooks_impl().await;
            child.apply_plugin_registry_snapshot(Some(unrelated)).await;
            start(&child).await;
            assert_eq!(
                std::fs::read_to_string(parent_root.path().join("marker")).unwrap(),
                "xx"
            );
            assert!(!other_root.path().join("marker").exists());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn plugin_hooks_new_children_inherit_live_parent_after_reload() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let home = tempfile::tempdir().unwrap();
            let _home = xai_grok_test_support::EnvGuard::set(
                "BLADERUNNER_HOME",
                home.path().to_str().unwrap(),
            );
            let _auth = xai_grok_test_support::EnvGuard::set(
                "GROK_AUTH_PATH",
                home.path().join("auth.json").to_str().unwrap(),
            );
            let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
            let old_root = tempfile::tempdir().unwrap();
            let new_root = tempfile::tempdir().unwrap();
            let parent = actor(old_root.path(), false).await;
            *parent.hook_registry.borrow_mut() = with_plugin_hooks(
                None,
                PluginHookSource::Registry(Some(&plugins(
                    old_root.path(),
                    "old-plugin",
                    true,
                    true,
                ))),
            );
            // This is the exact shared clone stored in SessionHandle at startup.
            let parent_handle_hooks = parent.hook_registry.clone();
            let existing_child = actor(old_root.path(), true).await;
            *existing_child.hook_registry.borrow_mut() =
                with_plugin_hooks(parent_handle_hooks.snapshot(), PluginHookSource::Parent);
            start(&existing_child).await;

            parent
                .apply_plugin_registry_snapshot(Some(plugins(
                    new_root.path(),
                    "new-plugin",
                    true,
                    true,
                )))
                .await;
            parent.reload_hooks_impl().await;
            let new_child = actor(new_root.path(), true).await;
            *new_child.hook_registry.borrow_mut() =
                with_plugin_hooks(parent_handle_hooks.snapshot(), PluginHookSource::Parent);
            start(&new_child).await;
            assert_eq!(
                std::fs::read_to_string(new_root.path().join("marker")).unwrap(),
                "x"
            );
            assert_eq!(
                std::fs::read_to_string(old_root.path().join("marker")).unwrap(),
                "x",
                "new children must not execute removed hooks"
            );
            start(&existing_child).await;
            assert_eq!(
                std::fs::read_to_string(old_root.path().join("marker")).unwrap(),
                "xx",
                "existing children retain their inherited scope"
            );

            parent
                .apply_plugin_registry_snapshot(Some(plugins(
                    new_root.path(),
                    "new-plugin",
                    false,
                    true,
                )))
                .await;
            assert!(parent_handle_hooks.snapshot().is_none());
            let disabled_child = actor(new_root.path(), true).await;
            *disabled_child.hook_registry.borrow_mut() =
                with_plugin_hooks(parent_handle_hooks.snapshot(), PluginHookSource::Parent);
            start(&disabled_child).await;
            assert_eq!(
                std::fs::read_to_string(new_root.path().join("marker")).unwrap(),
                "x"
            );
            parent.apply_plugin_registry_snapshot(None).await;
            assert!(parent_handle_hooks.snapshot().is_none());
        })
        .await;
}
