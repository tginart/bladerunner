//! `/n-shot N <prompt>` in the pager: fork the active session into N shots.
//!
//! Each shot is a `/fork --worktree` session that starts with the prompt, so
//! slash commands in the prompt (`/goal`, `/loop`, skills) run in every shot.
//! The view stays on the parent, which tracks the group.

use super::fork::{ForkFocus, spawn_fork};
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::app_view::AppView;
use crate::app::n_shot::{
    NShotColumnsState, NShotGroup, NShotMember, ShotStatus, ShotSummary, build_record,
    build_report, classify_shot, join_prompt, next_group_label, shot_label,
};
use crate::scrollback::block::RenderBlock;

/// Purpose: Start an n-shot group from the parent agent.
/// Input: The app, the parent agent, and the `/n-shot` args (`<N> <prompt>`).
/// Output: One start effect per shot, or no effects after a usage message.
/// Method:
///     1. Parse N and the prompt. Require a live session in a git repository.
///     2. Fork N worktree sessions with the prompt as their first prompt.
///     3. Mark each shot, record the group, and note the run on the parent.
pub(in crate::app::dispatch) fn dispatch_n_shot(
    app: &mut AppView,
    parent_id: AgentId,
    args: &str,
) -> Vec<Effect> {
    let (n, prompt) = match xai_grok_shell::session::parse_n_shot_args(args) {
        Ok(parsed) => parsed,
        Err(usage) => {
            push_parent_note(app, parent_id, usage);
            return vec![];
        }
    };
    let Some(parent) = app.agents.get(&parent_id) else {
        return vec![];
    };
    if parent.session.session_id.is_none() {
        app.show_toast("Cannot start /n-shot: session is still being created");
        return vec![];
    }
    let in_git_repo = parent.current_branch.is_some()
        || crate::git_info::cwd_git_info_lazy(&parent.session.cwd).is_some();
    if !in_git_repo {
        app.show_toast("/n-shot needs a git repository: each shot gets its own worktree");
        return vec![];
    }
    let label = next_group_label(&app.n_shot_groups, parent_id);
    let total = n as usize;
    let mut shots = Vec::with_capacity(total);
    let mut effects = Vec::with_capacity(total);
    for index in 1..=total {
        let Some((shot_id, effect)) = spawn_fork(
            app,
            parent_id,
            true,
            Some(prompt.clone()),
            ForkFocus::StayOnParent,
        ) else {
            break;
        };
        if let Some(shot) = app.agents.get_mut(&shot_id) {
            shot.n_shot = Some(NShotMember {
                label: label.clone(),
                index,
                total,
                parent: parent_id,
            });
        }
        shots.push(shot_id);
        effects.push(effect);
    }
    if shots.is_empty() {
        return vec![];
    }
    app.n_shot_columns = Some(NShotColumnsState {
        parent: Some(parent_id),
        label: label.clone(),
        focus: 0,
    });
    app.n_shot_groups.push(NShotGroup {
        label,
        parent: parent_id,
        prompt,
        shots,
        joined: false,
        last_statuses: Vec::new(),
        started_at: std::time::Instant::now(),
        settled_after: None,
    });
    effects
}

fn push_parent_note(app: &mut AppView, parent_id: AgentId, text: String) {
    if let Some(parent) = app.agents.get_mut(&parent_id) {
        parent.scrollback.push_block(RenderBlock::system(text));
    }
}

/// Purpose: Track every open n-shot group, once per poll.
/// Input: The app.
/// Output: Effects that write the group files, and the parent's compare turn.
/// Method:
///     1. Classify each shot of each group that has not joined yet.
///     2. When a status changed, write `shots.json` and each shot's back-link.
///     3. When every shot settled, write `report.md` and ask the parent to compare.
pub(crate) fn tick(app: &mut AppView) -> Vec<Effect> {
    let mut effects = Vec::new();
    for group_idx in 0..app.n_shot_groups.len() {
        let group = app.n_shot_groups[group_idx].clone();
        if group.joined {
            continue;
        }
        let Some(dir) = group_dir(app, &group) else {
            continue;
        };
        let shots = summarize_shots(app, &group);
        let statuses: Vec<ShotStatus> = shots.iter().map(|s| s.status).collect();
        let all_settled = statuses.iter().all(|s| s.is_settled());
        let all_closed = statuses.iter().all(|s| *s == ShotStatus::Closed);
        if statuses == group.last_statuses && !all_settled {
            continue;
        }
        let report_path = dir.join("report.md");
        let parent_session_id = parent_session_id(app, &group).unwrap_or_default();
        let mut joined = group.clone();
        joined.joined = all_settled;
        let mut files = vec![(
            dir.join("shots.json"),
            pretty(&build_record(
                &joined,
                &parent_session_id,
                &report_path,
                &shots,
            )),
        )];
        files.extend(back_links(app, &group, &parent_session_id, &dir));
        if all_settled && !all_closed {
            files.push((report_path.clone(), build_report(&group, &shots)));
        }
        effects.push(Effect::WriteNShotFiles { files });
        upsert_group_block(app, &group, &shots, all_settled);
        let state = &mut app.n_shot_groups[group_idx];
        state.last_statuses = statuses;
        state.joined = all_settled;
        if all_settled {
            state.settled_after = Some(state.started_at.elapsed());
        }
        if all_settled && !all_closed {
            effects.extend(ask_parent_to_compare(app, &group, &report_path));
        }
    }
    effects
}

/// True while some group still waits for its shots.
pub(crate) fn has_open_groups(app: &AppView) -> bool {
    app.n_shot_groups.iter().any(|g| !g.joined)
}

/// Purpose: Show the group as one live block in the parent's transcript.
/// Input: The group, its shots, and whether every shot settled.
/// Output: None. The block is created once and then changed in place.
fn upsert_group_block(app: &mut AppView, group: &NShotGroup, shots: &[ShotSummary], done: bool) {
    use crate::scrollback::blocks::{WorkflowBlock, WorkflowBlockPhase, WorkflowBlockStatus};

    let Some(parent) = app.agents.get_mut(&group.parent) else {
        return;
    };
    let key = format!("n-shot:{}", group.label);
    let entry_id = match parent
        .workflow_blocks
        .get(&key)
        .copied()
        .filter(|id| parent.scrollback.get_by_id(*id).is_some())
    {
        Some(id) => id,
        None => {
            let mut block = WorkflowBlock::started(&key, &group.label, &group.prompt);
            block.heading = "";
            let id = parent.scrollback.push_block(RenderBlock::Workflow(block));
            parent.workflow_blocks.insert(key.clone(), id);
            id
        }
    };
    let elapsed = group.elapsed();
    if let Some(entry) = parent.scrollback.get_by_id_mut(entry_id)
        && let RenderBlock::Workflow(ref mut block) = entry.block
    {
        block.status = if done {
            WorkflowBlockStatus::Done { elapsed }
        } else {
            WorkflowBlockStatus::Running
        };
        block.phases = shots
            .iter()
            .map(|shot| WorkflowBlockPhase {
                title: format!("{} {}", shot_label(shot.index), shot.status.label()),
                state: match shot.status {
                    ShotStatus::Settled => "done",
                    ShotStatus::Closed => "closed",
                    _ => "active",
                }
                .to_string(),
            })
            .collect();
        block.elapsed = elapsed;
        entry.invalidate_cache();
    }
    if done {
        parent.scrollback.finish_running(entry_id);
        parent.workflow_blocks.remove(&key);
    } else {
        parent.scrollback.set_entry_running(entry_id, true);
    }
}

fn summarize_shots(app: &AppView, group: &NShotGroup) -> Vec<ShotSummary> {
    group
        .shots
        .iter()
        .enumerate()
        .map(|(i, shot_id)| {
            let agent = app.agents.get(shot_id);
            ShotSummary {
                index: i + 1,
                session_id: agent
                    .and_then(|a| a.session.session_id.as_ref())
                    .map(|sid| sid.0.to_string()),
                worktree: agent
                    .filter(|a| a.session.is_worktree)
                    .map(|a| a.session.cwd.to_string_lossy().into_owned()),
                status: classify_shot(agent),
                final_message: agent.and_then(last_agent_message),
            }
        })
        .collect()
}

fn last_agent_message(agent: &crate::app::agent_view::AgentView) -> Option<String> {
    (0..agent.scrollback.len())
        .rev()
        .find_map(|i| match agent.scrollback.entry(i) {
            Some(entry) => match &entry.block {
                RenderBlock::AgentMessage(msg) => Some(msg.copy_text(false)),
                _ => None,
            },
            None => None,
        })
}

fn parent_session_id(app: &AppView, group: &NShotGroup) -> Option<String> {
    let parent = app.agents.get(&group.parent)?;
    parent
        .session
        .session_id
        .as_ref()
        .map(|sid| sid.0.to_string())
}

/// The group's directory: `<parent session dir>/n-shot/<label>/`.
fn group_dir(app: &AppView, group: &NShotGroup) -> Option<std::path::PathBuf> {
    let parent = app.agents.get(&group.parent)?;
    let session_id = parent.session.session_id.clone()?;
    let info = xai_grok_shell::session::info::Info {
        id: session_id,
        cwd: parent.session.cwd.to_string_lossy().to_string(),
    };
    Some(
        xai_grok_shell::session::persistence::session_dir(&info)
            .join("n-shot")
            .join(&group.label),
    )
}

/// `n_shot.json` in each shot's session dir, pointing back at the group.
fn back_links(
    app: &AppView,
    group: &NShotGroup,
    parent_session_id: &str,
    dir: &std::path::Path,
) -> Vec<(std::path::PathBuf, String)> {
    let mut files = Vec::new();
    for (i, shot_id) in group.shots.iter().enumerate() {
        let Some(shot) = app.agents.get(shot_id) else {
            continue;
        };
        let Some(session_id) = shot.session.session_id.clone() else {
            continue;
        };
        let info = xai_grok_shell::session::info::Info {
            id: session_id,
            cwd: shot.session.cwd.to_string_lossy().to_string(),
        };
        let link = serde_json::json!({
            "group": group.label,
            "shot": shot_label(i + 1),
            "index": i + 1,
            "total": group.shots.len(),
            "parent_session_id": parent_session_id,
            "record": dir.join("shots.json"),
        });
        files.push((
            xai_grok_shell::session::persistence::session_dir(&info).join("n_shot.json"),
            pretty(&link),
        ));
    }
    files
}

fn ask_parent_to_compare(
    app: &mut AppView,
    group: &NShotGroup,
    report_path: &std::path::Path,
) -> Vec<Effect> {
    let Some(parent) = app.agents.get_mut(&group.parent) else {
        return vec![];
    };
    parent
        .session
        .enqueue_prompt(join_prompt(group, report_path));
    crate::app::dispatch::queue::maybe_drain_queue_and_note_peek(app, group.parent)
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Open the columns of the active parent's latest group. Returns false when the
/// active agent has no session-shot group, so `/shots` can fall back to the
/// subagent n-shot view.
pub(in crate::app::dispatch) fn open_latest_columns(app: &mut AppView) -> bool {
    let crate::app::app_view::ActiveView::Agent(parent_id) = app.active_view else {
        return false;
    };
    let Some(group) = app
        .n_shot_groups
        .iter()
        .rev()
        .find(|g| g.parent == parent_id)
    else {
        return false;
    };
    app.n_shot_columns = Some(NShotColumnsState {
        parent: Some(parent_id),
        label: group.label.clone(),
        focus: 0,
    });
    true
}

/// Close the columns view and switch to its focused shot.
pub(in crate::app::dispatch) fn attach_focused_shot(app: &mut AppView) -> Vec<Effect> {
    let Some(columns) = app.n_shot_columns.take() else {
        return vec![];
    };
    let shot = app
        .n_shot_groups
        .iter()
        .find(|g| Some(g.parent) == columns.parent && g.label == columns.label)
        .and_then(|g| g.shots.get(columns.focus).copied());
    match shot {
        Some(shot_id) if app.agents.contains_key(&shot_id) => {
            crate::app::dispatch::ctx::switch_to_agent(
                app,
                shot_id,
                crate::app::dispatch::ctx::SwitchCause::Picker,
            );
        }
        _ => app.show_toast("That shot session is closed"),
    }
    vec![]
}
