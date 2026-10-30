//! Generates a compact git status for the system prompt.
//!
//! Uses the git CLI for performance — libgit2's status is 5-10x slower than
//! the native git binary on large repos due to inefficient index refresh.
//! Output is prioritized by change type and limited to ~1k characters.

use crate::file_system::FsError;
use crate::file_system::fsmonitor::{FsmonitorOverride, probe_fsmonitor_override};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Gets a compact git status for the system prompt using the git CLI.
///
/// Output includes:
/// 1. Branch name
/// 2. Upstream ahead/behind status
/// 3. Staged files (if any)
///
/// Total output is capped at ~1k characters.
pub async fn git_status(working_directory: impl Into<PathBuf>) -> Result<String, FsError> {
    let working_directory = working_directory.into();
    let permit = crate::git_odb::try_acquire_odb();

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        git_status_impl(&working_directory)
    })
    .await
    .map_err(|e| FsError::Other(format!("git status task failed: {e}")))?
}

/// Matches Node's default `execFile` `maxBuffer` (1 MiB). This cap is
/// load-bearing: `git status` output at or above it makes the spawn throw, so
/// the repo is dropped from `<git_status>` entirely (never truncated).
/// Oversized output is treated as an error -- the caller maps `Err` to a
/// dropped section.
const GIT_STATUS_BUFFER_LIMIT: usize = 1024 * 1024;

/// Whether `git status` stdout is large enough that the repo is dropped
/// (`>= 1 MiB`). Extracted as a pure predicate so it is unit-testable
/// without spawning git.
fn git_status_exceeds_buffer(stdout_len: usize) -> bool {
    stdout_len >= GIT_STATUS_BUFFER_LIMIT
}

/// Collapse runs of 2+ spaces to a single space.
///
/// The `<git_status>` body collapses consecutive spaces, so the
/// porcelain two-column status renders with a single separator: `A  staged.txt`
/// (index-added, clean worktree) becomes `A staged.txt`, `M  mod.txt` becomes
/// `M mod.txt`, `R  old -> new` becomes `R old -> new`. A single leading space
/// (e.g. ` M file`, worktree-modified) and the rename ` -> ` separator are
/// preserved because they are runs of length one.
/// Newlines are never touched.
fn collapse_status_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(ch);
    }
    out
}

/// `git status --short --branch` for the prompt-time `<git_status>` block.
///
/// This runs unattended in a checkout the user may not trust, so repo-local
/// config must not be able to run commands: `core.fsmonitor` is pinned (see
/// [`probe_fsmonitor_override`]) and every repo-local content filter driver
/// is emptied. Submodule work trees are not inspected, since that would run
/// git under each submodule's own config. If the future is dropped (timeout),
/// the git process group is killed.
pub async fn git_status_short(working_directory: impl Into<PathBuf>) -> Result<String, FsError> {
    let working_directory = working_directory.into();
    let fsmonitor = probe_fsmonitor_override(working_directory.clone()).await;
    git_status_short_pinned(working_directory, fsmonitor).await
}

async fn git_status_short_pinned(
    working_directory: PathBuf,
    fsmonitor: FsmonitorOverride,
) -> Result<String, FsError> {
    // Held for the whole run to bound ODB contention.
    let _permit = crate::git_odb::try_acquire_odb();

    // libgit2 config walk is blocking; keep it off the async scheduler.
    let pin_cwd = working_directory.clone();
    let filter_pins = tokio::task::spawn_blocking(move || {
        crate::git_content_filters::content_filter_config_pins(&pin_cwd)
    })
    .await
    .map_err(|e| FsError::Other(format!("git config pin task failed: {e}")))?
    .ok_or_else(|| FsError::Other("unreadable git config".to_string()))?;

    let mut cmd = xai_tty_utils::git_command();
    for pin in &filter_pins {
        cmd.args(["-c", pin.as_str()]);
    }
    cmd.args(["-c", fsmonitor.git_config_arg()]);
    // The pins above cover only the superproject's config. A dirty check of a
    // populated submodule spawns a child `git status` that reads the
    // submodule's own (equally untrusted) config, so skip it: only commit
    // changes recorded in the superproject are reported for submodules.
    cmd.args([
        "status",
        "--short",
        "--branch",
        "--untracked-files=normal",
        "--ignore-submodules=dirty",
    ])
    .current_dir(&working_directory)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());

    let output = super::process::output_killing_group_on_drop(cmd)
        .await
        .map_err(|e| {
            FsError::Other(format!(
                "git status --short --branch --untracked-files=normal failed: {}",
                e
            ))
        })?;

    if !output.status.success() {
        return Err(FsError::Other(format!(
            "git status --short --branch exited with code {:?}",
            output.status.code()
        )));
    }

    // Output >= 1 MiB is dropped entirely, not truncated. Render-time
    // truncation handles the < 1 MiB case.
    if git_status_exceeds_buffer(output.stdout.len()) {
        return Err(FsError::Other(
            "git status --short --branch output exceeded 1 MiB buffer".to_string(),
        ));
    }

    // Consecutive spaces in the status body are collapsed so staged
    // entries (`A  file` -> `A file`) match the wire format.
    Ok(collapse_status_spaces(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn git_status_impl(working_directory: &Path) -> Result<String, FsError> {
    let _timer = /* instrumentation_timer */ () ; // dev macro; noop stub ("git_status.impl")
    let max_status_chars = 1000;
    let mut output = String::with_capacity(max_status_chars);

    // Get branch name
    let branch_name = {
        let _timer = /* instrumentation_timer */ () ; // dev macro; noop stub ("git_status.branch_info")
        run_git(working_directory, &["rev-parse", "--abbrev-ref", "HEAD"])
    };

    match &branch_name {
        Some(branch) if branch == "HEAD" => {
            // Detached HEAD — get short commit hash
            if let Some(hash) = run_git(working_directory, &["rev-parse", "--short", "HEAD"]) {
                let _ = writeln!(output, "HEAD detached at {}", hash);
            }
        }
        Some(branch) => {
            let _ = writeln!(output, "On branch {}", branch);
        }
        None => {
            return Err(FsError::Other("not a git repository".to_string()));
        }
    }

    // Get upstream ahead/behind
    {
        let _timer = (); // instrumentation_timer noop stub
        if let Some(upstream_name) = run_git(
            working_directory,
            &["rev-parse", "--abbrev-ref", "@{upstream}"],
        ) && let Some(counts) = run_git(
            working_directory,
            &["rev-list", "--count", "--left-right", "@{upstream}...HEAD"],
        ) {
            let parts: Vec<&str> = counts.split_whitespace().collect();
            if let (Some(behind_str), Some(ahead_str)) = (parts.first(), parts.get(1)) {
                let behind: usize = behind_str.parse().unwrap_or(0);
                let ahead: usize = ahead_str.parse().unwrap_or(0);

                let status_msg = match (ahead, behind) {
                    (0, 0) => {
                        format!("Your branch is up to date with '{}'.", upstream_name)
                    }
                    (a, 0) => format!(
                        "Your branch is ahead of '{}' by {} commit{}.",
                        upstream_name,
                        a,
                        if a == 1 { "" } else { "s" }
                    ),
                    (0, b) => format!(
                        "Your branch is behind '{}' by {} commit{}.",
                        upstream_name,
                        b,
                        if b == 1 { "" } else { "s" }
                    ),
                    (a, b) => format!(
                        "Your branch and '{}' have diverged ({} ahead, {} behind).",
                        upstream_name, a, b
                    ),
                };
                let _ = writeln!(output, "{}", status_msg);
            }
        }
    }

    // Get staged changes (index vs HEAD) — fast, no workdir scan
    let staged_output = {
        let _timer = /* instrumentation_timer */ () ; // dev macro; noop stub ("git_status.staged")
        run_git(
            working_directory,
            &["diff", "--cached", "--name-status", "HEAD"],
        )
    };

    let mut staged: Vec<String> = Vec::new();
    if let Some(ref diff_output) = staged_output {
        for line in diff_output.lines() {
            let mut parts = line.splitn(2, '\t');
            let status_char = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("");
            if path.is_empty() {
                continue;
            }
            let formatted = match status_char.chars().next() {
                Some('A') => format!("\tnew file: {}", path),
                Some('M') => format!("\tmodified: {}", path),
                Some('D') => format!("\tdeleted: {}", path),
                Some('R') => format!("\trenamed: {}", path),
                _ => format!("\t{}: {}", status_char, path),
            };
            staged.push(formatted);
        }
    }

    // Check if clean
    if staged.is_empty() {
        let _ = writeln!(output, "\nnothing to commit, working tree clean");
        return Ok(output);
    }

    // Reserve space for truncation message
    let reserve_for_truncation = 50;
    let char_budget = max_status_chars - reserve_for_truncation;

    // Write staged files
    if !staged.is_empty() && output.len() < char_budget {
        let _ = writeln!(output, "\nChanges to be committed:");
        for (shown, line) in staged.iter().enumerate() {
            if output.len() + line.len() + 1 > char_budget {
                let remaining = staged.len() - shown;
                if remaining > 0 {
                    let _ = writeln!(output, "\t... and {} more staged", remaining);
                }
                break;
            }
            let _ = writeln!(output, "{}", line);
        }
    }

    Ok(output)
}

/// Run a read-only git command and return its stdout, trimmed.
/// Returns None on failure.
///
/// Uses `--no-optional-locks` to avoid creating `index.lock` for stat-cache
/// refreshes.  This function is called from background tasks (system prompt
/// generation) and must never contend with foreground git operations.
///
/// Like [`git_status_short`], it pins `core.fsmonitor=false` and empties every
/// repo-local content filter so repo config can't run commands.
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    // Caller is already on a blocking pool (`git_status_impl`).
    let filter_pins = crate::git_content_filters::content_filter_config_pins(cwd)?;
    let mut cmd = xai_tty_utils::git_command();
    for pin in &filter_pins {
        cmd.args(["-c", pin.as_str()]);
    }
    let output = cmd
        .args(["-c", FsmonitorOverride::Disabled.git_config_arg()])
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_buffer_cap_matches_spec() {
        assert!(!git_status_exceeds_buffer(0));
        assert!(!git_status_exceeds_buffer(GIT_STATUS_BUFFER_LIMIT - 1));
        // At or above 1 MiB -> dropped.
        assert!(git_status_exceeds_buffer(GIT_STATUS_BUFFER_LIMIT));
        assert!(git_status_exceeds_buffer(GIT_STATUS_BUFFER_LIMIT + 1));
    }

    /// Staged entries collapse the porcelain double space, while leading
    /// single spaces and ` -> ` are preserved.
    #[test]
    fn collapse_status_spaces_matches_spec() {
        let raw = "## main...origin/main\n M committed.txt\nA  staged.txt\nM  mod.txt\nR  old.txt -> new.txt\n?? untracked.txt\n";
        let want = "## main...origin/main\n M committed.txt\nA staged.txt\nM mod.txt\nR old.txt -> new.txt\n?? untracked.txt\n";
        assert_eq!(collapse_status_spaces(raw), want);
    }

    /// Newlines are never collapsed (blank lines preserved).
    #[test]
    fn collapse_status_spaces_preserves_newlines() {
        assert_eq!(collapse_status_spaces("a\n\n\nb"), "a\n\n\nb");
        assert_eq!(collapse_status_spaces(""), "");
    }

    #[cfg(unix)]
    fn init_attributed_repo(tmp: &Path) {
        let repo = git2::Repository::init(tmp).unwrap();
        std::fs::write(tmp.join(".gitattributes"), "data.txt filter=Pwn\n").unwrap();
        std::fs::write(tmp.join("data.txt"), "hello\n").unwrap();
        // Commit before installing any hook so the commit itself runs nothing.
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitattributes")).unwrap();
        index.add_path(Path::new("data.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    /// Write an executable that touches `marker` and passes stdin through.
    #[cfg(unix)]
    fn write_marker_script(tmp: &Path, marker: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = tmp.join("pwn.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexec cat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    fn append_git_config(git_dir: &Path, extra: &str) {
        let cfg_path = git_dir.join("config");
        let mut cfg = std::fs::read_to_string(&cfg_path).unwrap();
        cfg.push_str(extra);
        std::fs::write(&cfg_path, cfg).unwrap();
    }

    /// Same length as the committed body plus the index mtime, so status must
    /// re-hash the file through the clean filter.
    #[cfg(unix)]
    fn dirty_for_clean_filter(work_tree: &Path, git_dir: &Path) {
        std::fs::write(work_tree.join("data.txt"), "HELLO\n").unwrap();
        let data = std::fs::File::options()
            .write(true)
            .open(work_tree.join("data.txt"))
            .unwrap();
        let mtime = std::fs::metadata(git_dir.join("index"))
            .unwrap()
            .modified()
            .unwrap();
        data.set_modified(mtime).unwrap();
    }

    #[cfg(unix)]
    fn run_git_cli(cwd: &Path, args: &[&str]) {
        let output = xai_tty_utils::git_command_locking()
            .args(["-c", "protocol.file.allow=always"])
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn run_unpinned_status(tmp: &Path) {
        xai_tty_utils::git_command()
            .args(["status", "--short"])
            .current_dir(tmp)
            .output()
            .expect("unpinned status");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_status_short_does_not_run_content_filters() {
        let tmp = tempfile::tempdir().unwrap();
        init_attributed_repo(tmp.path());
        let marker = tmp.path().join("pwned");
        let script = write_marker_script(tmp.path(), &marker);
        let git_dir = tmp.path().join(".git");
        append_git_config(
            &git_dir,
            &format!("\n[filter \"Pwn\"]\n\tclean = {}\n", script.display()),
        );
        dirty_for_clean_filter(tmp.path(), &git_dir);

        run_unpinned_status(tmp.path());
        assert!(marker.exists(), "unpinned status must run the filter");
        std::fs::remove_file(&marker).unwrap();

        git_status_short(tmp.path()).await.expect("pinned status");
        assert!(!marker.exists(), "git_status_short must not run the filter");
        git_status(tmp.path()).await.expect("summary status");
        assert!(!marker.exists(), "git_status must not run the filter");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_status_short_does_not_run_fsmonitor_hook() {
        let tmp = tempfile::tempdir().unwrap();
        init_attributed_repo(tmp.path());
        let marker = tmp.path().join("pwned");
        let script = write_marker_script(tmp.path(), &marker);
        append_git_config(
            &tmp.path().join(".git"),
            &format!("\n[core]\n\tfsmonitor = {}\n", script.display()),
        );

        run_unpinned_status(tmp.path());
        assert!(
            marker.exists(),
            "unpinned status must run the fsmonitor hook"
        );
        std::fs::remove_file(&marker).unwrap();

        git_status_short(tmp.path()).await.expect("pinned status");
        assert!(!marker.exists(), "git_status_short must not run the hook");
        git_status(tmp.path()).await.expect("summary status");
        assert!(!marker.exists(), "git_status must not run the hook");
    }

    /// A submodule's config lives in `.git/modules/<name>/config`, which the
    /// superproject pins do not cover; status must not run git inside it.
    #[cfg(unix)]
    #[tokio::test]
    async fn git_status_short_does_not_run_submodule_content_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        let superproject = tmp.path().join("super");
        std::fs::create_dir_all(&upstream).unwrap();
        std::fs::create_dir_all(&superproject).unwrap();
        init_attributed_repo(&upstream);
        run_git_cli(&superproject, &["init", "-q"]);
        run_git_cli(
            &superproject,
            &["submodule", "add", "-q", upstream.to_str().unwrap(), "sub"],
        );
        run_git_cli(&superproject, &["commit", "-qm", "add sub"]);

        let marker = tmp.path().join("pwned");
        let script = write_marker_script(tmp.path(), &marker);
        let sub_git_dir = superproject.join(".git/modules/sub");
        append_git_config(
            &sub_git_dir,
            &format!("\n[filter \"Pwn\"]\n\tclean = {}\n", script.display()),
        );
        dirty_for_clean_filter(&superproject.join("sub"), &sub_git_dir);

        run_unpinned_status(&superproject);
        assert!(
            marker.exists(),
            "unpinned status must run the submodule filter"
        );
        std::fs::remove_file(&marker).unwrap();

        git_status_short(&superproject)
            .await
            .expect("pinned status");
        assert!(!marker.exists(), "git_status_short must not run the filter");
        git_status(&superproject).await.expect("summary status");
        assert!(!marker.exists(), "git_status must not run the filter");
    }
}
