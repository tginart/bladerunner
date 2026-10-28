pub(crate) mod host_service;
pub(crate) mod manager;
pub(crate) mod notify;
pub(crate) mod registry;
pub(crate) mod schema_contract;
pub(crate) mod store;
pub(crate) mod tracker;

#[cfg(test)]
mod builtin_tests {
    #[test]
    fn every_builtin_validates_and_matches_its_registered_name() {
        for builtin in super::registry::BUILTIN_WORKFLOWS {
            let meta = xai_workflow::extract_meta(builtin.script)
                .unwrap_or_else(|e| panic!("builtin '{}' must validate: {e}", builtin.name));
            assert_eq!(
                meta.name, builtin.name,
                "registry key must equal meta.name for '{}'",
                builtin.name
            );
        }
    }

    fn n_shot_script() -> &'static str {
        super::registry::BUILTIN_WORKFLOWS
            .iter()
            .find(|builtin| builtin.name == "n-shot")
            .map(|builtin| builtin.script)
            .expect("n-shot builtin registered")
    }

    #[test]
    fn n_shot_completes_for_identical_and_per_shot_args() {
        let max_shots = crate::session::slash_commands::N_SHOT_MAX_SHOTS;
        assert!(n_shot_script().contains(&format!("let max_shots = {max_shots};")));
        let cases = [
            serde_json::json!({ "n": 3, "prompt": "fix the bug" }),
            serde_json::json!({ "shots": [
                { "prompt": "fix it simply", "model": "fast-model" },
                { "prompt": "fix it robustly", "label": "robust" },
            ] }),
        ];
        for args in cases {
            let report = xai_workflow::validate_script(n_shot_script(), Some(args.clone()))
                .unwrap_or_else(|e| panic!("{args}: {e}"));
            assert!(
                report.outcome_summary.starts_with("completed:"),
                "{args}: {}",
                report.outcome_summary
            );
        }
    }

    /// Purpose: Verify that the report gives each shot's base for inspection and adoption.
    /// Input: Two simulated shots, one with a base commit and one without it.
    /// Output: A report that preserves the base and marks the missing base as unknown.
    #[test]
    fn n_shot_report_includes_each_worktree_base() {
        use xai_workflow::host::{AgentResult, WorkflowHostRequest};

        let (host_tx, mut host_rx) = tokio::sync::mpsc::unbounded_channel();
        let (report_tx, report_rx) = std::sync::mpsc::channel();
        let host = std::thread::spawn(move || {
            while let Some(request) = host_rx.blocking_recv() {
                match request {
                    WorkflowHostRequest::ReserveAgentCalls { reply, .. }
                    | WorkflowHostRequest::ReleaseAgentCalls { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    WorkflowHostRequest::SpawnAgent { opts, reply } => {
                        let has_base = opts.label.as_deref() == Some("shot-1");
                        assert!(opts.fork_context);
                        assert!(opts.isolation_worktree);
                        assert!(opts.retain_worktree);
                        assert_eq!(opts.model.as_deref(), has_base.then_some("shot-model"));
                        let _ = reply.send(Ok(AgentResult {
                            agent_id: opts.label.unwrap(),
                            success: true,
                            output: serde_json::json!("done"),
                            cancelled: false,
                            tokens_used: 0,
                            duration_ms: 0,
                            worktree_path: Some("/worktree".into()),
                            worktree_base_commit: has_base.then(|| "abc123".into()),
                            worktree_changes: Some("1 file changed".into()),
                        }));
                    }
                    WorkflowHostRequest::WriteScratchFile { content, reply, .. } => {
                        report_tx.send(content).unwrap();
                        let _ = reply.send(Ok("scratch/report.md".into()));
                    }
                    WorkflowHostRequest::Phase { .. }
                    | WorkflowHostRequest::Log { .. }
                    | WorkflowHostRequest::Telemetry { .. } => {}
                    request => panic!("unexpected host request: {request:?}"),
                }
            }
        });
        let outcome = xai_workflow::run_workflow(xai_workflow::WorkflowRunParams {
            script: n_shot_script().into(),
            args: serde_json::json!({
                "prompt": "fix it",
                "shots": [{"model": "shot-model"}, {}],
            }),
            journal: xai_workflow::Journal::new(None),
            host_tx,
            cancel: tokio_util::sync::CancellationToken::new(),
            max_ops: 10_000_000,
        });
        assert!(matches!(
            outcome,
            xai_workflow::WorkflowOutcome::Completed { .. }
        ));
        let report = report_rx.recv().unwrap();
        host.join().unwrap();
        let first = report.split("## shot-1").nth(1).unwrap();
        let (first, second) = first.split_once("## shot-2").unwrap();
        assert!(first.contains("- Base commit: abc123"), "{report}");
        assert!(second.contains("- Base commit: unknown"), "{report}");
        assert!(report.contains("diff --binary <base-commit>"), "{report}");
        assert!(!report.contains("diff HEAD"), "{report}");
    }

    #[test]
    fn n_shot_pauses_on_bad_args() {
        let cases = [
            serde_json::json!({ "n": 2 }),
            serde_json::json!({ "n": 0, "prompt": "x" }),
            serde_json::json!({ "n": 11, "prompt": "x" }),
            serde_json::json!({ "shots": [{ "model": "m" }] }),
        ];
        for args in cases {
            let report = xai_workflow::validate_script(n_shot_script(), Some(args.clone()))
                .unwrap_or_else(|e| panic!("{args}: {e}"));
            assert!(
                report.outcome_summary.starts_with("paused"),
                "{args}: {}",
                report.outcome_summary
            );
        }
    }

    #[test]
    fn deep_research_binds_shards_and_renders_verified_claims() {
        let script = super::registry::BUILTIN_WORKFLOWS
            .iter()
            .find(|builtin| builtin.name == "deep-research")
            .map(|builtin| builtin.script)
            .expect("deep-research builtin registered");
        assert!(script.contains("expected_ids[shard_idx]"));
        assert!(script.contains("verification_results[assigned_shard]"));
        assert!(script.contains("verified_claim_ids"));
        assert!(script.contains("**Status: Partial**"));
        assert!(!script.contains("label: \"research-reporter\""));
        assert!(script.contains("label: \"report-synthesizer\""));
        assert!(script.contains("<report-body>"));
        assert!(!script.contains("output_schema: synthesis_schema"));
        assert!(script.contains("failed citation validation"));
        assert!(script.contains("let findings_fallback"));
        assert!(script.contains("full_report += \"\\n## Sources\\n\""));
        assert!(script.contains("report: chat_report"));
        assert!(!script.contains("chat_report += \"\\n## Sources\\n\""));
    }
}
