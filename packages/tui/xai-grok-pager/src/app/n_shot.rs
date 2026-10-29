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
    /// True after the parent was asked to compare the shots.
    pub joined: bool,
    /// Shot statuses at the last tick, to write the record only on change.
    pub last_statuses: Vec<ShotStatus>,
    pub started_at: std::time::Instant,
    /// Time from launch until every shot settled. Set at the join.
    pub settled_after: Option<std::time::Duration>,
}

impl NShotGroup {
    /// Run time: until the join once settled, else until now.
    pub fn elapsed(&self) -> std::time::Duration {
        self.settled_after
            .unwrap_or_else(|| self.started_at.elapsed())
    }
}

/// Marks an agent as shot `index` of `total` in the group `label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NShotMember {
    pub label: String,
    pub index: usize,
    pub total: usize,
    pub parent: AgentId,
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
}

impl ShotStatus {
    /// True when the shot needs nothing more before the parent compares.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Settled | Self::Closed)
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
        }
    }
}

/// Purpose: Classify one shot for the widget and for the join.
/// Input: The shot agent, or `None` when the agent was closed.
/// Output: The shot status.
/// Method:
///     1. A missing agent is closed. An agent with no session is still starting.
///     2. Questions and permission prompts come first, then a running turn.
///     3. Between turns, an active goal or a live loop keeps the shot open.
///     4. Other background work counts as working. Else the shot is settled.
/// Note: A paused, blocked, or finished goal counts as settled, because it
///     waits on the user or is done.
pub(crate) fn classify_shot(agent: Option<&AgentView>) -> ShotStatus {
    let Some(agent) = agent else {
        return ShotStatus::Closed;
    };
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
        || agent.pending_first_prompt.is_some();
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
pub(crate) fn next_group_label(groups: &[NShotGroup], parent: AgentId) -> String {
    let taken = groups.iter().filter(|g| g.parent == parent).count();
    format!("n-shot-{}", taken + 1)
}

/// One shot, as the record, the report, and the widget show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShotSummary {
    pub index: usize,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub status: ShotStatus,
    pub final_message: Option<String>,
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
                "label": shot_label(shot.index),
                "session_id": shot.session_id,
                "worktree": shot.worktree,
                "status": shot.status.label(),
            })
        })
        .collect();
    serde_json::json!({
        "label": group.label,
        "objective": group.prompt,
        "parent_session_id": parent_session_id,
        "joined": group.joined,
        "report": report_path,
        "shots": shots,
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
            "## {}\n\n- Status: {}\n- Session: {}\n- Worktree: {}\n\n### Final message\n\n{}\n\n",
            shot_label(shot.index),
            shot.status.label(),
            shot.session_id.as_deref().unwrap_or("(none)"),
            shot.worktree.as_deref().unwrap_or("(none)"),
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
            joined: false,
            last_statuses: vec![],
            started_at: std::time::Instant::now(),
            settled_after: None,
        }
    }

    fn shot(index: usize, status: ShotStatus) -> ShotSummary {
        ShotSummary {
            index,
            session_id: Some(format!("s{index}")),
            worktree: Some(format!("/wt/{index}")),
            status,
            final_message: Some("done".into()),
        }
    }

    #[test]
    fn settled_means_nothing_left_to_run() {
        assert!(ShotStatus::Settled.is_settled());
        assert!(ShotStatus::Closed.is_settled());
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
    fn group_labels_count_per_parent() {
        let mut groups = vec![group()];
        assert_eq!(next_group_label(&groups, AgentId(0)), "n-shot-2");
        assert_eq!(next_group_label(&groups, AgentId(5)), "n-shot-1");
        groups.clear();
        assert_eq!(next_group_label(&groups, AgentId(0)), "n-shot-1");
    }
}
