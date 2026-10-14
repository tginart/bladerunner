//! Snapshot live work for the post-compaction reminder.
//!
//! Scheduler tasks and workflow runs are read independently, then converted into
//! owned summaries. Completed one-shots, expired loops, and terminal workflows
//! are excluded so shortened conversation history preserves only unfinished work.
use super::super::SessionActor;
use crate::session::helpers::compaction_context::{ScheduledLoopSummary, WorkflowRunSummary};
use xai_grok_tools::implementations::grok_build::scheduler::interval::interval_to_human;

pub(in crate::session) struct ScheduledCompactionWork {
    pub(in crate::session) scheduled_loops: Vec<ScheduledLoopSummary>,
    pub(in crate::session) workflows: Vec<WorkflowRunSummary>,
    pub(in crate::session) workflow_tool_name: Option<String>,
}

impl SessionActor {
    /// Read the scheduler and workflow tracker before building a compaction reminder.
    ///
    /// Main callers: SessionActor's conversation compaction path.
    /// Why this exists: unfinished work must survive removal of older tool messages.
    pub(in crate::session) async fn snapshot_scheduled_compaction_work(
        &self,
    ) -> ScheduledCompactionWork {
        let scheduled_loops = {
            let now = chrono::Utc::now();
            let bridge = self.agent.borrow().tool_bridge().clone();
            bridge
                .list_scheduled_tasks()
                .await
                .into_iter()
                .filter(|t| t.pending_fire_at(now).is_some())
                .map(|t| {
                    let next_fire_at = format_loop_next_fire(t.next_fire_at(), now);
                    ScheduledLoopSummary {
                        task_id: t.id,
                        interval: interval_to_human(t.interval_secs),
                        next_fire_at,
                        prompt: t.prompt,
                        recurring: t.recurring,
                        durable: t.durable,
                        foreground: t.foreground,
                    }
                })
                .collect()
        };
        let workflows = {
            let tracker = self.workflow_tracker().await;
            let guard = tracker.lock();
            guard
                .list()
                .into_iter()
                .filter(|r| !r.status.is_terminal())
                .map(|r| WorkflowRunSummary {
                    elapsed_ms: guard.elapsed_ms(&r.run_id),
                    name: r.name,
                    run_id: r.run_id,
                    status: r.status.as_str().to_string(),
                    objective: r.objective,
                    current_phase: r.current_phase,
                    agents_used: r.agents_used,
                    agent_budget: r.agent_budget,
                })
                .collect::<Vec<_>>()
        };
        let workflow_tool_name = if workflows.is_empty() {
            None
        } else {
            use xai_grok_tools::types::tool::ToolKind;
            self.agent
                .borrow()
                .tool_bridge()
                .tool_for_kind(ToolKind::Workflow)
                .await
                .filter(|s| !s.is_empty())
        };
        ScheduledCompactionWork {
            scheduled_loops,
            workflows,
            workflow_tool_name,
        }
    }
}

/// Human-readable "next fire" for a scheduled loop in the compaction reminder.
fn format_loop_next_fire(
    next: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = next.signed_duration_since(now).num_seconds();
    if secs <= 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("in {secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("in {mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("in {hours}h");
    }
    next.format("%d %b %H:%M UTC").to_string()
}
