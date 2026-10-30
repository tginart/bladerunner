use std::path::{Path, PathBuf};

// Project-hook trust is no longer stored here: the shell's folder-trust store
// (`~/.grok/trusted_folders.toml`) is the single authority for whether a repo's
// project hooks run (the same gate as repo-local MCP/LSP). The helpers below
// exist only to migrate prior grants out of the legacy file.

/// Path to the legacy project-hook trust file
/// (`<user_grok_home>/trusted-hook-projects`), or `None` when no user grok home
/// resolves. Retained only for the one-time migration into folder-trust.
pub fn legacy_trust_file_path() -> Option<PathBuf> {
    Some(xai_grok_config::user_grok_home()?.join("trusted-hook-projects"))
}

/// Parse the legacy trusted-projects file into a list of project paths.
///
/// The legacy format is one canonical absolute path per line; blank and
/// `#`-comment lines are skipped. A missing file yields `Ok(empty)` (nothing to
/// migrate); any OTHER read error is returned as `Err` so the caller does not
/// mistake an unreadable file for an empty one and consume it. Consumed by the
/// one-time migration that seeds folder-trust from prior grants.
pub fn list_trusted_projects_with_file(trust_file: &Path) -> std::io::Result<Vec<PathBuf>> {
    let content = match std::fs::read_to_string(trust_file) {
        Ok(c) => c,
        // A missing file is "nothing to migrate", not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(PathBuf::from)
        .collect())
}

// ── Hook enable/disable ─────────────────────────────────────────────────

/// Check whether a hook is disabled by name.
///
/// Disabled hooks are listed in , one hook name per line.
pub fn is_hook_disabled(hook_name: &str) -> bool {
    match disabled_hooks_file_path() {
        Some(file) => is_hook_disabled_with_file(hook_name, &file),
        None => false,
    }
}

/// The one per-spec disable rule, shared by dispatch, the dispatch guard, and
/// display: a disabled spec is skipped unless it is managed policy, which the
/// user cannot disable.
pub fn is_spec_disabled(spec: &crate::config::HookSpec) -> bool {
    is_spec_disabled_by(spec, is_hook_disabled)
}

fn is_spec_disabled_by(spec: &crate::config::HookSpec, is_listed: impl Fn(&str) -> bool) -> bool {
    !spec.is_managed_policy() && (!spec.enabled || is_listed(&spec.name))
}

fn is_hook_disabled_with_file(hook_name: &str, file: &Path) -> bool {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => return false,
    };
    content
        .lines()
        .any(|l| !l.trim().is_empty() && !l.trim().starts_with('#') && l.trim() == hook_name)
}

/// Disable a hook by name. Adds to .
pub fn disable_hook(hook_name: &str) -> Result<(), String> {
    let file = disabled_hooks_file_path()
        .ok_or_else(|| "no user grok home (set $BLADERUNNER_HOME or $HOME)".to_string())?;
    disable_hook_with_file(hook_name, &file)
}

fn disable_hook_with_file(hook_name: &str, file: &Path) -> Result<(), String> {
    if is_hook_disabled_with_file(hook_name, file) {
        return Ok(()); // Already disabled.
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .map_err(|e| format!("failed to open disabled-hooks file: {e}"))?;
    writeln!(f, "{hook_name}").map_err(|e| format!("failed to write disabled-hooks file: {e}"))?;
    Ok(())
}

/// Enable a hook by name (remove from ).
pub fn enable_hook(hook_name: &str) -> Result<bool, String> {
    match disabled_hooks_file_path() {
        Some(file) => enable_hook_with_file(hook_name, &file),
        None => Ok(false),
    }
}

fn enable_hook_with_file(hook_name: &str, file: &Path) -> Result<bool, String> {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("failed to read disabled-hooks file: {e}")),
    };
    let mut found = false;
    let new_lines: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed == hook_name {
                found = true;
                false
            } else {
                true
            }
        })
        .collect();
    if !found {
        return Ok(false);
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    let mut f = std::fs::File::create(file)
        .map_err(|e| format!("failed to open disabled-hooks file: {e}"))?;
    for line in new_lines {
        writeln!(f, "{line}").map_err(|e| format!("failed to write disabled-hooks file: {e}"))?;
    }
    Ok(true)
}

/// Returns the path to `$BLADERUNNER_HOME/disabled-hooks`, or `None` when no user grok
/// home resolves.
fn disabled_hooks_file_path() -> Option<PathBuf> {
    Some(xai_grok_config::user_grok_home()?.join("disabled-hooks"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test creates its own legacy file in its own temp dir -- no shared state.
    fn trust_file_in(dir: &Path) -> PathBuf {
        let grok_dir = dir.join(".grok");
        std::fs::create_dir_all(&grok_dir).unwrap();
        grok_dir.join("trusted-hook-projects")
    }

    #[test]
    fn list_trusted_projects_parses_paths_skipping_comments_and_blanks() {
        let home = tempfile::tempdir().unwrap();
        let trust_file = trust_file_in(home.path());
        std::fs::write(
            &trust_file,
            "# comment\n\n/abs/project/one\n  /abs/project/two  \n# trailing\n",
        )
        .unwrap();

        let projects = list_trusted_projects_with_file(&trust_file).unwrap();
        assert_eq!(
            projects,
            vec![
                PathBuf::from("/abs/project/one"),
                PathBuf::from("/abs/project/two"),
            ]
        );
    }

    #[test]
    fn list_trusted_projects_missing_file_is_empty() {
        // A missing file is Ok(empty), NOT an error — so the migration treats it
        // as "nothing to migrate" rather than as an unreadable file.
        let projects =
            list_trusted_projects_with_file(Path::new("/nonexistent/trusted-hook-projects"))
                .expect("missing file resolves to Ok(empty)");
        assert!(projects.is_empty());
    }

    #[test]
    fn disabled_list_never_skips_managed_policy_hooks() {
        use crate::config::HookProvenance;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("disabled-hooks");
        disable_hook_with_file("gate", &file).unwrap();
        let is_listed = |name: &str| is_hook_disabled_with_file(name, &file);
        let json = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"g.sh"}]}]}}"#;
        let (specs, errors) = crate::config::parse_hook_file(json, &dir.path().join("hooks.json"));
        assert!(errors.is_empty(), "{errors:?}");

        for layer in [
            HookProvenance::SystemManaged,
            HookProvenance::Managed,
            HookProvenance::Requirements,
            HookProvenance::UserRequirements,
            HookProvenance::User,
            HookProvenance::File,
            HookProvenance::Plugin,
            HookProvenance::Unknown,
        ] {
            let mut spec = specs[0].clone();
            spec.name = "gate".into();
            spec.layer = layer;
            let root_owned = matches!(
                layer,
                HookProvenance::SystemManaged | HookProvenance::Requirements
            );
            assert_eq!(
                is_spec_disabled_by(&spec, is_listed),
                !root_owned,
                "disabled-hooks entry wrong for {layer:?}"
            );
        }
    }
}
