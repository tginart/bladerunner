# GitHub pull requests

Use `gh` when available, or an available GitHub connector with equivalent read
operations. If GitHub is unavailable, use a supplied patch or local refs only
when they represent the requested PR; otherwise explain what is missing.

## Identify and inspect

Resolve the canonical repository and PR number from the URL or repository
context. A bare number without a repository needs that context. For GitHub
Enterprise, use the correct host (`--repo HOST/OWNER/REPO` and `gh api --hostname
HOST`). The examples below assume github.com.

Set `repo` to the base `OWNER/REPO` and `pr` to the validated number. Use explicit
repository selection to avoid reviewing a similarly numbered PR in the wrong
checkout:

```bash
gh pr view "$pr" --repo "$repo" --json number,url,title,body,state,isDraft,baseRefName,baseRefOid,headRefName,headRefOid,headRepository,headRepositoryOwner
gh pr diff "$pr" --repo "$repo" --name-only
gh pr diff "$pr" --repo "$repo" --color never
gh pr checks "$pr" --repo "$repo" --json name,state,bucket,link
```

Record the head SHA. A draft, closed, or already reviewed PR can still be reviewed
when requested. CI failures or pending checks do not stop the code review;
`gh pr checks` may return a nonzero status for those conditions. Distinguish them
from authentication or network failures. Do not wait for CI by default.

Read discussion when it explains intent or helps avoid duplicate feedback:

```bash
gh pr view "$pr" --repo "$repo" --comments
gh api --method GET --paginate "repos/$repo/pulls/$pr/comments"
```

The latter includes inline review comments, which the former may not show.
When the full diff is too large or incomplete, enumerate files with
`gh api --method GET --paginate "repos/$repo/pulls/$pr/files"` and inspect each
relevant file. API patches can also be truncated or absent; retrieve full source
at the reviewed SHA rather than treating missing patch text as an empty change.
Recheck the head SHA after gathering the diff and source. If it moved, refresh
the affected material so the report describes one consistent revision.

## Read the correct revision

Compare `git rev-parse HEAD` with the PR's `headRefOid` before using local files.
If the commits are present locally, `git show 'HEAD_SHA:path/to/file'` and the
corresponding base revision provide context without checkout. Use the PR diff
as the scope; do not accidentally review unrelated local edits.
For before/after comparisons, resolve the PR merge base from the exact base and
head commits or GitHub comparison metadata. The current base branch tip may
include unrelated changes and is not necessarily the patch's before-version.

For a fork PR, the head source belongs to `headRepositoryOwner` and
`headRepository`, which may differ from the base repository. To read full files
remotely, set `head_repo` to the head `OWNER/REPO`, `head` to `headRefOid`, and
`path` to the repository-relative file path with URL-encoded path segments:

```bash
gh api --method GET -H 'Accept: application/vnd.github.raw+json' "repos/$head_repo/contents/$path" -f "ref=$head"
```

Fetch deleted files from the base repository at the base revision. For larger
reviews, fetching the exact refs into a temporary clone is another option;
preserve the user's worktree and branch. If relevant context cannot be retrieved,
state the limitation and avoid findings that depend on it.

Link findings to `https://github.com/HEAD_OWNER/HEAD_REPO/blob/HEAD_SHA/PATH#LSTART-LEND`
(or the Enterprise host). Substitute the actual full SHA, URL-encode the path,
and verify the line numbers against that snapshot.

## Publishing only when requested

By default, return the review in the conversation. If the user requests a comment
or a formal review, first complete the report, read existing feedback to avoid
duplicates, and re-read `headRefOid`. If it changed, reassess the affected findings
before posting; never publish stale line references. Do not approve or request
changes unless the user requested that disposition.

Write the exact report to a temporary file with real newlines, then use the
operation the user requested:

```bash
gh pr comment "$pr" --repo "$repo" --body-file "$report_file"
gh pr review "$pr" --repo "$repo" --comment --body-file "$report_file"
```

Use a supported review API for inline comments only when requested, with the
reviewed commit SHA and valid diff path, side, and line. After an uncertain write
failure, inspect existing comments or reviews before retrying to avoid duplicates.
