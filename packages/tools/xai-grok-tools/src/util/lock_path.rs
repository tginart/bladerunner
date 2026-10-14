//! Canonical keys for the CLI's per-batch file operation ordering.
//!
//! Resolve these filesystem-dependent keys on a blocking thread, before dispatch.

use std::path::{Component, Path, PathBuf};

/// Production argument priority. Directory listings deliberately have no file key.
pub const PROD_LOCK_PATH_KEYS: &[&str] = &["file_path", "path", "target_file"];

/// Resolve the first string path argument exactly as file tools do, then canonicalize.
pub fn lock_path_for_args(
    args: &serde_json::Value,
    cwd: &Path,
    display_cwd: Option<&Path>,
) -> Option<String> {
    let input = PROD_LOCK_PATH_KEYS
        .iter()
        .find_map(|key| args.get(*key)?.as_str())?;
    let resolved = crate::types::resources::resolve_model_path(cwd, display_cwd, input);
    Some(lock_path_for_path(&resolved, cwd))
}

/// Collapse aliases, including existing symlinks and ancestors of new files.
///
/// Canonicalize before interpreting `..`: `symlink/..` means the target's parent,
/// which need not be the symlink's lexical parent. For nonexistent suffixes,
/// normalize components after resolving each available ancestor.
pub fn lock_path_for_path(input: &Path, cwd: &Path) -> String {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let absolute = match dunce::canonicalize(&absolute) {
        Ok(canonical) => return canonical.to_string_lossy().into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::util::fs::try_resolve_unicode_filename_sync(&absolute).unwrap_or(absolute)
        }
        Err(_) => absolute,
    };
    resolve_missing_path(&absolute, &mut 40)
        .to_string_lossy()
        .into_owned()
}

/// Ordinary canonicalization fails for dangling links, although writes still
/// follow them. Expand those destinations explicitly with a finite hop budget.
fn resolve_missing_path(absolute: &Path, remaining_links: &mut usize) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            component => {
                resolved.push(component.as_os_str());
                if let Ok(canonical) = dunce::canonicalize(&resolved) {
                    resolved = canonical;
                } else if *remaining_links > 0
                    && let Ok(target) = std::fs::read_link(&resolved)
                {
                    *remaining_links -= 1;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        resolved.parent().unwrap_or(Path::new("/")).join(target)
                    };
                    resolved = resolve_missing_path(&target, remaining_links);
                }
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_share_the_missing_destination_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("links")).unwrap();
        let target = dir.path().join("new.txt");
        std::os::unix::fs::symlink("../new.txt", dir.path().join("links/relative")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("absolute")).unwrap();
        std::os::unix::fs::symlink("links/relative", dir.path().join("chain")).unwrap();
        let expected = lock_path_for_path(&target, dir.path());
        for alias in ["links/relative", "absolute", "chain"] {
            assert_eq!(lock_path_for_path(Path::new(alias), dir.path()), expected);
        }
        std::fs::write(dir.path().join("chain"), "through link").unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "through link");

        std::os::unix::fs::symlink("missing/nested", dir.path().join("directory")).unwrap();
        for suffix in ["new.txt", "../new.txt"] {
            assert_eq!(
                lock_path_for_path(&Path::new("directory").join(suffix), dir.path()),
                lock_path_for_path(&Path::new("missing/nested").join(suffix), dir.path()),
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_terminate_without_recursing_indefinitely() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("second", dir.path().join("first")).unwrap();
        std::os::unix::fs::symlink("first", dir.path().join("second")).unwrap();
        let key = lock_path_for_path(Path::new("first/new.txt"), dir.path());
        assert!(Path::new(&key).is_absolute());
        assert!(key.ends_with("/new.txt"));
    }

    #[tokio::test]
    async fn unicode_filename_aliases_match_file_tool_resolution() {
        for whitespace in ['\u{00a0}', '\u{202f}'] {
            let dir = tempfile::tempdir().unwrap();
            let actual = dir.path().join(format!("note{whitespace}file.txt"));
            let model = dir.path().join("note file.txt");
            std::fs::write(&actual, "before").unwrap();
            let fallback = crate::util::try_resolve_unicode_filename(&model)
                .await
                .unwrap();
            assert_eq!(fallback.resolved_path, actual);
            let expected = lock_path_for_path(&actual, dir.path());
            for key in PROD_LOCK_PATH_KEYS {
                assert_eq!(
                    lock_path_for_args(&serde_json::json!({ *key: model }), dir.path(), None),
                    Some(expected.clone()),
                );
            }
            // Exact spelling wins, and an ambiguous normalized match stays distinct.
            std::fs::write(&model, "exact").unwrap();
            assert_ne!(lock_path_for_path(&model, dir.path()), expected);
            std::fs::remove_file(&model).unwrap();
            let other = if whitespace == '\u{00a0}' {
                '\u{202f}'
            } else {
                '\u{00a0}'
            };
            std::fs::write(dir.path().join(format!("note{other}file.txt")), "other").unwrap();
            assert!(
                crate::util::try_resolve_unicode_filename(&model)
                    .await
                    .is_none()
            );
            assert_ne!(lock_path_for_path(&model, dir.path()), expected);
        }
    }

    #[test]
    fn relative_absolute_and_dot_aliases_share_a_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let expected = lock_path_for_path(&dir.path().join("src/new.rs"), dir.path());
        for alias in [
            "src/new.rs",
            "./src/new.rs",
            "src/../src/new.rs",
            "absent/../src/new.rs",
        ] {
            assert_eq!(
                lock_path_for_path(Path::new(alias), dir.path()),
                expected,
                "{alias}"
            );
        }
    }

    #[test]
    fn argument_priority_and_pathless_calls() {
        let dir = tempfile::tempdir().unwrap();
        let expected = lock_path_for_path(Path::new("one"), dir.path());
        for key in PROD_LOCK_PATH_KEYS {
            assert_eq!(
                lock_path_for_args(&serde_json::json!({ *key: "one" }), dir.path(), None),
                Some(expected.clone())
            );
        }
        assert_eq!(
            lock_path_for_args(
                &serde_json::json!({"file_path": "one", "path": "two"}),
                dir.path(),
                None
            ),
            Some(expected.clone())
        );
        assert_eq!(
            lock_path_for_args(
                &serde_json::json!({"file_path": 1, "path": "one"}),
                dir.path(),
                None
            ),
            Some(expected)
        );
        for args in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({"path": 3}),
            serde_json::json!({"target_directory": "one"}),
        ] {
            assert_eq!(lock_path_for_args(&args, dir.path(), None), None);
        }
    }

    #[test]
    fn display_cwd_aliases_use_the_same_resolution_as_file_tools() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("worktree");
        let display = dir.path().join("original");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(display.join("src")).unwrap();
        // The original file really exists: canonicalizing it before remapping
        // must not accidentally choose the original checkout's lock key.
        std::fs::write(display.join("src/existing.rs"), "original").unwrap();
        std::fs::write(cwd.join("src/existing.rs"), "fork").unwrap();
        for name in ["existing.rs", "new.rs"] {
            let relative = format!("src/{name}");
            let expected = lock_path_for_path(&cwd.join(&relative), &cwd);
            for key in PROD_LOCK_PATH_KEYS {
                for input in [
                    relative.clone(),
                    display.join(&relative).to_string_lossy().into_owned(),
                    cwd.join(&relative).to_string_lossy().into_owned(),
                    format!(" \"{}\"\n", display.join(&relative).display()),
                ] {
                    assert_eq!(
                        lock_path_for_args(
                            &serde_json::json!({ *key: input }),
                            &cwd,
                            Some(&display)
                        ),
                        Some(expected.clone()),
                        "{key}: {input}"
                    );
                }
            }
        }
        for outside in [
            dir.path().join("outside.rs"),
            dir.path().join("original-extra/file.rs"),
        ] {
            assert_eq!(
                lock_path_for_args(&serde_json::json!({"path": outside}), &cwd, Some(&display)),
                Some(lock_path_for_path(&outside, &cwd))
            );
        }
        let original = display.join("src/existing.rs");
        assert_eq!(
            lock_path_for_args(&serde_json::json!({"path": original}), &cwd, None),
            Some(lock_path_for_path(&original, &cwd))
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_ancestors_and_parent_components_preserve_filesystem_meaning() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real/nested");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();
        for suffix in [
            "new.txt",
            "new/nested.txt",
            "../new.txt",
            "../absent/../new.txt",
        ] {
            assert_eq!(
                lock_path_for_path(&Path::new("alias").join(suffix), dir.path()),
                lock_path_for_path(&real.join(suffix), dir.path()),
                "{suffix}"
            );
        }
        std::fs::write(real.join("existing.txt"), "a").unwrap();
        assert_eq!(
            lock_path_for_path(Path::new("alias/existing.txt"), dir.path()),
            lock_path_for_path(&real.join("existing.txt"), dir.path())
        );
        assert_ne!(
            lock_path_for_path(Path::new("alias/../new.txt"), dir.path()),
            lock_path_for_path(Path::new("new.txt"), dir.path())
        );
    }
}
