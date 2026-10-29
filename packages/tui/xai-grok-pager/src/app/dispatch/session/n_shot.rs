//! `/n-shot N <prompt>` in the pager: fork the active session into N shots.
//!
//! Each shot is a `/fork --worktree` session that starts with the prompt, so
//! slash commands in the prompt (`/goal`, `/loop`, skills) run in every shot.
//! The view stays on the parent, which tracks the group.

use super::fork::{ForkFocus, spawn_fork};
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::ActiveView;
use crate::app::app_view::AppView;
use crate::app::n_shot::{
    NShotColumnsState, NShotGroup, NShotMember, PendingShotStart, ShotSpec, ShotStatus,
    ShotSummary, StoredGroup, build_record, build_report, classify_shot, join_prompt,
    next_group_label, parse_record, shot_label,
};
use crate::scrollback::block::RenderBlock;

/// Purpose: Start an n-shot group from a typed `/n-shot`.
/// Input: The app, the parent agent, and the `/n-shot` args (`<N> <prompt>`).
/// Output: One start effect per shot, or no effects after a usage message.
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
    let label = next_group_label(&app.n_shot_groups, &app.n_shot_requests, parent_id);
    let shots = vec![
        ShotSpec {
            prompt,
            model: None
        };
        n as usize
    ];
    start_group(app, parent_id, label, shots)
}

/// Purpose: Fork the parent into one worktree session per shot and record the group.
/// Input: The app, the parent agent, the group label, and the shots.
/// Output: One start effect per shot.
/// Method:
///     1. Fork each shot. A shot without a model gets its prompt as the first prompt.
///     2. A shot with a model holds its prompt until the tick sets the model.
///     3. Mark each shot, record the group, and open the columns view.
fn start_group(
    app: &mut AppView,
    parent_id: AgentId,
    label: String,
    shots: Vec<ShotSpec>,
) -> Vec<Effect> {
    let total = shots.len();
    let objective = shots.first().map(|s| s.prompt.clone()).unwrap_or_default();
    let mut shot_ids = Vec::with_capacity(total);
    let mut effects = Vec::with_capacity(total);
    for (i, spec) in shots.into_iter().enumerate() {
        let first_prompt = spec.model.is_none().then(|| spec.prompt.clone());
        let Some((shot_id, effect)) =
            spawn_fork(app, parent_id, true, first_prompt, ForkFocus::StayOnParent)
        else {
            break;
        };
        if let Some(shot) = app.agents.get_mut(&shot_id) {
            shot.n_shot = Some(NShotMember {
                label: label.clone(),
                index: i + 1,
                total,
                parent: parent_id,
            });
            shot.n_shot_start = spec.model.map(|model| PendingShotStart {
                prompt: spec.prompt,
                model: agent_client_protocol::ModelId::new(model),
                requested_at: None,
            });
        }
        shot_ids.push(shot_id);
        effects.push(effect);
    }
    if shot_ids.is_empty() {
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
        prompt: objective,
        shots: shot_ids,
        joined: false,
        last_statuses: Vec::new(),
        started_at: std::time::Instant::now(),
        settled_after: None,
        stored: Vec::new(),
    });
    effects
}

/// Start each accepted `n_shot` tool call once its parent's turn has ended.
fn start_ready_requests(app: &mut AppView) -> Vec<Effect> {
    let mut effects = Vec::new();
    let mut waiting = Vec::new();
    for request in std::mem::take(&mut app.n_shot_requests) {
        let Some(parent) = app.agents.get(&request.parent) else {
            continue;
        };
        let parent_idle = parent.session.state.is_idle()
            && parent.session.turn_activity().is_none()
            && parent.session.pending_prompts.is_empty();
        if parent_idle {
            effects.extend(start_group(
                app,
                request.parent,
                request.label,
                request.shots,
            ));
        } else {
            waiting.push(request);
        }
    }
    app.n_shot_requests.extend(waiting);
    effects
}

/// Purpose: Send the first prompt of shots that wait for their own model.
/// Input: The app.
/// Output: Model switch effects, then prompt effects once each model is set.
/// Note: If the model switch does not land in time, the prompt goes anyway,
///     so a wrong model id cannot hold a shot forever.
fn start_pending_shots(app: &mut AppView) -> Vec<Effect> {
    const MODEL_SWITCH_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
    let shot_ids: Vec<AgentId> = app
        .agents
        .iter()
        .filter(|(_, a)| a.n_shot_start.is_some())
        .map(|(id, _)| *id)
        .collect();
    let mut effects = Vec::new();
    for shot_id in shot_ids {
        let Some(shot) = app.agents.get_mut(&shot_id) else {
            continue;
        };
        let Some(session_id) = shot.session.session_id.clone() else {
            continue;
        };
        if shot.session.loading_replay {
            continue;
        }
        let Some(start) = shot.n_shot_start.as_mut() else {
            continue;
        };
        match start.requested_at {
            None => {
                start.requested_at = Some(std::time::Instant::now());
                effects.push(Effect::SwitchModel {
                    agent_id: shot_id,
                    session_id,
                    model_id: start.model.clone(),
                    effort: None,
                    prev_model_id: None,
                });
            }
            Some(at) => {
                let model_set = shot.session.models.current.as_ref() == Some(&start.model);
                if !model_set && at.elapsed() < MODEL_SWITCH_WAIT {
                    continue;
                }
                let prompt = start.prompt.clone();
                shot.n_shot_start = None;
                shot.session.enqueue_prompt(prompt);
                effects.extend(
                    crate::app::dispatch::queue::maybe_drain_queue_and_note_peek(app, shot_id),
                );
            }
        }
    }
    effects
}

fn push_parent_note(app: &mut AppView, parent_id: AgentId, text: String) {
    if let Some(parent) = app.agents.get_mut(&parent_id) {
        parent.scrollback.push_block(RenderBlock::system(text));
    }
}

/// Purpose: Track every open n-shot group, once per poll.
/// Input: The app.
/// Output: Effects that start shots, write the group files, and start the
///     parent's compare turn.
/// Method:
///     1. Start accepted tool calls whose parent is idle, and model-held shots.
///     2. Classify each shot of each group that has not joined yet.
///     3. When a status changed, write `shots.json` and each shot's back-link.
///     4. When every shot settled, write `report.md` and ask the parent to compare.
pub(crate) fn tick(app: &mut AppView) -> Vec<Effect> {
    let mut effects = start_ready_requests(app);
    effects.extend(start_pending_shots(app));
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
        if all_settled {
            joined.settled_after = Some(group.started_at.elapsed());
        }
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
    !app.n_shot_requests.is_empty() || app.n_shot_groups.iter().any(|g| !g.joined)
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
    Some(n_shot_root(app.agents.get(&group.parent)?)?.join(&group.label))
}

/// The parent's `n-shot/` directory, which holds one directory per group.
fn n_shot_root(parent: &AgentView) -> Option<std::path::PathBuf> {
    let session_id = parent.session.session_id.clone()?;
    let info = xai_grok_shell::session::info::Info {
        id: session_id,
        cwd: parent.session.cwd.to_string_lossy().to_string(),
    };
    Some(xai_grok_shell::session::persistence::session_dir(&info).join("n-shot"))
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

/// Open the columns of the active parent's group `label`, if it has one.
/// Output: Load effects for shots that were stored but not loaded yet.
pub(in crate::app::dispatch) fn open_columns(app: &mut AppView, label: &str) -> Vec<Effect> {
    let ActiveView::Agent(parent_id) = app.active_view else {
        return vec![];
    };
    if !app
        .n_shot_groups
        .iter()
        .any(|g| g.parent == parent_id && g.label == label)
    {
        return vec![];
    }
    app.n_shot_columns = Some(NShotColumnsState {
        parent: Some(parent_id),
        label: label.to_string(),
        focus: 0,
    });
    load_stored_shots(app, parent_id, label)
}

/// Open the columns of the active parent's latest group. Returns `None` when
/// the active agent has no session-shot group, so `/shots` can fall back to
/// the subagent n-shot view.
pub(in crate::app::dispatch) fn open_latest_columns(app: &mut AppView) -> Option<Vec<Effect>> {
    let ActiveView::Agent(parent_id) = app.active_view else {
        return None;
    };
    let label = app
        .n_shot_groups
        .iter()
        .rev()
        .find(|g| g.parent == parent_id)?
        .label
        .clone();
    Some(open_columns(app, &label))
}

/// Close the columns view and switch to its focused shot.
pub(in crate::app::dispatch) fn attach_focused_shot(app: &mut AppView) -> Vec<Effect> {
    let Some(columns) = app.n_shot_columns.take() else {
        return vec![];
    };
    let effects = match columns.parent {
        Some(parent_id) => load_stored_shots(app, parent_id, &columns.label),
        None => vec![],
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
    effects
}

/// Purpose: Rebuild the n-shot groups of a resumed parent from its `shots.json` files.
/// Input: The app and the agent whose session loaded.
/// Output: Load effects for the shots of groups that had not joined yet.
/// Method:
///     1. Skip shot sessions, and groups that are already in memory.
///     2. Read each `n-shot/<label>/shots.json` in the parent's session dir.
///     3. Record each group with its stored shots and show its block again.
///     4. Load the shots of a group that had not joined, so the tick can join it.
/// Note: The shots of a joined group load when the user opens its columns.
pub(in crate::app::dispatch) fn restore_groups(
    app: &mut AppView,
    parent_id: AgentId,
) -> Vec<Effect> {
    let Some(parent) = app.agents.get(&parent_id) else {
        return vec![];
    };
    if parent.n_shot.is_some() {
        return vec![];
    }
    let Some(root) = n_shot_root(parent) else {
        return vec![];
    };
    let mut effects = Vec::new();
    for stored in read_stored_groups(&root) {
        let is_known = app
            .n_shot_groups
            .iter()
            .any(|g| g.parent == parent_id && g.label == stored.label);
        if is_known {
            continue;
        }
        let label = stored.label.clone();
        let group = restored_group(app, parent_id, stored);
        upsert_group_block(app, &group, &stored_summaries(&group), group.joined);
        let is_joined = group.joined;
        app.n_shot_groups.push(group);
        if !is_joined {
            effects.extend(load_stored_shots(app, parent_id, &label));
        }
    }
    effects
}

/// Read every group record under `root`, in label order.
fn read_stored_groups(root: &std::path::Path) -> Vec<StoredGroup> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            tracing::warn!(dir = %root.display(), error = %e, "cannot read n-shot groups");
            return vec![];
        }
    };
    let mut groups = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path().join("shots.json");
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let record = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        match record.as_ref().and_then(parse_record) {
            Some(group) => groups.push(group),
            None => tracing::warn!(path = %path.display(), "invalid n-shot record"),
        }
    }
    groups.sort_by_key(|g| {
        let number = g.label.trim_start_matches("n-shot-").parse::<usize>();
        (number.unwrap_or(usize::MAX), g.label.clone())
    });
    groups
}

/// Make the in-memory group of a stored record. Each shot gets a fresh agent
/// id that has no agent yet, so it shows as closed until it loads.
fn restored_group(app: &mut AppView, parent_id: AgentId, stored: StoredGroup) -> NShotGroup {
    let shots = stored
        .shots
        .iter()
        .map(|_| {
            let id = AgentId(app.next_agent_id);
            app.next_agent_id += 1;
            id
        })
        .collect();
    let elapsed = stored.elapsed.unwrap_or_default();
    NShotGroup {
        label: stored.label,
        parent: parent_id,
        prompt: stored.prompt,
        shots,
        joined: stored.joined,
        last_statuses: stored.shots.iter().map(|s| s.status).collect(),
        started_at: std::time::Instant::now()
            .checked_sub(elapsed)
            .unwrap_or_else(std::time::Instant::now),
        settled_after: stored.joined.then_some(elapsed),
        stored: stored.shots,
    }
}

fn stored_summaries(group: &NShotGroup) -> Vec<ShotSummary> {
    group
        .stored
        .iter()
        .enumerate()
        .map(|(i, shot)| ShotSummary {
            index: i + 1,
            session_id: Some(shot.session_id.clone()),
            worktree: shot
                .worktree
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            status: shot.status,
            final_message: None,
        })
        .collect()
}

/// Purpose: Load the stored shot sessions of a group that are not open yet.
/// Input: The app, the parent agent, and the group label.
/// Output: The load effects.
/// Note: A load switches the view to the shot, so the old view comes back after.
fn load_stored_shots(app: &mut AppView, parent_id: AgentId, label: &str) -> Vec<Effect> {
    let Some(group_idx) = app
        .n_shot_groups
        .iter()
        .position(|g| g.parent == parent_id && g.label == label)
    else {
        return vec![];
    };
    let previous_view = app.active_view;
    let total = app.n_shot_groups[group_idx].shots.len();
    let mut effects = Vec::new();
    for i in 0..total {
        let group = &app.n_shot_groups[group_idx];
        if app.agents.contains_key(&group.shots[i]) {
            continue;
        }
        let Some(stored) = group.stored.get(i).cloned() else {
            continue;
        };
        if find_agent_by_session(app, &stored.session_id).is_none() {
            effects.extend(super::load::dispatch_load_session(
                app,
                stored.session_id.clone(),
                stored.worktree.clone(),
                false,
            ));
        }
        let Some(shot_id) = find_agent_by_session(app, &stored.session_id) else {
            continue;
        };
        if let Some(shot) = app.agents.get_mut(&shot_id) {
            shot.n_shot = Some(NShotMember {
                label: label.to_string(),
                index: i + 1,
                total,
                parent: parent_id,
            });
        }
        app.n_shot_groups[group_idx].shots[i] = shot_id;
    }
    if app.active_view != previous_view {
        match previous_view {
            ActiveView::Agent(id) => crate::app::dispatch::ctx::switch_to_agent(
                app,
                id,
                crate::app::dispatch::ctx::SwitchCause::Picker,
            ),
            other => app.active_view = other,
        }
    }
    effects
}

fn find_agent_by_session(app: &AppView, session_id: &str) -> Option<AgentId> {
    app.agents.iter().find_map(|(id, agent)| {
        let sid = agent.session.session_id.as_ref()?;
        (*sid.0 == *session_id).then_some(*id)
    })
}
