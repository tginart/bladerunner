//! n-shot groups: one parent session forked into N top-level shot sessions.
//!
//! Each shot is a normal `/fork --worktree` session whose first prompt is the
//! user's prompt with `/n-shot N` removed. The pager keeps the group here, so
//! the parent can show a live widget, the dashboard can nest the shots, and
//! the parent can compare the shots once every one of them has settled.

use std::path::Path;

use super::agent::{AgentId, GoalDisplayStatus};
use super::agent_view::AgentView;
use crate::views::dashboard::row::classify_top_level;
use crate::views::dashboard::state::RowState;

/// One `/n-shot` run, owned by its parent agent.
#[derive(Debug, Clone)]
pub(crate) struct NShotGroup {
    /// Display name, unique per parent: `n-shot-1`, `n-shot-2`, ...
    pub label: String,
    pub parent: AgentId,
    /// The shot prompt, with `/n-shot N` removed.
    pub prompt: String,
    /// Shot agents, in shot order.
    pub shots: Vec<AgentId>,
    pub shot_labels: Vec<String>,
    /// File snapshot that must complete before the join or another write.
    pub pending_write: Option<Vec<ShotSummary>>,
    pub write_retry_at: Option<std::time::Instant>,
    /// True after the parent was asked to compare the shots.
    pub joined: bool,
    /// Shot statuses at the last tick, to write the record only on change.
    pub last_statuses: Vec<ShotStatus>,
    pub started_at: std::time::Instant,
    /// Time from launch until every shot settled. Set at the join.
    pub settled_after: Option<std::time::Duration>,
    /// Durable shot identities, in shot order. A session can load on demand.
    /// A slot is empty until the session has an id.
    pub stored: Vec<Option<StoredShot>>,
}

impl NShotGroup {
    /// Run time: until the join once settled, else until now.
    pub fn elapsed(&self) -> std::time::Duration {
        self.settled_after
            .unwrap_or_else(|| self.started_at.elapsed())
    }
}

/// A shot as `shots.json` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredShot {
    pub label: String,
    pub session_id: Option<String>,
    pub creation_error: Option<String>,
    pub worktree: Option<std::path::PathBuf>,
    pub status: ShotStatus,
}

/// A group as `shots.json` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredGroup {
    pub label: String,
    pub prompt: String,
    pub joined: bool,
    pub elapsed: Option<std::time::Duration>,
    pub shots: Vec<StoredShot>,
}

/// Marks an agent as shot `index` of `total` in the group `label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NShotMember {
    pub shot_label: String,
    pub creation_error: Option<String>,
    pub label: String,
    pub index: usize,
    pub total: usize,
    pub parent: AgentId,
}

/// One shot to start: its first prompt and an optional model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShotSpec {
    pub label: Option<String>,
    pub prompt: String,
    pub model: Option<String>,
}

/// An `n_shot` tool call accepted from the shell. It starts when the parent's
/// turn ends, so no shot forks a half-finished turn.
#[derive(Debug, Clone)]
pub(crate) struct NShotRequest {
    pub parent: AgentId,
    pub label: String,
    pub shots: Vec<ShotSpec>,
}

/// A shot with its own model: the first prompt waits until the model is set.
#[derive(Debug, Clone)]
pub(crate) struct PendingShotStart {
    pub prompt: String,
    pub model: agent_client_protocol::ModelId,
    /// When the model switch was sent; `None` until the session loaded.
    pub requested_at: Option<std::time::Instant>,
}

/// Purpose: Read the shots from the `n-shot` workflow args of an `n_shot` call.
/// Input: The args: `prompt` and `n`, or `shots` with a `prompt` and `model` each.
/// Output: One spec per shot, or an error message.
pub(crate) fn parse_request_shots(args: &serde_json::Value) -> Result<Vec<ShotSpec>, String> {
    let max = xai_grok_tools::implementations::grok_build::n_shot::N_SHOT_MAX_SHOTS;
    let text = |v: &serde_json::Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let default_prompt = text(&args["prompt"]);
    let count = match args["shots"].as_array() {
        Some(items) => items.len() as u64,
        None => match args.get("n").filter(|value| !value.is_null()) {
            Some(value) => value
                .as_u64()
                .ok_or_else(|| format!("an n-shot run needs 1 to {max} shots"))?,
            None => 2,
        },
    };
    if count == 0 || count > max as u64 {
        return Err(format!("an n-shot run needs 1 to {max} shots"));
    }
    let shots: Vec<ShotSpec> = match args["shots"].as_array() {
        Some(items) => items
            .iter()
            .map(|item| {
                let prompt = text(&item["prompt"])
                    .or_else(|| default_prompt.clone())
                    .ok_or_else(|| "every shot needs a prompt".to_string())?;
                Ok(ShotSpec {
                    label: text(&item["label"]),
                    prompt,
                    model: text(&item["model"]),
                })
            })
            .collect::<Result<_, String>>()?,
        None => {
            let prompt = default_prompt.ok_or_else(|| "no prompt".to_string())?;
            vec![
                ShotSpec {
                    label: None,
                    prompt,
                    model: None
                };
                count as usize
            ]
        }
    };
    Ok(shots)
}

/// The open side-by-side columns view of one group.
#[derive(Debug, Clone, Default)]
pub(crate) struct NShotColumnsState {
    pub parent: Option<AgentId>,
    pub label: String,
    /// Focused shot, 0-based. Clamped when the view is drawn.
    pub focus: usize,
}

/// What a shot is doing, as the n-shot widget and the join see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShotStatus {
    /// The fork or its worktree is still being created.
    Starting,
    Working,
    NeedsInput,
    /// A `/goal` is active between turns.
    Goal,
    /// Scheduled `/loop` tasks are live.
    Looping {
        count: usize,
    },
    /// Nothing is running and nothing is scheduled.
    Settled,
    /// The shot session was closed.
    Closed,
    /// The fork or its worktree failed before a session was created.
    Failed,
}

impl ShotStatus {
    /// True when the shot needs nothing more before the parent compares.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Settled | Self::Closed | Self::Failed)
    }

    pub fn label(self) -> String {
        match self {
            Self::Starting => "starting".into(),
            Self::Working => "working".into(),
            Self::NeedsInput => "needs input".into(),
            Self::Goal => "goal active".into(),
            Self::Looping { count: 1 } => "loop active".into(),
            Self::Looping { count } => format!("{count} loops active"),
            Self::Settled => "settled".into(),
            Self::Closed => "closed".into(),
            Self::Failed => "failed".into(),
        }
    }
}

/// Purpose: Classify one shot for the widget and for the join.
/// Input: The shot agent, or `None` when the agent was closed.
/// Output: The shot status.
/// Method:
///     1. A creation error is failed. A missing session without an error is still starting.
///     2. Questions and permission prompts come first, then a running turn.
///     3. Between turns, an active goal or a live loop keeps the shot open.
///     4. Other background work counts as working. Else the shot is settled.
/// Note: A paused, blocked, or finished goal counts as settled, because it
///     waits on the user or is done.
pub(crate) fn classify_shot(agent: Option<&AgentView>) -> ShotStatus {
    let Some(agent) = agent else {
        return ShotStatus::Closed;
    };
    if agent
        .n_shot
        .as_ref()
        .is_some_and(|shot| shot.creation_error.is_some())
    {
        return ShotStatus::Failed;
    }
    if agent.session.session_id.is_none() {
        return ShotStatus::Starting;
    }
    let row_state = classify_top_level(agent);
    if row_state == RowState::NeedsInput {
        return ShotStatus::NeedsInput;
    }
    let turn_running = !agent.session.state.is_idle()
        || agent.session.turn_activity().is_some()
        || !agent.session.pending_prompts.is_empty()
        || agent.session.loading_replay
        || agent.pending_first_prompt.is_some()
        || agent.n_shot_start.is_some();
    if turn_running {
        return ShotStatus::Working;
    }
    if agent
        .goal_state
        .as_ref()
        .is_some_and(|g| g.status == GoalDisplayStatus::Active)
    {
        return ShotStatus::Goal;
    }
    let loops = agent.session.scheduled_tasks.len();
    if loops > 0 {
        return ShotStatus::Looping { count: loops };
    }
    if row_state == RowState::Working {
        return ShotStatus::Working;
    }
    ShotStatus::Settled
}

/// Return the next free group label for `parent`: `n-shot-1`, `n-shot-2`, ...
/// Requests that have not started yet already hold their label.
pub(crate) fn next_group_label(
    groups: &[NShotGroup],
    requests: &[NShotRequest],
    parent: AgentId,
) -> String {
    let taken = groups.iter().filter(|g| g.parent == parent).count()
        + requests.iter().filter(|r| r.parent == parent).count();
    format!("n-shot-{}", taken + 1)
}

/// One shot, as the record, the report, and the widget show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShotSummary {
    pub label: String,
    pub index: usize,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub status: ShotStatus,
    pub final_message: Option<String>,
    pub creation_error: Option<String>,
}

pub(crate) fn shot_label(index: usize) -> String {
    format!("shot-{index}")
}

/// Purpose: Make the `shots.json` record of a group.
/// Input: The group, the parent session id, the report path, and the shots.
/// Output: The record as JSON.
pub(crate) fn build_record(
    group: &NShotGroup,
    parent_session_id: &str,
    report_path: &Path,
    shots: &[ShotSummary],
) -> serde_json::Value {
    let shots: Vec<serde_json::Value> = shots
        .iter()
        .map(|shot| {
            serde_json::json!({
                "index": shot.index,
                "label": shot.label,
                "session_id": shot.session_id,
                "worktree": shot.worktree,
                "status": shot.status.label(),
                "creation_error": shot.creation_error,
            })
        })
        .collect();
    serde_json::json!({
        "label": group.label,
        "objective": group.prompt,
        "parent_session_id": parent_session_id,
        "joined": group.joined,
        "elapsed_ms": group.settled_after.map(|d| d.as_millis() as u64),
        "report": report_path,
        "shots": shots,
    })
}

/// Purpose: Read a group back from its `shots.json` record.
/// Input: The record that `build_record` wrote.
/// Output: The group, or `None` when the record has no label or a shot has
///     no session id without a creation failure.
pub(crate) fn parse_record(record: &serde_json::Value) -> Option<StoredGroup> {
    let mut shots = Vec::new();
    for (i, shot) in record["shots"].as_array()?.iter().enumerate() {
        let status = match shot["status"].as_str() {
            Some("settled") => ShotStatus::Settled,
            Some("closed") => ShotStatus::Closed,
            Some("failed") => ShotStatus::Failed,
            _ => ShotStatus::Working,
        };
        let session_id = shot["session_id"].as_str().map(String::from);
        if session_id.is_none() && status != ShotStatus::Failed {
            return None;
        }
        shots.push(StoredShot {
            label: shot["label"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| shot_label(i + 1)),
            session_id,
            creation_error: shot["creation_error"].as_str().map(String::from),
            worktree: shot["worktree"].as_str().map(std::path::PathBuf::from),
            status,
        });
    }
    Some(StoredGroup {
        label: record["label"].as_str()?.to_string(),
        prompt: record["objective"].as_str().unwrap_or_default().to_string(),
        joined: record["joined"].as_bool().unwrap_or(false),
        elapsed: record["elapsed_ms"]
            .as_u64()
            .map(std::time::Duration::from_millis),
        shots,
    })
}

/// Purpose: Make the comparison report that the parent reads at the join.
/// Input: The group and the shots.
/// Output: The report as Markdown.
pub(crate) fn build_report(group: &NShotGroup, shots: &[ShotSummary]) -> String {
    let mut report = format!(
        "# {}: {} shots\n\nPrompt:\n\n{}\n\nEach shot is a full session forked from \
         the parent, working in its own git worktree. Inspect a shot with git in its \
         worktree, for example `git -C <worktree> status` and `git -C <worktree> diff HEAD`. \
         A shot may also have committed, so check `git -C <worktree> log` too.\n\n",
        group.label,
        shots.len(),
        group.prompt
    );
    for shot in shots {
        report.push_str(&format!(
            "## {}\n\n- Status: {}\n- Session: {}\n- Worktree: {}\n{}\n### Final message\n\n{}\n\n",
            shot.label,
            shot.status.label(),
            shot.session_id.as_deref().unwrap_or("(none)"),
            shot.worktree.as_deref().unwrap_or("(none)"),
            shot.creation_error
                .as_ref()
                .map(|error| format!("- Creation error: {error}\n"))
                .unwrap_or_default(),
            shot.final_message.as_deref().unwrap_or("(no message)"),
        ));
    }
    report
}

/// Purpose: Make the prompt that asks the parent to compare the shots.
/// Input: The group, the number of shots, and the report path.
/// Output: The prompt text.
pub(crate) fn join_prompt(group: &NShotGroup, report_path: &Path) -> String {
    format!(
        "{label}: all {n} shots settled. Read the report at {path}, then compare the \
         shots for me: the strengths, weaknesses, and risks of each, and your \
         recommendation. Do not change this working tree until I pick a shot or a \
         combination. After you apply my choice, call the n_shot tool with only \
         `resolve` (report_path {path}, the outcome, the shot labels used, and a \
         rationale) to record it.",
        label = group.label,
        n = group.shots.len(),
        path = report_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group() -> NShotGroup {
        NShotGroup {
            label: "n-shot-1".into(),
            parent: AgentId(0),
            prompt: "/goal fix it".into(),
            shots: vec![AgentId(1), AgentId(2)],
            shot_labels: vec![shot_label(1), shot_label(2)],
            pending_write: None,
            write_retry_at: None,
            joined: false,
            last_statuses: vec![],
            started_at: std::time::Instant::now(),
            settled_after: None,
            stored: vec![],
        }
    }

    fn shot(index: usize, status: ShotStatus) -> ShotSummary {
        ShotSummary {
            label: shot_label(index),
            index,
            session_id: Some(format!("s{index}")),
            worktree: Some(format!("/wt/{index}")),
            status,
            final_message: Some("done".into()),
            creation_error: None,
        }
    }

    #[test]
    fn settled_means_nothing_left_to_run() {
        assert!(ShotStatus::Settled.is_settled());
        assert!(ShotStatus::Closed.is_settled());
        assert!(ShotStatus::Failed.is_settled());
        for open in [
            ShotStatus::Starting,
            ShotStatus::Working,
            ShotStatus::NeedsInput,
            ShotStatus::Goal,
            ShotStatus::Looping { count: 1 },
        ] {
            assert!(!open.is_settled(), "{open:?}");
        }
    }

    #[test]
    fn record_uses_the_labels_that_resolve_checks() {
        let shots = [shot(1, ShotStatus::Settled), shot(2, ShotStatus::Goal)];
        let record = build_record(&group(), "parent", Path::new("/r/report.md"), &shots);
        assert_eq!(record["shots"][0]["label"], "shot-1");
        assert_eq!(record["shots"][1]["status"], "goal active");
        assert_eq!(record["parent_session_id"], "parent");
        let report = build_report(&group(), &shots);
        assert!(report.contains("## shot-2"));
        assert!(report.contains("/wt/1"));
        assert!(join_prompt(&group(), Path::new("/r/report.md")).contains("/r/report.md"));
    }

    #[test]
    fn record_reads_back_after_a_resume() {
        let mut joined = group();
        joined.joined = true;
        joined.settled_after = Some(std::time::Duration::from_secs(90));
        let shots = [shot(1, ShotStatus::Settled), shot(2, ShotStatus::Closed)];
        let record = build_record(&joined, "parent", Path::new("/r/report.md"), &shots);
        let stored = parse_record(&record).unwrap();
        assert_eq!(stored.label, "n-shot-1");
        assert_eq!(stored.prompt, "/goal fix it");
        assert!(stored.joined);
        assert_eq!(stored.elapsed, Some(std::time::Duration::from_secs(90)));
        assert_eq!(
            stored.shots[1],
            StoredShot {
                label: "shot-2".into(),
                session_id: Some("s2".into()),
                creation_error: None,
                worktree: Some("/wt/2".into()),
                status: ShotStatus::Closed,
            }
        );
        let mut no_session = record.clone();
        no_session["shots"][0]["session_id"] = serde_json::Value::Null;
        assert_eq!(parse_record(&no_session), None);
    }

    #[test]
    fn custom_labels_survive_request_report_record_and_resume() {
        let specs = parse_request_shots(&serde_json::json!({
            "prompt": "fix it",
            "shots": [{ "label": "minimal-fix" }, { "label": "rewrite" }, {}],
        }))
        .unwrap();
        assert_eq!(specs[0].label.as_deref(), Some("minimal-fix"));
        assert_eq!(specs[1].label.as_deref(), Some("rewrite"));
        assert_eq!(specs[2].label, None);
        let mut shots = vec![shot(1, ShotStatus::Settled), shot(2, ShotStatus::Closed)];
        shots[0].label = "minimal-fix".into();
        shots[1].label = "rewrite".into();
        let mut record = build_record(&group(), "parent", Path::new("/r/report.md"), &shots);
        assert_eq!(record["shots"][0]["label"], "minimal-fix");
        let stored = parse_record(&record).unwrap();
        assert_eq!(stored.shots[0].label, "minimal-fix");
        assert_eq!(stored.shots[1].label, "rewrite");
        let report = build_report(&group(), &shots);
        assert!(report.contains("## minimal-fix"));
        assert!(report.contains("## rewrite"));
        record["shots"][0].as_object_mut().unwrap().remove("label");
        assert_eq!(parse_record(&record).unwrap().shots[0].label, "shot-1");
    }

    #[test]
    fn group_labels_count_per_parent() {
        let mut groups = vec![group()];
        assert_eq!(next_group_label(&groups, &[], AgentId(0)), "n-shot-2");
        assert_eq!(next_group_label(&groups, &[], AgentId(5)), "n-shot-1");
        let pending = [NShotRequest {
            parent: AgentId(0),
            label: "n-shot-2".into(),
            shots: vec![],
        }];
        assert_eq!(next_group_label(&groups, &pending, AgentId(0)), "n-shot-3");
        groups.clear();
        assert_eq!(next_group_label(&groups, &[], AgentId(0)), "n-shot-1");
    }

    #[test]
    fn validates_shot_count_before_allocation() {
        for n in [0, 11, u64::MAX] {
            let result = parse_request_shots(&serde_json::json!({ "prompt": "fix it", "n": n }));
            assert!(result.is_err(), "invalid count {n}");
        }
        for n in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("10"),
        ] {
            assert!(
                parse_request_shots(&serde_json::json!({ "prompt": "fix it", "n": n })).is_err()
            );
        }
        for n in [1, 10] {
            assert_eq!(
                parse_request_shots(&serde_json::json!({ "prompt": "fix it", "n": n }))
                    .unwrap()
                    .len(),
                n
            );
        }
        let too_many = vec![serde_json::json!({ "prompt": "fix it" }); 11];
        assert!(parse_request_shots(&serde_json::json!({ "shots": too_many })).is_err());
        // A supplied shot list still overrides an otherwise invalid count.
        assert_eq!(
            parse_request_shots(
                &serde_json::json!({ "n": u64::MAX, "shots": [{ "prompt": "fix it" }] })
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn failed_creation_records_do_not_require_a_session_id() {
        let mut failed = shot(1, ShotStatus::Failed);
        failed.session_id = None;
        failed.worktree = None;
        failed.final_message = None;
        failed.creation_error = Some("disk full".into());
        let record = build_record(
            &group(),
            "parent",
            Path::new("/r/report.md"),
            &[failed.clone()],
        );
        let stored = parse_record(&record).unwrap();
        assert_eq!(stored.shots[0].session_id, None);
        assert_eq!(stored.shots[0].status, ShotStatus::Failed);
        assert_eq!(stored.shots[0].creation_error.as_deref(), Some("disk full"));
        assert!(build_report(&group(), &[failed]).contains("Creation error: disk full"));
    }

    #[test]
    fn parses_identical_and_per_shot_requests() {
        let same = parse_request_shots(&serde_json::json!({ "prompt": "fix it", "n": 3 }));
        assert_eq!(same.unwrap().len(), 3);
        let varied = parse_request_shots(&serde_json::json!({
            "prompt": "fix it",
            "shots": [{ "model": "m1" }, { "prompt": "rewrite it" }],
        }))
        .unwrap();
        assert_eq!(
            varied[0],
            ShotSpec {
                label: None,
                prompt: "fix it".into(),
                model: Some("m1".into())
            }
        );
        assert_eq!(
            varied[1],
            ShotSpec {
                label: None,
                prompt: "rewrite it".into(),
                model: None
            }
        );
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "shots": [{ "model": "m1" }] }),
            serde_json::json!({ "prompt": "x", "n": 11 }),
            serde_json::json!({ "prompt": "x", "shots": [] }),
        ] {
            assert!(parse_request_shots(&bad).is_err(), "{bad}");
        }
    }
}
