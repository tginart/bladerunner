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
