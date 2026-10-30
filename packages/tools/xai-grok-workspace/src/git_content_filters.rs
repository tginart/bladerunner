//! Repo-local content-filter pins for unattended git.
//!
//! A repo's `.git/config` can define `filter.<driver>.clean/smudge/process`
//! commands, which `git status` runs when it re-hashes a dirty file. For git
//! that runs without the user asking (prompt-time status), every repo-local
//! driver is pinned to an empty command with `-c` so git never runs it.

use std::collections::BTreeSet;
use std::path::Path;

/// `git -c` arguments that neutralize every repo-local filter driver.
/// `None` when the local config can't be fully read; callers must then skip
/// the git call rather than run it unpinned.
pub(crate) fn content_filter_config_pins(cwd: &Path) -> Option<Vec<String>> {
    let drivers = local_content_filter_drivers(cwd)?;
    let mut pins = Vec::with_capacity(drivers.len() * 4);
    for driver in drivers {
        // required=false keeps status from erroring on the emptied filter.
        pins.push(format!("filter.{driver}.clean="));
        pins.push(format!("filter.{driver}.smudge="));
        pins.push(format!("filter.{driver}.process="));
        pins.push(format!("filter.{driver}.required=false"));
    }
    Some(pins)
}

fn local_content_filter_drivers(cwd: &Path) -> Option<Vec<String>> {
    let entries = read_local_git_config_entries(cwd)?;
    let mut drivers = BTreeSet::new();
    for (name, value) in entries {
        if value.trim().is_empty() {
            continue;
        }
        let Some(driver) = filter_command_driver(&name) else {
            continue;
        };
        // Hostile names can't be expressed as `git -c filter.<name>.*` (git
        // splits on the first `=`), so fail closed.
        if !filter_driver_name_is_pinnable(driver) {
            return None;
        }
        drivers.insert(driver.to_owned());
    }
    Some(drivers.into_iter().collect())
}

/// Local/worktree level entries only (including include/includeIf targets).
/// `None` on read errors; empty if `cwd` is not in a repo.
fn read_local_git_config_entries(cwd: &Path) -> Option<Vec<(String, String)>> {
    let repo = match git2::Repository::discover(cwd) {
        Ok(repo) => repo,
        Err(e)
            if e.code() == git2::ErrorCode::NotFound
                && e.class() == git2::ErrorClass::Repository =>
        {
            return Some(Vec::new());
        }
        Err(_) => return None,
    };
    // `repo.config()` can still open global levels when local is unreadable.
    let git_dir = repo.path();
    let common = repo.commondir();
    if path_unreadable(&common.join("config"))
        || path_unreadable(&git_dir.join("config"))
        || path_unreadable(&git_dir.join("config.worktree"))
    {
        return None;
    }
    let config = repo.config().ok()?;
    let mut entries = config.entries(None).ok()?;
    let mut out = Vec::new();
    while let Some(entry) = entries.next() {
        let entry = entry.ok()?;
        match entry.level() {
            git2::ConfigLevel::Local | git2::ConfigLevel::Worktree => {}
            _ => continue,
        }
        // git2 yields None for non-UTF-8. Dropping that entry would hide a
        // filter git can still run.
        let name = entry.name()?;
        // libgit2 does not evaluate `includeIf.hasconfig:remote.*.url`.
        if local_includeif_unsupported(name) {
            return None;
        }
        // A bare key is boolean true. `value()` panics when no value is defined.
        let value = if entry.has_value() {
            let Some(value) = entry.value() else {
                if filter_command_driver(name).is_some() {
                    return None;
                }
                continue;
            };
            value
        } else if filter_command_driver(name).is_some() {
            return None;
        } else {
            "true"
        };
        out.push((name.to_owned(), value.to_owned()));
    }
    Some(out)
}

fn filter_command_driver(name: &str) -> Option<&str> {
    // `get` is None on a non-char boundary; a byte slice would panic.
    let head = name.get(..7)?;
    if !head.eq_ignore_ascii_case("filter.") {
        return None;
    }
    let rest = name.get(7..)?;
    let (driver, prop) = rest.rsplit_once('.')?;
    if !driver.is_empty()
        && (prop.eq_ignore_ascii_case("clean")
            || prop.eq_ignore_ascii_case("smudge")
            || prop.eq_ignore_ascii_case("process"))
    {
        Some(driver)
    } else {
        None
    }
}

fn filter_driver_name_is_pinnable(driver: &str) -> bool {
    let mut chars = driver.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn local_includeif_unsupported(name: &str) -> bool {
    let Some(head) = name.get(..10) else {
        return false;
    };
    if !head.eq_ignore_ascii_case("includeif.") {
        return false;
    }
    let condition = name.get(10..).unwrap_or("").split(':').next().unwrap_or("");
    !matches!(
        condition.to_ascii_lowercase().as_str(),
        "gitdir" | "gitdir/i" | "onbranch"
    )
}

fn path_unreadable(path: &Path) -> bool {
    // Directories open on Linux, so require a readable regular file after
    // following symlinks.
    match std::fs::File::open(path) {
        Ok(f) => match f.metadata() {
            Ok(meta) => !meta.is_file(),
            Err(_) => true,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_config(config: &[u8]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join(".git/config"), config).unwrap();
        tmp
    }

    #[test]
    fn filter_command_driver_does_not_panic_on_multibyte() {
        assert!(filter_command_driver("alias.éxxx").is_none());
    }

    #[test]
    fn pins_outside_repo_are_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(content_filter_config_pins(tmp.path()), Some(Vec::new()));
    }

    #[test]
    fn pins_safe_lfs_process_driver() {
        let tmp = repo_with_config(
            b"[core]\n\trepositoryformatversion = 0\n\
              [filter \"lfs\"]\n\tprocess = git-lfs filter-process\n",
        );
        let pins = content_filter_config_pins(tmp.path()).expect("readable");
        assert_eq!(
            pins,
            vec![
                "filter.lfs.clean=".to_owned(),
                "filter.lfs.smudge=".to_owned(),
                "filter.lfs.process=".to_owned(),
                "filter.lfs.required=false".to_owned(),
            ]
        );
    }

    #[test]
    fn pins_valueless_boolean_key_does_not_panic() {
        let tmp = repo_with_config(
            b"[core]\n\trepositoryformatversion = 0\n\tignorecase\n\
              [filter \"lfs\"]\n\tprocess = git-lfs filter-process\n",
        );
        let pins = content_filter_config_pins(tmp.path()).expect("bare boolean is readable");
        assert!(pins.iter().any(|p| p == "filter.lfs.process="), "{pins:?}");
    }

    #[test]
    fn pins_mixed_case_driver_keeps_spelling() {
        let tmp = repo_with_config(
            b"[core]\n\trepositoryformatversion = 0\n\
              [filter \"Pwn\"]\n\tclean = /tmp/pwn\n",
        );
        let pins = content_filter_config_pins(tmp.path()).expect("readable");
        assert!(pins.iter().any(|p| p == "filter.Pwn.clean="), "{pins:?}");
        assert!(!pins.iter().any(|p| p.starts_with("filter.pwn.")));
    }

    #[test]
    fn pins_unsupported_includeif_fail_closed() {
        let tmp = repo_with_config(
            b"[core]\n\trepositoryformatversion = 0\n\
              [includeIf \"hasconfig:remote.*.url:https://example.com/**\"]\n\
              \tpath = ../filters.gitconfig\n",
        );
        assert!(content_filter_config_pins(tmp.path()).is_none());
    }

    #[test]
    fn pins_valueless_filter_command_fail_closed() {
        let tmp = repo_with_config(
            b"[core]\n\trepositoryformatversion = 0\n\
              [filter \"pwn\"]\n\tclean\n",
        );
        assert!(content_filter_config_pins(tmp.path()).is_none());
    }

    #[test]
    fn pins_non_utf8_filter_value_fail_closed() {
        let mut cfg =
            b"[core]\n\trepositoryformatversion = 0\n[filter \"pwn\"]\n\tclean = ".to_vec();
        cfg.extend_from_slice(&[0xff, 0xfe]);
        cfg.push(b'\n');
        let tmp = repo_with_config(&cfg);
        assert!(content_filter_config_pins(tmp.path()).is_none());
    }

    #[test]
    fn pins_hostile_driver_names_fail_closed() {
        for section in ["pwn=x", "pwn x", "pwn.x"] {
            let cfg = format!(
                "[core]\n\trepositoryformatversion = 0\n\
                 [filter \"{section}\"]\n\tclean = /tmp/pwn\n"
            );
            let tmp = repo_with_config(cfg.as_bytes());
            assert!(
                content_filter_config_pins(tmp.path()).is_none(),
                "section={section:?}"
            );
        }
    }

    #[test]
    fn pins_unreadable_config_are_none() {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let cfg = tmp.path().join(".git/config");
        std::fs::remove_file(&cfg).unwrap();
        std::fs::create_dir(&cfg).unwrap();
        assert!(content_filter_config_pins(tmp.path()).is_none());
    }
}
