//! Config file loading for Grok.
//!
//! Merge order (lowest → highest priority):
//! 1. Bundled Bladerunner provider defaults, with no credentials
//! 2. `/etc/grok/managed_config.toml`
//! 3. `$BLADERUNNER_HOME/managed_config.toml`
//! 4. `$BLADERUNNER_HOME/config.toml`
//! 5. `$BLADERUNNER_HOME/requirements.toml` (cloud cache; Ed25519-signed at rest once a
//!    key is embedded — see [`signed_policy`] — below the OS-protected layers)
//! 6. `/etc/grok/requirements.toml`
//! 7. macOS MDM managed preferences (`ai.x.grok`, admin-forced) — macOS only
//!
//! Each layer applies its own [`[[version_overrides]]`](version_overrides)
//! before merge. Requirements layers (#4–#6) may opt into fail-closed startup;
//! see [`validate_requirements`].

pub mod campaigns;
pub mod config_override;
pub mod fs_atomic;
pub mod global_hook_sources;
mod loader;
mod macos_managed;
mod managed_cache;
pub mod managed_text;
mod paths;
pub mod mcp_bearer_token_file;
pub use mcp_bearer_token_file::BearerTokenPath;
pub mod shell;
pub mod signed_policy;
mod validation;
pub mod version_overrides;

// Only the cross-crate campaign surface is re-exported at the root; the rest stays
// reachable via the `pub mod` paths for in-crate use without widening the API.
pub use campaigns::{
    CampaignEntry, CampaignOverrides, filter_active_campaigns, ids_touching_paths,
};
pub use global_hook_sources::{
    GlobalHookSource, GlobalHookSourceError, GlobalHookSourceKind, ResolvedGlobalHookSources,
    ensure_grok_hook_slots, existing_ancestor_chain, is_direct_hook_json_name,
    list_direct_hook_json_files, missing_configured_sources, path_has_symlink_component,
    resolve_global_hook_sources, resolve_trust_boundary_sources, unique_ancestors_rootward,
};

#[cfg(unix)]
pub use global_hook_sources::{
    validate_direct_hook_json_file, validated_hook_json_files_for_sources,
};
pub use loader::{
    CampaignsState, ConfigLayers, HookConfigLayer, HookProvenance, MANAGED_CONFIG_FILENAME,
    ManagedConfigLayer, REQUIREMENTS_FILENAME, USER_CONFIG_FILENAME,
    apply_version_overrides_with_registered, campaigns_application_disabled, campaigns_state_path,
    deep_merge_toml, expand_env_vars_in_string, expand_env_vars_in_toml, hook_config_layers,
    hook_config_layers_at, load_config_file, load_dismissed_ids_from_home,
    load_effective_config_disk_only, load_from_disk, load_managed_config,
    load_system_managed_config, load_toml_file, managed_config_layers, managed_config_layers_at,
    toml_error_detail,
};
pub use macos_managed::MDM_REQUIREMENTS_SOURCE;
pub use managed_cache::{
    MANAGED_CONFIG_CACHE_FILE, ServingIdentity, SyncMarker, bump_rollback_floor,
    bump_rollback_floor_with_now, confirmed_team_switch, confirmed_team_switch_at,
    fail_closed_policy_armed_at, is_managed_config_hard_stale_for, is_managed_config_stale_for,
    managed_config_identity_changed_at, managed_deployment_id, managed_policy_compromised_for,
    mark_managed_config_synced, mark_managed_config_synced_at, normalize_identity,
};
pub use paths::{
    BLADERUNNER_HOME_ENV, claude_managed_settings_path, claude_managed_settings_probe_path,
    create_dir_all_owner_only, decode_cwd_from_dirname, default_grok_home, encode_cwd_dirname,
    ensure_sessions_cwd_dir, ensure_sessions_cwd_dir_in, grok_application, grok_application_in,
    grok_home, sessions_cwd_dir, sessions_cwd_dir_in, set_dir_owner_only, system_config_dir,
    user_grok_home,
};
pub use validation::{
    RequirementsError, RequirementsLayer, RequirementsSource, load_merged_requirements,
    requirements_layers, validate_requirements,
};
pub use version_overrides::{VersionOverrideError, apply_version_overrides};

/// Return the canonical Bladerunner name and optional legacy Grok alias for a
/// harness-owned environment variable. Provider-owned names such as
/// `XAI_API_KEY` and standard names such as `OTEL_*` pass through unchanged.
fn env_names(name: &str) -> (&str, Option<String>) {
    if let Some(suffix) = name.strip_prefix("GROK_") {
        return (name, Some(format!("BLADERUNNER_{suffix}")));
    }
    if let Some(suffix) = name.strip_prefix("BLADERUNNER_") {
        return (name, Some(format!("GROK_{suffix}")));
    }
    (name, None)
}

/// Read a harness environment variable with Bladerunner-first precedence.
///
/// Callers may pass either spelling during migration. If both are set,
/// `BLADERUNNER_*` wins and a value-free warning makes the conflict visible.
pub fn env_string(name: &str) -> Option<String> {
    let (requested, counterpart) = env_names(name);
    let (canonical, legacy) = if requested.starts_with("BLADERUNNER_") {
        (requested.to_string(), counterpart)
    } else if let Some(canonical) = counterpart {
        (canonical, Some(requested.to_string()))
    } else {
        return std::env::var(requested).ok();
    };

    let canonical_value = std::env::var(&canonical).ok();
    let legacy_value = legacy
        .as_ref()
        .and_then(|legacy| std::env::var(legacy).ok());
    if canonical_value.is_some() && legacy_value.is_some() {
        tracing::warn!(
            canonical = %canonical,
            legacy = %legacy.as_deref().unwrap_or_default(),
            "both canonical and legacy environment variables are set; using canonical value"
        );
    }
    canonical_value.or(legacy_value)
}

/// Parse an env var as a boolean. `None` if unset or unrecognized.
pub fn env_bool(name: &str) -> Option<bool> {
    let value = env_string(name)?;
    match value.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod env_alias_tests {
    use super::env_names;

    #[test]
    fn harness_names_map_in_both_directions() {
        assert_eq!(
            env_names("GROK_MEMORY"),
            ("GROK_MEMORY", Some("BLADERUNNER_MEMORY".to_string()))
        );
        assert_eq!(
            env_names("BLADERUNNER_MEMORY"),
            ("BLADERUNNER_MEMORY", Some("GROK_MEMORY".to_string()))
        );
        assert_eq!(env_names("XAI_API_KEY"), ("XAI_API_KEY", None));
    }
}
