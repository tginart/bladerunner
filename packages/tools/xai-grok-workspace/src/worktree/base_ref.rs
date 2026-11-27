//! Purpose: This module finds the ref that a new worktree starts from when the
//! user selects the default branch of `origin` as the base.
//!
//! Note: The pager calls this module only for a new worktree session with no
//! explicit `--worktree-ref`. Resume, fork, and subagent worktrees use `HEAD`.

use std::path::Path;
use std::time::Duration;

use git2::Repository;

use crate::session::git::{detect_default_branch, git_cli_mut};

const REMOTE_NAME: &str = "origin";
// Offline or slow remotes must not block worktree creation for long.
const FETCH_TIMEOUT_S: u64 = 10;

/// Purpose: Find the latest commit of the default branch on `origin`.
/// Input: A path inside a git repository or a linked worktree.
/// Output: A ref such as `origin/main`. `None` when the repository has no
/// `origin` remote or no `origin/<default>` ref.
/// Method:
///     1. Find the default branch from the local `origin` refs.
///     2. Fetch that branch from `origin`, with a time limit.
///     3. Return `origin/<branch>` when that ref exists after the fetch.
/// Note: A failed fetch is not an error. The function then uses the local
/// `origin/<branch>` ref, which can be stale. The caller uses `HEAD` on `None`.
pub async fn resolve_origin_base_ref(cwd: &Path) -> Option<String> {
    let cwd_owned = cwd.to_path_buf();
    let branch = tokio::task::spawn_blocking(move || find_origin_default_branch(&cwd_owned))
        .await
        .ok()
        .flatten()?;
    fetch_origin_branch(cwd, &branch).await;
    let cwd_owned = cwd.to_path_buf();
    let branch_owned = branch.clone();
    let has_ref = tokio::task::spawn_blocking(move || has_origin_branch(&cwd_owned, &branch_owned))
        .await
        .unwrap_or(false);
    if !has_ref {
        tracing::warn!(
            cwd = %cwd.display(),
            branch,
            "worktree base: origin/<default> ref missing; using HEAD"
        );
        return None;
    }
    Some(format!("{REMOTE_NAME}/{branch}"))
}

fn find_origin_default_branch(cwd: &Path) -> Option<String> {
    let repo = Repository::discover(cwd).ok()?;
    repo.find_remote(REMOTE_NAME).ok()?;
    detect_default_branch(&repo)
}

async fn fetch_origin_branch(cwd: &Path, branch: &str) {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{REMOTE_NAME}/{branch}");
    let args = ["fetch", "--quiet", "--no-tags", REMOTE_NAME, refspec.as_str()];
    let timeout = Duration::from_secs(FETCH_TIMEOUT_S);
    match tokio::time::timeout(timeout, git_cli_mut(cwd, &args)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(
            cwd = %cwd.display(),
            branch,
            error = %e,
            "worktree base: fetch failed; using local origin ref"
        ),
        Err(_) => tracing::warn!(
            cwd = %cwd.display(),
            branch,
            timeout_s = FETCH_TIMEOUT_S,
            "worktree base: fetch timed out; using local origin ref"
        ),
    }
}

fn has_origin_branch(cwd: &Path, branch: &str) -> bool {
    let Ok(repo) = Repository::discover(cwd) else {
        return false;
    };
    repo.find_branch(&format!("{REMOTE_NAME}/{branch}"), git2::BranchType::Remote)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", rev])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn commit(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), name).unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", name]);
    }

    #[tokio::test]
    async fn returns_none_without_origin_remote() {
        let tmp = tempfile::tempdir().unwrap();
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        commit(tmp.path(), "a");
        assert_eq!(resolve_origin_base_ref(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn returns_none_outside_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_origin_base_ref(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn fetches_latest_origin_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        let clone = tmp.path().join("clone");
        std::fs::create_dir(&upstream).unwrap();
        run_git(&upstream, &["init", "-q", "-b", "main"]);
        commit(&upstream, "a");
        run_git(
            tmp.path(),
            &["clone", "-q", upstream.to_str().unwrap(), clone.to_str().unwrap()],
        );
        // The clone is now behind: upstream has a commit the clone never fetched.
        commit(&upstream, "b");
        let upstream_tip = rev_parse(&upstream, "HEAD");
        assert_ne!(rev_parse(&clone, "origin/main"), upstream_tip);

        let base = resolve_origin_base_ref(&clone).await;

        assert_eq!(base.as_deref(), Some("origin/main"));
        assert_eq!(rev_parse(&clone, "origin/main"), upstream_tip);
    }
}
