//! Read GitHub snapshots through the user's GitHub CLI account.
//! Input is a branch-relative PR number, a pull request URL, or the current branch.
//! Output is a review index with one independently selectable item per file or discussion.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::process::Command;
use url::Url;

const FIELDS: &str = "id,url,number,title,body,state,isDraft,author,baseRefName,baseRefOid,headRefName,headRefOid,reviewDecision,additions,deletions,changedFiles";
const THREADS: &str = "query($id:ID!, $endCursor:String) {
  node(id:$id) { ... on PullRequest { reviewThreads(first:50, after:$endCursor) {
    nodes { id path line originalLine isResolved isOutdated comments(first:100) {
      totalCount repliesPage: pageInfo { hasNextPage next: endCursor } nodes { author { login } body createdAt diffHunk }
    } }
    pageInfo { hasNextPage endCursor }
  } } }
}";

const REPLIES: &str = "query($id:ID!, $after:String!) {
  node(id:$id) { ... on PullRequestReviewThread { comments(first:100, after:$after) {
    totalCount pageInfo { hasNextPage next: endCursor } nodes { author { login } body createdAt }
  } } }
}";

#[derive(Debug)]
pub struct Snapshot {
    pub badge: String,
    pub subject: String,
    pub stats: String,
    pub checks: String,
    pub url: String,
    pub title: String,
    pub head: String,
    pub overview: Item,
    pub files: Vec<Item>,
    pub discussions: Vec<Item>,
}

#[derive(Debug)]
pub struct Item {
    pub label: String,
    pub location: String,
    pub parts: Vec<String>,
}

impl Snapshot {
    /// Purpose: Give a PR-wide question a sample of both the diff and discussion.
    /// Input: The loaded snapshot. Output: Labeled excerpts with explicit omission markers.
    pub fn overview_context(&self) -> String {
        let mut context = self.overview.parts[0].clone();
        context.push_str("\n\nChanged files and diffs\n");
        append_excerpts(&mut context, &self.files, 48 * 1024);
        context.push_str("\n\nDiscussion\n");
        append_excerpts(&mut context, &self.discussions, 24 * 1024);
        context
    }
}

fn append_excerpts(context: &mut String, items: &[Item], budget_bytes: usize) {
    let limit = context.len() + budget_bytes;
    for item in items {
        for text in
            std::iter::once(item.label.as_str()).chain(item.parts.iter().map(String::as_str))
        {
            let available = limit.saturating_sub(context.len());
            if text.len() >= available {
                context.push_str(&text[..text.floor_char_boundary(available)]);
                context.push_str("\n[More content available in the viewer.]\n");
                return;
            }
            context.push_str(text);
            context.push('\n');
        }
    }
}

impl Item {
    fn single(label: String, location: String, body: String) -> Self {
        Self {
            label,
            location,
            parts: vec![body],
        }
    }
}

/// Resolve the target once, then fetch all sections against its canonical repository.
/// Keep the other sections readable if a secondary endpoint fails.
pub async fn load(cwd: &Path, target: &str) -> Result<Snapshot> {
    let mut args = vec!["pr", "view", "--json", FIELDS];
    if !target.is_empty() {
        args.extend(["--", target]);
    }
    let pr: Metadata = serde_json::from_slice(&gh(cwd, &args).await?)?;
    let (host, repo, _) = pr_location(&pr.url)?;
    let api = format!("repos/{repo}/pulls/{}", pr.number);
    let files_path = format!("{api}/files?per_page=100");
    let reviews_path = format!("{api}/reviews?per_page=100");
    let comments_path = format!("repos/{repo}/issues/{}/comments?per_page=100", pr.number);
    let id = format!("id={}", pr.id);
    let query = format!("query={THREADS}");
    let thread_args = [
        "graphql",
        "-f",
        &id,
        "-f",
        &query,
        "--jq",
        ".data.node.reviewThreads.nodes",
    ];
    let (files, comments, reviews, threads) = tokio::join!(
        pages::<File>(cwd, &host, [files_path.as_str()]),
        pages::<Comment>(cwd, &host, [comments_path.as_str()]),
        pages::<Review>(cwd, &host, [reviews_path.as_str()]),
        read_threads(cwd, &host, thread_args),
    );
    // A push during pagination can mix patches from different revisions.
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Revisions {
        base_ref_oid: String,
        head_ref_oid: String,
        status_check_rollup: Option<Vec<StatusCheck>>,
    }
    let refs = match gh(
        cwd,
        &[
            "pr",
            "view",
            "--json",
            "baseRefOid,headRefOid,statusCheckRollup",
            "--",
            &pr.url,
        ],
    )
    .await
    {
        Ok(bytes) => bytes,
        // Some repository tokens can read diffs but cannot read checks.
        Err(_) => {
            gh(
                cwd,
                &[
                    "pr",
                    "view",
                    "--json",
                    "baseRefOid,headRefOid",
                    "--",
                    &pr.url,
                ],
            )
            .await?
        }
    };
    let latest: Revisions = serde_json::from_slice(&refs)?;
    ensure!(
        latest.base_ref_oid == pr.base_ref_oid && latest.head_ref_oid == pr.head_ref_oid,
        "The pull request changed while it was loading. Open /pr again for the latest revision."
    );
    let title = format!("{repo} #{} · {}", pr.number, pr.title);
    let mut overview = pr.overview();
    let files = section(files, "Files", &mut overview);
    if files.len() != pr.changed_files {
        overview.push_str(&format!(
            "\nShowing {} of {} changed files.",
            files.len(),
            pr.changed_files
        ));
    }
    let comments = section(comments, "Comments", &mut overview);
    let reviews = section(reviews, "Reviews", &mut overview);
    let thread_status = threads.as_ref().map_or_else(
        |_| "Threads unavailable".to_owned(),
        |items| {
            let count = items.iter().filter(|thread| !thread.is_resolved).count();
            format!("{count} open thread{}", if count == 1 { "" } else { "s" })
        },
    );
    let threads = section(threads, "Inline threads", &mut overview);
    let stats = format!(
        "{} files · +{} −{} · {thread_status}",
        pr.changed_files, pr.additions, pr.deletions
    );
    let mut checks = check_summary(latest.status_check_rollup.as_deref());
    if !pr.review_decision.is_empty() {
        checks = format!(
            "{} · {checks}",
            pr.review_decision.replace('_', " ").to_lowercase()
        );
    }
    overview.push_str(&format!("\n{checks} · {thread_status}\n\n"));
    overview.push_str(if pr.body.trim().is_empty() {
        "No description supplied."
    } else {
        &pr.body
    });
    Ok(Snapshot {
        badge: format!(
            "PR #{} · {}",
            pr.number,
            if pr.is_draft { "DRAFT" } else { &pr.state }
        ),
        subject: pr.title,
        stats,
        checks,
        url: pr.url,
        title,
        head: pr.head_ref_oid,
        overview: Item::single(
            "Overview".into(),
            "Pull request description".into(),
            overview,
        ),
        files: files.into_iter().map(File::into_item).collect(),
        discussions: discussion_items(comments, reviews, threads),
    })
}

/// Normalize a typed PR number or the link copied from a GitHub PR tab.
pub fn target(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(String::new());
    }
    if let Ok(n) = input.trim_start_matches('#').parse::<u64>()
        && n > 0
    {
        return Ok(n.to_string());
    }
    let (host, repo, number) = pr_location(input)?;
    Ok(format!("https://{host}/{repo}/pull/{number}"))
}

/// Purpose: Keep review decisions, general comments and inline replies in conversation order.
/// Input: The three GitHub discussion collections. Output: One row per review, comment or thread.
fn discussion_items(
    comments: Vec<Comment>,
    reviews: Vec<Review>,
    threads: Vec<Thread>,
) -> Vec<Item> {
    let mut discussion = Vec::new();
    for c in comments {
        let label = format!("@{} · comment", login(&c.user));
        discussion.push((
            c.created_at.clone(),
            Item::single(
                label,
                "Conversation".into(),
                format!("{}\n\n{}", c.created_at, c.body),
            ),
        ));
    }
    for r in reviews {
        let label = format!("@{} · {}", login(&r.user), r.state.replace('_', " "));
        let body = r.body.unwrap_or_default();
        if body.trim().is_empty() && r.state == "COMMENTED" {
            continue;
        }
        let date = r.submitted_at.unwrap_or_default();
        discussion.push((
            date.clone(),
            Item::single(label, "Review".into(), format!("{date}\n\n{body}")),
        ));
    }
    discussion.extend(threads.into_iter().map(Thread::into_item));
    discussion.sort_by(|a, b| a.0.cmp(&b.0));
    discussion.into_iter().map(|(_, item)| item).collect()
}

fn pr_location(input: &str) -> Result<(String, String, u64)> {
    let usage = "Use /pr, /pr <number>, or /pr <HTTPS pull request URL>.";
    let url = Url::parse(input).context(usage)?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none(),
        usage
    );
    let host = url.domain().context(usage)?;
    let path: Vec<_> = url.path().trim_matches('/').split('/').collect();
    ensure!(path.len() >= 4 && path[2] == "pull", usage);
    for name in &path[..2] {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
            usage
        );
    }
    let number: u64 = path[3].parse().context(usage)?;
    ensure!(number > 0, usage);
    Ok((host.to_owned(), format!("{}/{}", path[0], path[1]), number))
}

async fn gh(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("gh");
    command
        .current_dir(cwd)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .context("GitHub request timed out")?
        .context("Could not start gh. Install GitHub CLI and run gh auth login.")?;
    if !output.status.success() {
        bail!(
            "gh exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

async fn pages<'a, T: DeserializeOwned>(
    cwd: &Path,
    host: &'a str,
    endpoint: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<T>> {
    let mut args = vec!["api", "--hostname", host, "--paginate"];
    args.extend(endpoint);
    let bytes = gh(cwd, &args).await?;
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_slice(&bytes).into_iter::<Vec<T>>() {
        items.extend(page?);
    }
    Ok(items)
}

/// Fetch any replies beyond the first page of an inline thread.
async fn read_threads(cwd: &Path, host: &str, args: [&str; 7]) -> Result<Vec<Thread>> {
    let mut threads = pages::<Thread>(cwd, host, args).await?;
    let query = format!("query={REPLIES}");
    for thread in &mut threads {
        let id = format!("id={}", thread.id);
        while thread.comments.page_info.has_next_page {
            let cursor = thread
                .comments
                .page_info
                .next
                .as_deref()
                .context("GitHub omitted the next reply cursor")?;
            let after = format!("after={cursor}");
            let bytes = gh(
                cwd,
                &[
                    "api",
                    "--hostname",
                    host,
                    "graphql",
                    "-f",
                    &id,
                    "-f",
                    &after,
                    "-f",
                    &query,
                    "--jq",
                    ".data.node.comments",
                ],
            )
            .await?;
            let page: ThreadComments = serde_json::from_slice(&bytes)?;
            ensure!(
                !page.page_info.has_next_page || page.page_info.next.as_deref() != Some(cursor),
                "GitHub repeated a reply page"
            );
            thread.comments.nodes.extend(page.nodes);
            thread.comments.page_info = page.page_info;
        }
    }
    Ok(threads)
}

#[derive(Deserialize)]
struct StatusCheck {
    state: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
}

fn check_summary(checks: Option<&[StatusCheck]>) -> String {
    let Some(checks) = checks else {
        return "Checks unavailable".into();
    };
    if checks.is_empty() {
        return "No checks reported".into();
    }
    let (mut passed, mut pending, mut failed, mut skipped) = (0, 0, 0, 0);
    for check in checks {
        let state = check
            .conclusion
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(check.state.as_deref())
            .or(check.status.as_deref())
            .unwrap_or("PENDING");
        match state {
            "SUCCESS" => passed += 1,
            "NEUTRAL" | "SKIPPED" => skipped += 1,
            "CANCELLED" | "FAILURE" | "ERROR" | "TIMED_OUT" | "ACTION_REQUIRED" | "STALE"
            | "STARTUP_FAILURE" => failed += 1,
            _ => pending += 1,
        }
    }
    let counts = [
        (failed, "failed"),
        (pending, "pending"),
        (passed, "passed"),
        (skipped, "skipped"),
    ];
    let labels: Vec<_> = counts
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
    format!("Checks · {}", labels.join(" · "))
}

fn section<T>(result: Result<Vec<T>>, name: &str, overview: &mut String) -> Vec<T> {
    result.unwrap_or_else(|e| {
        overview.push_str(&format!("\n{name} unavailable: {e:#}"));
        vec![]
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    id: String,
    url: String,
    number: u64,
    title: String,
    body: String,
    state: String,
    is_draft: bool,
    author: Option<Author>,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_name: String,
    head_ref_oid: String,
    review_decision: String,
    changed_files: usize,
    additions: usize,
    deletions: usize,
}

impl Metadata {
    fn overview(&self) -> String {
        format!(
            "{} · @{}\n{} → {}\n{} files · +{} −{}",
            if self.is_draft { "DRAFT" } else { &self.state },
            login(&self.author),
            self.head_ref_name,
            self.base_ref_name,
            self.changed_files,
            self.additions,
            self.deletions
        )
    }
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

fn login(author: &Option<Author>) -> &str {
    author.as_ref().map_or("deleted", |a| a.login.as_str())
}

#[derive(Deserialize)]
struct Comment {
    user: Option<Author>,
    created_at: String,
    body: String,
}

#[derive(Deserialize)]
struct Review {
    user: Option<Author>,
    submitted_at: Option<String>,
    state: String,
    body: Option<String>,
}

#[derive(Deserialize)]
struct File {
    filename: String,
    previous_filename: Option<String>,
    status: String,
    additions: usize,
    deletions: usize,
    patch: Option<String>,
}

impl File {
    fn into_item(self) -> Item {
        let label = self.filename.clone();
        let path = match self.previous_filename {
            Some(old) => format!("{old} → {}", self.filename),
            None => self.filename,
        };
        let location = format!(
            "{path} · {} · +{} −{}",
            self.status, self.additions, self.deletions
        );
        let mut parts = Vec::new();
        if let Some(patch) = self.patch {
            for line in patch.split_inclusive('\n') {
                if parts.is_empty() || line.starts_with("@@ ") {
                    parts.push(String::new());
                }
                parts.last_mut().expect("hunk exists").push_str(line);
            }
        }
        if parts.is_empty() {
            parts.push("GitHub supplied no textual patch for this file.".into());
        }
        Item {
            label,
            location,
            parts,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    id: String,
    path: String,
    line: Option<u32>,
    original_line: Option<u32>,
    is_resolved: bool,
    is_outdated: bool,
    comments: ThreadComments,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadComments {
    #[serde(alias = "repliesPage")]
    page_info: PageInfo,
    total_count: usize,
    nodes: Vec<Reply>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    next: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    author: Option<Author>,
    body: String,
    created_at: String,
    diff_hunk: Option<String>,
}

impl Thread {
    /// Keep the original code excerpt and all returned replies together.
    fn into_item(self) -> (String, Item) {
        let mut location = self.path;
        if let Some(line) = self.line.or(self.original_line) {
            location.push_str(&format!(":{line}"));
        }
        let label = format!(
            "{} · {location}{}",
            if self.is_resolved { "Resolved" } else { "Open" },
            if self.is_outdated { " (outdated)" } else { "" }
        );
        let replies = self.comments.nodes;
        let first = replies.first();
        let date = first.map(|r| r.created_at.clone()).unwrap_or_default();
        let mut body = String::new();
        for reply in &replies {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!(
                "@{} · {}\n{}",
                login(&reply.author),
                reply.created_at,
                reply.body
            ));
        }
        if replies.len() < self.comments.total_count {
            body.push_str(&format!(
                "\n\nShowing {} of {} comments.",
                replies.len(),
                self.comments.total_count
            ));
        }
        if let Some(excerpt) = first.and_then(|r| r.diff_hunk.as_deref()) {
            body.push_str("\n\nReferenced diff\n");
            body.push_str(excerpt);
        }
        (date, Item::single(label, location, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_current_number_and_enterprise_url_and_rejects_flags() {
        assert_eq!(target(" ").unwrap(), "");
        assert_eq!(target("#13").unwrap(), "13");
        assert_eq!(
            target("https://git.example.org/team/code/pull/13/files#diff-aa").unwrap(),
            "https://git.example.org/team/code/pull/13"
        );
        for invalid in [
            "--web",
            "12 --repo example/other",
            "0",
            "https://git.example.org/team/code/issues/13",
        ] {
            assert!(target(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn renamed_and_binary_files_keep_their_identity_without_a_checkout() {
        let file: File = serde_json::from_value(serde_json::json!({
            "filename":"new.rs", "previous_filename":"old.rs", "status":"renamed",
            "additions":2, "deletions":0, "patch":"@@ -1 +1 @@\n+first\n@@ -8 +8 @@\n+second"
        }))
        .unwrap();
        let item = file.into_item();
        assert!(item.location.contains("old.rs → new.rs"));
        assert_eq!(
            item.parts,
            ["@@ -1 +1 @@\n+first\n", "@@ -8 +8 @@\n+second"]
        );
        let binary: File = serde_json::from_value(serde_json::json!({
            "filename":"icon.png", "status":"added", "additions":0, "deletions":0
        }))
        .unwrap();
        assert!(binary.into_item().parts[0].contains("no textual patch"));
    }

    #[test]
    fn inline_discussion_keeps_status_authors_replies_and_original_code_together() {
        let thread: Thread = serde_json::from_value(serde_json::json!({
            "id":"thread-test", "path":"app.rs", "originalLine":8, "isResolved":true, "isOutdated":true,
            "comments":{"totalCount":2,"pageInfo":{"hasNextPage":false},"nodes":[
                {"author":{"login":"jo"}, "body":"Could this block?", "createdAt":"2026-01-01", "diffHunk":"@@ -8 +8 @@\n+wait()"},
                {"author":null, "body":"Fixed in the next commit.", "createdAt":"2026-01-02"}
            ]}
        })).unwrap();
        let (_, item) = thread.into_item();
        assert!(item.label.contains("Resolved · app.rs:8 (outdated)"));
        let body = &item.parts[0];
        assert!(body.contains("@jo · 2026-01-01\nCould this block?"));
        assert!(body.contains("@deleted · 2026-01-02\nFixed in the next commit."));
        assert!(body.contains("+wait()"));
    }
}
