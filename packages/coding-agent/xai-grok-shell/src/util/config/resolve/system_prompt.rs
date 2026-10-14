pub const ENV_SYSTEM_PROMPT_LABEL: &str = "BLADERUNNER_SYSTEM_PROMPT_LABEL";

/// Resolve system-prompt identity label.
/// Precedence: env → config per-model → `[agent]` → provider per-model →
/// provider global → active model slug. Empty/whitespace falls through.
///
/// Per-model TOML is looked up by session catalog id, then routing slug
/// (`ModelInfo.model`). Do not use CLI `-m` alone — it may outlive a mid-session
/// model switch.
pub(crate) fn resolve_system_prompt_label(
    cfg: &crate::agent::config::Config,
    model_id: &str,
    model: Option<&crate::agent::config::ModelInfo>,
) -> String {
    let label_for = |key: &str| {
        cfg.config_models
            .get(key)
            .and_then(|m| m.system_prompt_label.clone())
    };
    let user_per_model =
        label_for(model_id).or_else(|| model.map(|m| m.model.as_str()).and_then(label_for));

    let active_model_slug = model
        .map(|model| model.model.as_str())
        .filter(|slug| !slug.trim().is_empty())
        .unwrap_or(model_id);

    resolve_system_prompt_label_from_tiers(
        user_per_model,
        cfg.agent.system_prompt_label.clone(),
        model.and_then(|m| m.system_prompt_label.clone()),
        cfg.remote_settings
            .as_ref()
            .and_then(|r| r.system_prompt_label.clone()),
        active_model_slug,
    )
}

pub(crate) fn resolve_system_prompt_label_from_tiers(
    user_per_model: Option<String>,
    user_global: Option<String>,
    gb_per_model: Option<String>,
    gb_global: Option<String>,
    active_model_slug: &str,
) -> String {
    let non_empty = |s: Option<String>| {
        s.and_then(|v| {
            let t = v.trim();
            (!t.is_empty()).then(|| t.to_string())
        })
    };
    xai_grok_config::env_string(ENV_SYSTEM_PROMPT_LABEL)
        .and_then(|s| non_empty(Some(s)))
        .or_else(|| non_empty(user_per_model))
        .or_else(|| non_empty(user_global))
        .or_else(|| non_empty(gb_per_model))
        .or_else(|| non_empty(gb_global))
        .unwrap_or_else(|| active_model_slug.trim().to_string())
}

#[cfg(test)]
mod system_prompt_label_tests {
    use super::{ENV_SYSTEM_PROMPT_LABEL, resolve_system_prompt_label_from_tiers};

    /// Serialize access to both environment spellings and clear them for tier tests.
    /// `env_wins_over_all_tiers` mutates the env; without this lock, parallel tests
    /// that expect the var unset (e.g. `gb_per_model_beats_gb_global`) flake.
    fn with_env_cleared<R>(f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap();
        let legacy = "GROK_SYSTEM_PROMPT_LABEL";
        let prev = std::env::var(ENV_SYSTEM_PROMPT_LABEL).ok();
        let legacy_prev = std::env::var(legacy).ok();
        // Safety: test-only, locked.
        unsafe { std::env::remove_var(ENV_SYSTEM_PROMPT_LABEL) };
        unsafe { std::env::remove_var(legacy) };
        let r = f();
        match prev {
            Some(v) => unsafe { std::env::set_var(ENV_SYSTEM_PROMPT_LABEL, v) },
            None => unsafe { std::env::remove_var(ENV_SYSTEM_PROMPT_LABEL) },
        }
        match legacy_prev {
            Some(v) => unsafe { std::env::set_var(legacy, v) },
            None => unsafe { std::env::remove_var(legacy) },
        }
        r
    }

    #[test]
    fn active_model_slug_is_default_when_all_overrides_are_unset() {
        with_env_cleared(|| {
            assert_eq!(
                resolve_system_prompt_label_from_tiers(None, None, None, None, "gpt-5.4"),
                "gpt-5.4"
            );
        });
    }

    #[test]
    fn per_model_beats_global_and_gb() {
        with_env_cleared(|| {
            assert_eq!(
                resolve_system_prompt_label_from_tiers(
                    Some("PerModel".into()),
                    Some("Global".into()),
                    Some("GbPer".into()),
                    Some("GbGlobal".into()),
                    "model-slug",
                ),
                "PerModel"
            );
        });
    }

    #[test]
    fn global_beats_gb() {
        with_env_cleared(|| {
            assert_eq!(
                resolve_system_prompt_label_from_tiers(
                    None,
                    Some("Global".into()),
                    Some("GbPer".into()),
                    Some("GbGlobal".into()),
                    "model-slug",
                ),
                "Global"
            );
        });
    }

    #[test]
    fn gb_per_model_beats_gb_global() {
        with_env_cleared(|| {
            assert_eq!(
                resolve_system_prompt_label_from_tiers(
                    None,
                    None,
                    Some("GbPer".into()),
                    Some("GbGlobal".into()),
                    "model-slug",
                ),
                "GbPer"
            );
        });
    }

    #[test]
    fn empty_and_whitespace_fall_through() {
        with_env_cleared(|| {
            assert_eq!(
                resolve_system_prompt_label_from_tiers(
                    Some("  ".into()),
                    Some("".into()),
                    None,
                    Some("GbGlobal".into()),
                    "model-slug",
                ),
                "GbGlobal"
            );
        });
    }

    #[test]
    fn env_wins_over_all_tiers() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Safety: test-only, locked.
        unsafe { std::env::set_var(ENV_SYSTEM_PROMPT_LABEL, "FromEnv") };
        let got = resolve_system_prompt_label_from_tiers(
            Some("PerModel".into()),
            Some("Global".into()),
            Some("GbPer".into()),
            Some("GbGlobal".into()),
            "model-slug",
        );
        unsafe { std::env::remove_var(ENV_SYSTEM_PROMPT_LABEL) };
        assert_eq!(got, "FromEnv");
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
