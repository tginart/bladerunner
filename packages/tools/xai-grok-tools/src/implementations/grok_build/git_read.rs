//! Fixed read-only Git operations for the review companion. No shell or argv input.
use crate::types::output::ToolOutput;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{ToolMetadata, resolve_cwd, shared_resources};
use tokio::io::{AsyncRead, AsyncReadExt};

const OUTPUT_LIMIT: usize = 64 * 1024;

/// Retain only the prefix, draining the rest with a fixed scratch buffer so
/// neither stdout nor stderr can block the child or grow our memory usage.
async fn read_capped(mut stream: impl AsyncRead + Unpin) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(OUTPUT_LIMIT);
    let mut scratch = [0u8; 8192];
    let mut truncated = false;
    loop {
        let count = stream.read(&mut scratch).await?;
        if count == 0 {
            return Ok((bytes, truncated));
        }
        let keep = count.min(OUTPUT_LIMIT - bytes.len());
        bytes.extend_from_slice(&scratch[..keep]);
        truncated |= keep < count;
    }
}

async fn capped_output(
    mut command: tokio::process::Command,
) -> std::io::Result<(std::process::ExitStatus, (Vec<u8>, bool), (Vec<u8>, bool))> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    tokio::try_join!(child.wait(), read_capped(stdout), read_capped(stderr))
}

/// Keep inherited Git argv/config/repository overrides from changing the
/// inspection target or re-enabling helpers. Repository config is read below
/// only to neutralize every configured filter before inspecting file content.
fn inspection_command(cwd: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(cwd)
        .args([
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    command
}

async fn inspection_output(
    cwd: &std::path::Path,
    args: Vec<String>,
) -> std::io::Result<(std::process::ExitStatus, (Vec<u8>, bool), (Vec<u8>, bool))> {
    // Config inspection itself does not invoke content conversions. Includes,
    // global config and per-worktree config are included by Git's normal reader.
    let mut config = inspection_command(cwd);
    config.args([
        "config",
        "--null",
        "--name-only",
        "--get-regexp",
        "^filter\\..*\\.(clean|smudge|process|required)$",
    ]);
    let (status, (keys, truncated), _) = capped_output(config).await?;
    if truncated || !(status.success() || status.code() == Some(1)) {
        return Err(std::io::Error::other(
            "Cannot safely inspect Git filter configuration",
        ));
    }
    let mut command = inspection_command(cwd);
    for key in keys.split(|b| *b == 0).filter(|key| !key.is_empty()) {
        let key = std::str::from_utf8(key).map_err(std::io::Error::other)?;
        // '=' is legal inside a config subsection but cannot be represented
        // as the key portion of Git's command-line key=value override.
        if key.contains('=') {
            return Err(std::io::Error::other(
                "Cannot safely override Git filter name",
            ));
        }
        let value = if key.ends_with(".required") {
            "false"
        } else {
            ""
        };
        command.arg("-c").arg(format!("{key}={value}"));
    }
    capped_output({
        command.args(args);
        command
    })
    .await
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitReadOperation {
    Status,
    Diff,
    Show,
    Log,
    Branches,
    Worktrees,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitReadInput {
    pub operation: GitReadOperation,
    /// Optional revision for diff/show/log. Defaults to HEAD for show/log.
    pub revision: Option<String>,
    /// Optional repository-relative file path for diff/show/log.
    pub path: Option<String>,
}

fn read_args(input: &GitReadInput) -> Result<Vec<String>, String> {
    if let Some(revision) = &input.revision {
        if revision.is_empty() || revision.starts_with('-') || revision.contains('\0') {
            return Err("Expected a Git revision, not an option".into());
        }
    }
    if let Some(path) = &input.path {
        if path.is_empty()
            || std::path::Path::new(path).is_absolute()
            || std::path::Path::new(path).components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
            || path.contains('\0')
        {
            return Err("Expected a repository-relative file path".into());
        }
    }
    let mut args: Vec<String> = match input.operation {
        GitReadOperation::Status => vec![
            "status",
            "--short",
            "--untracked-files=normal",
            "--ignore-submodules=all",
        ],
        GitReadOperation::Diff => vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=all",
        ],
        GitReadOperation::Show => vec![
            "show",
            "--no-ext-diff",
            "--no-textconv",
            "--no-show-signature",
            "--no-color",
            "--ignore-submodules=all",
        ],
        GitReadOperation::Log => vec![
            "log",
            "--oneline",
            "--max-count=30",
            "--no-show-signature",
            "--no-color",
        ],
        GitReadOperation::Branches => vec!["branch", "--list", "--all", "--no-color"],
        GitReadOperation::Worktrees => vec!["worktree", "list", "--porcelain"],
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    if matches!(
        input.operation,
        GitReadOperation::Diff | GitReadOperation::Show | GitReadOperation::Log
    ) {
        args.push("--end-of-options".into());
        if let Some(revision) = &input.revision {
            args.push(revision.clone());
        } else if matches!(
            input.operation,
            GitReadOperation::Show | GitReadOperation::Log
        ) {
            args.push("HEAD".into());
        }
        args.push("--".into());
        if let Some(path) = &input.path {
            args.push(path.clone());
        }
    } else if input.path.is_some() || input.revision.is_some() {
        return Err("This operation does not accept a path or revision".into());
    }
    Ok(args)
}

#[derive(Debug, Default)]
pub struct GitReadTool;

impl ToolMetadata for GitReadTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        "Inspect repository status, diff, show, log, branches, or worktrees. Read-only; no commands or extra arguments. Output is capped."
    }
}

impl xai_tool_runtime::Tool for GitReadTool {
    type Args = GitReadInput;
    type Output = ToolOutput;
    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("git_read").expect("valid tool id")
    }
    fn description(
        &self,
        _: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new("git_read", self.description_template())
    }
    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: GitReadInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let args =
            read_args(&input).map_err(|e| xai_tool_runtime::ToolError::custom("git_read", e))?;
        let resources = shared_resources(&ctx)?;
        let cwd = resolve_cwd(&ctx, &resources).await?;
        let (status, stdout, stderr) = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            inspection_output(&cwd, args),
        )
        .await
        .map_err(|_| xai_tool_runtime::ToolError::custom("git_read", "Git inspection timed out"))?
        .map_err(|e| xai_tool_runtime::ToolError::custom("git_read", e.to_string()))?;
        let (bytes, truncated) = if status.success() { stdout } else { stderr };
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            text.push_str("\n[Output truncated; select a narrower file or revision]");
        }
        if !status.success() {
            return Err(xai_tool_runtime::ToolError::custom("git_read", text));
        }
        Ok(ToolOutput::Text(text.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn streaming_cap_retains_only_the_prefix_and_detects_overflow() {
        for size in [0, OUTPUT_LIMIT, OUTPUT_LIMIT + 1, 100 * OUTPUT_LIMIT] {
            let (bytes, truncated) = read_capped(tokio::io::repeat(b'x').take(size as u64))
                .await
                .unwrap();
            assert_eq!(bytes.len(), size.min(OUTPUT_LIMIT));
            assert!(bytes.iter().all(|b| *b == b'x'));
            assert_eq!(truncated, size > OUTPUT_LIMIT);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capped_process_drains_both_pipes_and_preserves_exit_status() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "i=0; while [ \"$i\" -lt 10000 ]; do printf 'stdout output line\n'; printf 'stderr error line\n' >&2; i=$((i+1)); done; exit 3"]);
        let (status, (stdout, out_truncated), (stderr, err_truncated)) =
            tokio::time::timeout(std::time::Duration::from_secs(10), capped_output(command))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(stdout.len(), OUTPUT_LIMIT);
        assert_eq!(stderr.len(), OUTPUT_LIMIT);
        assert!(out_truncated && err_truncated);
        assert!(stdout.starts_with(b"stdout output line\n"));
        assert!(stderr.starts_with(b"stderr error line\n"));
    }

    #[tokio::test]
    async fn inspection_reads_real_git_without_mutating_files_index_or_head() {
        use crate::types::{
            resources::{Cwd, Resources},
            tool_metadata::test_ctx,
        };
        use xai_tool_runtime::Tool;
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(temp.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Review test"]);
        git(&["config", "user.email", "review@example.test"]);
        std::fs::write(temp.path().join("sample.txt"), "before\n").unwrap();
        git(&["add", "sample.txt"]);
        git(&["-c", "commit.gpgsign=false", "commit", "-m", "baseline"]);
        std::fs::write(temp.path().join("sample.txt"), "after\n").unwrap();
        // An external diff must never execute, even if the repository requests it.
        git(&["config", "diff.external", "definitely-not-a-real-command"]);
        let head = git(&["rev-parse", "HEAD"]);
        let index = std::fs::read(temp.path().join(".git/index")).unwrap();
        let mut resources = Resources::new();
        resources.insert(Cwd(temp.path().to_owned()));
        let resources = resources.into_shared();
        for operation in [
            GitReadOperation::Status,
            GitReadOperation::Diff,
            GitReadOperation::Show,
            GitReadOperation::Log,
            GitReadOperation::Branches,
            GitReadOperation::Worktrees,
        ] {
            let output = GitReadTool
                .run(
                    test_ctx(resources.clone()),
                    GitReadInput {
                        operation,
                        revision: None,
                        path: None,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(output, ToolOutput::Text(_)));
        }
        assert_eq!(git(&["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read(temp.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("sample.txt")).unwrap(),
            "after\n"
        );
    }
    #[test]
    fn rejects_options_traversal_and_unknown_operations() {
        for revision in ["--output=/tmp/write", "-c", ""] {
            assert!(
                read_args(&GitReadInput {
                    operation: GitReadOperation::Diff,
                    revision: Some(revision.into()),
                    path: None
                })
                .is_err()
            );
        }
        for path in ["../outside", "/tmp/outside"] {
            assert!(
                read_args(&GitReadInput {
                    operation: GitReadOperation::Show,
                    revision: None,
                    path: Some(path.into())
                })
                .is_err()
            );
        }
        assert!(
            serde_json::from_str::<GitReadInput>(
                r#"{"operation":"commit","revision":null,"path":null}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<GitReadInput>(
                r#"{"operation":"diff","args":["--output=/tmp/write"]}"#
            )
            .is_err()
        );
    }
    #[test]
    fn paths_cannot_become_options_or_pathspec_magic() {
        let args = read_args(&GitReadInput {
            operation: GitReadOperation::Diff,
            revision: Some("main".into()),
            path: Some("--output=file".into()),
        })
        .unwrap();
        assert_eq!(args[args.len() - 3..], ["main", "--", "--output=file"]);
        assert!(args.contains(&"--no-ext-diff".into()));
        assert!(args.contains(&"--no-textconv".into()));
    }
    #[tokio::test]
    async fn configured_clean_and_process_filters_never_execute() {
        use crate::types::{
            resources::{Cwd, Resources},
            tool_metadata::test_ctx,
        };
        use xai_tool_runtime::Tool;
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(temp.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Review test"]);
        git(&["config", "user.email", "review@example.test"]);
        std::fs::write(temp.path().join("sample.txt"), "before\n").unwrap();
        git(&["add", "sample.txt"]);
        git(&["-c", "commit.gpgsign=false", "commit", "-m", "baseline"]);
        std::fs::write(temp.path().join("sample.txt"), "after\n").unwrap();
        std::fs::write(temp.path().join(".gitattributes"), "*.txt filter=unsafe\n").unwrap();
        git(&["config", "filter.unsafe.clean", "touch clean-marker; cat"]);
        git(&["diff", "--no-ext-diff", "--no-textconv", "--no-color"]);
        assert!(
            temp.path().join("clean-marker").exists(),
            "reproduce the original bypass"
        );
        std::fs::remove_file(temp.path().join("clean-marker")).unwrap();
        // Process commands may come from included configuration too.
        std::fs::write(
            temp.path().join("included.config"),
            "[filter \"unsafe\"]\nprocess = touch process-marker; exit 1\nrequired = true\n",
        )
        .unwrap();
        git(&[
            "config",
            "include.path",
            temp.path().join("included.config").to_str().unwrap(),
        ]);
        let mut resources = Resources::new();
        resources.insert(Cwd(temp.path().to_owned()));
        let resources = resources.into_shared();
        for operation in [GitReadOperation::Status, GitReadOperation::Diff] {
            let output = GitReadTool
                .run(
                    test_ctx(resources.clone()),
                    GitReadInput {
                        operation,
                        revision: None,
                        path: None,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(output, ToolOutput::Text(_)));
            assert!(!temp.path().join("clean-marker").exists());
            assert!(!temp.path().join("process-marker").exists());
        }
        assert_eq!(
            std::fs::read_to_string(temp.path().join("sample.txt")).unwrap(),
            "after\n"
        );
        // Subsection names containing '=' cannot be safely overridden via -c.
        git(&[
            "config",
            "filter.eq=driver.clean",
            "touch unusual-marker; cat",
        ]);
        let result = GitReadTool
            .run(
                test_ctx(resources),
                GitReadInput {
                    operation: GitReadOperation::Diff,
                    revision: None,
                    path: None,
                },
            )
            .await;
        assert!(result.is_err());
        assert!(!temp.path().join("unusual-marker").exists());
    }
}
