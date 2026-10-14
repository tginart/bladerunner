//! The dispatcher, hooks, permission UI, and runtime agree on effective MCP arguments.
use super::support::*;
use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use xai_grok_tools::registry::types::ToolConfig;
use xai_grok_tools::types::output::{TextOutput, ToolOutput};

const REMOTE: &str = "splunk__RunSearch";
#[derive(Debug)]
struct Remote(Arc<Mutex<Vec<Value>>>);
impl xai_grok_tools::types::tool_metadata::ToolMetadata for Remote {
    fn kind(&self) -> xai_grok_tools::types::tool::ToolKind {
        xai_grok_tools::types::tool::ToolKind::Other
    }
    fn tool_namespace(&self) -> xai_grok_tools::types::tool::ToolNamespace {
        xai_grok_tools::types::tool::ToolNamespace::MCP
    }
    fn description_template(&self) -> &str {
        "local repair fixture"
    }
}
impl xai_tool_runtime::Tool for Remote {
    type Args = Value;
    type Output = ToolOutput;
    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(REMOTE).unwrap()
    }
    fn description(
        &self,
        _: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(REMOTE, "local repair fixture")
    }
    async fn run(
        &self,
        _: xai_tool_runtime::ToolCallContext,
        args: Value,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        self.0.lock().unwrap().push(args.clone());
        Ok(ToolOutput::Text(TextOutput::from(args.to_string())))
    }
}
async fn fixture(
    wrapper: bool,
) -> (
    SessionActor,
    Arc<Mutex<Vec<Value>>>,
    Arc<Mutex<Vec<Value>>>,
    Arc<Mutex<Vec<Value>>>,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let (gateway_tx, mut gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut actor, event_rx) =
        create_test_actor_ex(0, 256_000, 85, gateway_tx.clone(), persistence_tx).await;
    *actor.agent.borrow_mut() = test_agent_with_tools(if wrapper {
        vec![ToolConfig::for_tool::<
            xai_grok_tools::implementations::use_tool::UseTool,
        >()]
    } else {
        vec![]
    })
    .await;
    actor.hook_resolved_workspace_root = actor.session_info.cwd.clone();
    let captured = Arc::new(Mutex::new(vec![]));
    let bridge = actor.agent.borrow().tool_bridge().clone();
    bridge
        .register_mcp_tools(
            REMOTE.to_owned(),
            Remote(captured.clone()),
            Some(json!({"type":"object","properties":{"input":{"type":"string"}}})),
        )
        .await
        .unwrap();
    actor
        .mcp_state
        .lock()
        .await
        .record_init_failure("splunk", true, None);
    actor.tool_metadata_snapshot.lock().unwrap().tools.push(
        crate::session::tool_index::ToolMetadata {
            qualified_name: REMOTE.into(),
            server_name: "splunk".into(),
            tool_name: "RunSearch".into(),
            description: String::new(),
            parameters: vec!["input".into()],
            input_schema: json!({"type":"object","properties":{"input":{"type":"string"}}}),
        },
    );
    let cwd =
        xai_grok_paths::AbsPathBuf::new(std::path::PathBuf::from(&actor.session_info.cwd)).unwrap();
    actor.permissions = xai_grok_workspace::permission::spawn_permission_manager(
        actor.session_info.id.clone(),
        xai_acp_lib::AcpAgentGatewaySender::new(gateway_tx),
        cwd,
        xai_grok_workspace::permission::ClientType::Generic,
        None,
        vec![],
        vec![],
        false,
        None,
    )
    .0;
    let prompts = Arc::new(Mutex::new(vec![]));
    let prompt_copy = prompts.clone();
    let callbacks = Arc::new(Mutex::new(vec![]));
    let callback_copy = callbacks.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            match msg {
                xai_acp_lib::AcpClientMessage::RequestPermission(args) => {
                    prompt_copy
                        .lock()
                        .unwrap()
                        .push(args.request.tool_call.fields.raw_input.clone().unwrap());
                    let _ = args
                        .response_tx
                        .send(Ok(acp::RequestPermissionResponse::new(
                            acp::RequestPermissionOutcome::Selected(
                                acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(
                                    "allow-once",
                                )),
                            ),
                        )));
                }
                xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                    callback_copy
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str::<Value>(args.request.params.get()).unwrap());
                    let response: Arc<serde_json::value::RawValue> =
                        serde_json::value::to_raw_value(&json!({"decision":"allow"}))
                            .unwrap()
                            .into();
                    let _ = args.response_tx.send(Ok(acp::ExtResponse::new(response)));
                }
                xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                _ => {}
            }
        }
    });
    actor
        .workspace_ops
        .bind_local_session(
            &actor.session_id_string(),
            actor.tool_context.cwd.to_path_buf(),
            actor.tool_context.hunk_tracker_handle.clone(),
            bridge.toolset(),
            None,
        )
        .unwrap();
    (actor, captured, prompts, callbacks, event_rx)
}
fn call(wrapper: bool, input: Value) -> ToolCallResponse {
    ToolCallResponse {
        id: "repair-call".into(),
        kind: "function".into(),
        function: crate::sampling::types::ToolCallFunction::new(
            if wrapper { "use_tool" } else { REMOTE },
            if wrapper {
                json!({"tool_name":REMOTE,"tool_input":input})
            } else {
                input
            }
            .to_string(),
        ),
    }
}
async fn prepare(
    actor: &SessionActor,
    call: ToolCallResponse,
) -> Result<PreparedToolCall, ToolLoop> {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        actor.prepare_tool_call(call, &mut vec![]),
    )
    .await
    .unwrap()
    .unwrap()
}
fn hooks(actor: &SessionActor, root: &std::path::Path, command: &str) {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join("hook.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{command}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (specs, errors) = xai_grok_hooks::config::parse_hooks_from_value_with_dir(
        &json!({"PreToolUse":[{"hooks":[{"type":"command","command":script.to_str().unwrap()}]}]}),
        "repair-test",
        root,
    );
    assert!(errors.is_empty(), "{errors:?}");
    let mut registry = xai_grok_hooks::discovery::HookRegistry::default();
    registry.append_specs(specs);
    *actor.hook_registry.borrow_mut() = Some(Arc::new(registry));
}
#[tokio::test(flavor = "current_thread")]
async fn direct_and_inline_dispatch_and_permissions_receive_repaired_args() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for wrapper in [false, true] {
                let (actor, captured, prompts, _, mut events) = fixture(wrapper).await;
                let prepared =
                    prepare(&actor, call(wrapper, json!({"input":{"SPL":"index=main"}})))
                        .await
                        .unwrap();
                let expected = json!({"input":r#"{"SPL":"index=main"}"#});
                assert_eq!(
                    serde_json::from_str::<Value>(&prepared.raw_arguments).unwrap(),
                    prepared.parsed_args
                );
                assert!(
                    prepared
                        .coercion_note
                        .as_ref()
                        .unwrap()
                        .contains("coerced and sent")
                );
                let bridge = actor.agent.borrow().tool_bridge().clone();
                bridge
                    .call(
                        &prepared.tool_name,
                        prepared.parsed_args.clone(),
                        &prepared.call_id,
                    )
                    .await
                    .unwrap();
                assert_eq!(*captured.lock().unwrap(), vec![expected.clone()]);
                assert_eq!(prepared.effective_lock_args, expected);
                let mut final_display = None;
                while let Ok(event) = events.try_recv() {
                    if let SessionEvent::Notification(SessionNotification::Acp(notification)) =
                        event
                    {
                        if let acp::SessionUpdate::ToolCallUpdate(update) = notification.update {
                            if let Some(raw) = update.fields.raw_input {
                                final_display = Some(raw);
                            }
                        }
                    }
                }
                let display = final_display.expect("refined display input");
                assert_eq!(display["tool_input"].clone(), expected);

                let permission = prompts.lock().unwrap();
                assert_eq!(permission.len(), 1);
                assert_eq!(permission[0]["tool_input"].clone(), expected);
            }
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn repaired_inputs_rerun_hooks_and_denial_stops_dispatch_and_permissions() {
    tokio::task::LocalSet::new().run_until(async {
        for wrapper in [false,true] {
            let (actor,captured,prompts,_,_) = fixture(wrapper).await;
            let root = tempfile::tempdir().unwrap();
            // The original input is an object; the second pass contains an escaped JSON string.
            hooks(&actor,root.path(),r#"read -r payload; case "$payload" in *'"input":"'*) printf '%s' '{"decision":"deny","reason":"corrected input denied"}';; *) printf '%s' '{"decision":"allow"}';; esac"#);
            assert!(prepare(&actor,call(wrapper,json!({"input":{"SPL":"index=main"}}))).await.is_err());
            assert!(captured.lock().unwrap().is_empty());
            assert!(prompts.lock().unwrap().is_empty());
        }
    }).await;
}
#[tokio::test(flavor = "current_thread")]
async fn second_pass_hook_rewrite_controls_dispatch_permission_and_notice() {
    tokio::task::LocalSet::new().run_until(async {
        for wrapper in [false,true] {
            let (actor,captured,prompts,_,_) = fixture(wrapper).await;
            let root = tempfile::tempdir().unwrap();
            hooks(&actor,root.path(),r#"read -r payload; case "$payload" in *'"input":"'*) printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"input":"hook-approved"}}}';; *) printf '%s' '{"decision":"allow"}';; esac"#);
            let prepared = prepare(&actor,call(wrapper,json!({"input":{"SPL":"index=main"}}))).await.unwrap();
            assert!(prepared.coercion_note.is_none());
            let bridge = actor.agent.borrow().tool_bridge().clone();
            bridge.call(&prepared.tool_name,prepared.parsed_args.clone(),&prepared.call_id).await.unwrap();
            let expected = json!({"input":"hook-approved"});
            assert_eq!(*captured.lock().unwrap(),vec![expected.clone()]);
            let permission = prompts.lock().unwrap();
            assert_eq!(permission.len(),1);
            assert_eq!(permission[0]["tool_input"].clone(),expected);
        }
    }).await;
}
#[tokio::test(flavor = "current_thread")]
async fn duplicate_outer_json_is_not_repaired() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _, _, _, _) = fixture(false).await;
            let mut call = call(false, json!({}));
            call.function.arguments = r#"{"input":{},"input":{"SPL":"index=main"}}"#.into();
            let prepared = prepare(&actor, call.clone()).await.unwrap();
            assert_eq!(prepared.raw_arguments, call.function.arguments);
            assert!(prepared.parsed_args["input"].is_object());
            assert!(prepared.coercion_note.unwrap().contains("could not coerce"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn large_effective_arguments_stay_complete_while_client_hook_envelopes_are_bounded() {
    tokio::task::LocalSet::new().run_until(async {
        let (actor,captured,_,callbacks,_) = fixture(false).await;
        let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
        client_hooks.insert(xai_grok_hooks::event::HookEventName::PreToolUse,vec![crate::extensions::hooks::ClientHookGroup {
            matcher:None,callback_ids:vec!["capture".into()],timeout:None,
        }]);
        *actor.client_hooks.borrow_mut() = client_hooks;
        let input = json!({"input":{"payload":"x".repeat(xai_grok_hooks::event::MAX_PAYLOAD_SIZE * 2)}});
        let prepared = prepare(&actor,call(false,input)).await.unwrap();
        let bridge = actor.agent.borrow().tool_bridge().clone();
        bridge.call(&prepared.tool_name,prepared.parsed_args.clone(),&prepared.call_id).await.unwrap();
        assert!(captured.lock().unwrap()[0]["input"].as_str().unwrap().len() > xai_grok_hooks::event::MAX_PAYLOAD_SIZE * 2);
        let values = callbacks.lock().unwrap();
        assert_eq!(values.len(),2);
        for value in values.iter() {
            assert_eq!(value["toolInputTruncated"],true,"{value}");
            assert!(value["toolInput"].as_str().unwrap().len() < xai_grok_hooks::event::MAX_PAYLOAD_SIZE + 100);
        }
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn successful_execution_attaches_bounded_model_notice() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, captured, _, _, _) = fixture(true).await;
            actor
                .execute_tool_calls(vec![call(true, json!({"input":{"SPL":"index=main"}}))])
                .await
                .unwrap();
            assert_eq!(captured.lock().unwrap().len(), 1);
            let history = actor.chat_state_handle.get_conversation().await;
            let text = history
                .iter()
                .find_map(|item| match item {
                    ConversationItem::ToolResult(result)
                        if result.tool_call_id == "repair-call" =>
                    {
                        Some(result.content.to_string())
                    }
                    _ => None,
                })
                .expect("model tool result");
            assert!(text.contains("coerced and sent"), "{text}");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn invalid_nonobject_hook_output_leaves_valid_wrapper_unchanged() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, captured, prompts, _, _) = fixture(true).await;
            let root = tempfile::tempdir().unwrap();
            hooks(
                &actor,
                root.path(),
                r#"printf '%s' '{"decision":"allow","updatedInput":[]}'"#,
            );
            // Invalid hook output fails open without changing otherwise valid arguments.
            let prepared = prepare(&actor, call(true, json!({"input":"valid"})))
                .await
                .unwrap();
            assert_eq!(prepared.effective_lock_args, json!({"input":"valid"}));
            assert!(captured.lock().unwrap().is_empty());
            assert_eq!(prompts.lock().unwrap().len(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn renamed_wrapper_keeps_final_wire_arguments_and_inner_lock_arguments() {
    tokio::task::LocalSet::new().run_until(async {
        let (actor,captured,_,_,_) = fixture(true).await;
        let mut config = ToolConfig::for_tool::<xai_grok_tools::implementations::use_tool::UseTool>();
        config.name_override = Some("invoke_remote".into());
        config.params_name_overrides = Some(std::collections::HashMap::from([
            ("tool_name".into(),"target".into()),("tool_input".into(),"args".into()),
        ]));
        *actor.agent.borrow_mut() = test_agent_with_tools(vec![config]).await;
        let bridge = actor.agent.borrow().tool_bridge().clone();
        bridge.register_mcp_tools(REMOTE.into(),Remote(captured.clone()),Some(json!({"type":"object","properties":{"input":{"type":"string"}}}))).await.unwrap();
        let mut call = call(true,json!({}));
        call.function.name = "invoke_remote".into();
        call.function.arguments = json!({"target":REMOTE,"args":{"input":{"SPL":"index=main"},"path":"final-file"}}).to_string();
        let prepared = prepare(&actor,call).await.unwrap();
        assert_eq!(prepared.parsed_args,json!({"target":REMOTE,"args":{"input":r#"{"SPL":"index=main"}"#,"path":"final-file"}}));
        assert_eq!(prepared.effective_lock_args["path"],"final-file");
        bridge.call(&prepared.tool_name,prepared.parsed_args.clone(),&prepared.call_id).await.unwrap();
        assert_eq!(captured.lock().unwrap()[0],prepared.effective_lock_args);
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn hook_arguments_that_do_not_parse_are_blocked_before_permission() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, captured, prompts, _, _) = fixture(false).await;
            *actor.agent.borrow_mut() =
                test_agent_with_tools(vec![ToolConfig::from_id("GrokBuild:read_file")]).await;
            let root = tempfile::tempdir().unwrap();
            hooks(
                &actor,
                root.path(),
                r#"printf '%s' '{"decision":"allow","updatedInput":{"target_file":null}}'"#,
            );
            let mut call = call(false, json!({"target_file":"/tmp/fixture"}));
            call.function.name = "read_file".into();
            assert!(prepare(&actor, call).await.is_err());
            assert!(captured.lock().unwrap().is_empty());
            assert!(prompts.lock().unwrap().is_empty());
        })
        .await;
}
