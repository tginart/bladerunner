---
name: code-review
description: >-
  Review code for actionable bugs, regressions, and violated project contracts.
  Use when asked to review local changes, a branch, commit, file, or pull request.
  Supports GitHub PRs through gh and local repositories without GitHub.
metadata:
  short-description: "Review changes or a PR for bugs and regressions"
---

# Code review

Find defects the author would want to fix. Review without editing source files
or changing the index, branch, or worktree. Return findings in the conversation;
publish a GitHub comment or review only when the user explicitly requests it.
Follow the user's chosen scope and review focus. A request to fix findings is a
separate implementation step, after reporting the review.

## Choose the target

Interpret the invocation arguments as a target and optional focus, not as shell
code. Resolve and quote paths and refs before passing them to commands.

Examples:

- `/code-review` — review current local changes.
- `/code-review staged` — review only what is staged for commit.
- `/code-review branch against origin/main` — review the current branch's changes.
- `/code-review commit abc123` or `/code-review range base..head`.
- `/code-review src/auth.rs` — review that file; use its diff when available,
  or review the existing code when a whole-file review is requested.
- `/code-review https://github.com/owner/repo/pull/123` or `/code-review PR 123`.

For a GitHub PR, read [references/github.md](references/github.md).
For another host, use the available host tooling, supplied patch, or local Git
refs with the same review criteria. GitHub access is not a prerequisite.
For an explicit file review outside a Git repository, inspect the supplied files
directly and state that there is no change baseline.

For local Git work, start with `git status --short` and identify the repository
root. Choose the relevant inspection commands:

| Scope | Commands / interpretation |
|-------|---------------------------|
| Local changes | `git diff --no-ext-diff --no-textconv HEAD --` for the combined staged and unstaged tracked changes; `git ls-files --others --exclude-standard` for new files, whose contents also need review. |
| Staged only | `git diff --no-ext-diff --no-textconv --cached --`; read the index version with `git show ':path/to/file'`, since the worktree may differ. |
| Branch | Resolve the intended base, then `git merge-base BASE HEAD` and `git diff --no-ext-diff --no-textconv MERGE_BASE HEAD --`. Do not assume the base is named `main`. |
| Commit | `git show --format=fuller --no-ext-diff --no-textconv COMMIT --`; review against its parent. Resolve the intended parent for merge commits. |
| Range | `git diff --no-ext-diff --no-textconv BASE HEAD --` for an explicit endpoint range; use the merge base instead when the request is for branch changes. |
| Paths | Restrict the selected diff with `-- PATH...`; follow relevant dependencies outside those paths for evidence. |

On a repository with no commits, inspect the staged diff and new files without
assuming `HEAD` exists. With no arguments and a clean worktree, review branch
changes only if the intended base is clear from the request, PR, or repository
metadata. If there is no change or the target is ambiguous, explain that and ask
for a target instead of silently auditing the entire repository.

## Review with context

1. State the target and baseline. Read applicable project instructions, review
   guidance, and the PR description or commit message to understand intent.
   For a remote target, read guidance from that target's revision. Apply rules
   only to the paths they govern.
2. Read the diff and the relevant full files at the reviewed revision. Follow
   callers, consumers, dispatch sites, configuration, and tests when needed to
   establish impact. Do not treat a different local checkout as the reviewed code.
3. Focus on correctness, compatibility, security, data integrity, error handling,
   concurrency, and material performance regressions. For changed public types
   or commands, verify the consuming side handles the new contract. Use relevant
   test coverage to check behavior; missing tests alone are not a bug finding.
4. Validate each candidate against surrounding code and the baseline. Specify
   the input, state, or environment that triggers it, and identify the affected
   behavior. Input-dependent bugs are valid when that scenario is supported by
   evidence. Deduplicate issues with the same cause and remedy.
5. Run a focused check only when it materially resolves uncertainty and is
   appropriate for the environment. Keep any repro artifacts outside the user's
   tree; avoid installs, auto-fix commands, or checks that alter source or require
   live services. Report checks not run and missing context honestly.

For change reviews, report defects introduced or made reachable by the change.
For an explicitly requested existing-file audit, existing defects are in scope;
do not describe them as regressions without a baseline.

Exclude speculative concerns, intentional behavior, style preferences, generic
refactoring advice, and demands for rigor absent in the project. Flag a project
rule violation only when the rule actually applies, with a reference to the rule
and a concrete consequence. Passing CI is useful evidence, not proof of correctness.

## Report

Lead with findings, ordered by priority:

- **P0:** Unconditional critical failure requiring immediate action; reserve for
  release-blocking or similarly severe defects without special triggering inputs.
- **P1:** Serious defect to fix promptly, such as a reachable auth bypass, data
  loss, or a broken primary workflow.
- **P2:** Actionable bug affecting a supported scenario or edge case.
- **P3:** Low-impact but concrete defect worth correcting; omit cosmetic nits.

For each finding, use `[P1] Short actionable title`, a precise file and small
line range in the reviewed revision, and one paragraph explaining the trigger,
evidence, impact, and suggested remedy. For a change review, anchor the finding
to a changed line when possible and cite affected code elsewhere as evidence.
Use commit-pinned links for GitHub code and local file references otherwise.
Separate unresolved questions from validated findings.

Finish with a brief assessment, the scope reviewed, and checks performed or
limitations. If there are no validated findings, say so. Distinguish a completed
review with no findings from one blocked by missing code or credentials; never
claim the patch is correct when the evidence is incomplete.
