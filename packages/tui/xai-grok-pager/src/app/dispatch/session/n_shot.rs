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
    ShotSummary, StoredGroup, StoredShot, build_record, build_report, classify_shot, join_prompt,
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
    // Only the shell's built-in advertises the background-workflow gate.
    if !parent
        .session
        .available_commands
        .iter()
        .any(|command| command.name == "n-shot" && command.meta.is_none())
    {
        app.show_toast(
            "Cannot start /n-shot: background workflows are unavailable for this session",
        );
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
            label: None,
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
    let mut shot_labels = Vec::with_capacity(total);
    let mut effects = Vec::with_capacity(total);
    for (i, spec) in shots.into_iter().enumerate() {
        let first_prompt = spec.model.is_none().then(|| spec.prompt.clone());
        let Some((shot_id, effect)) =
            spawn_fork(app, parent_id, true, first_prompt, ForkFocus::StayOnParent)
        else {
            break;
        };
        let shot_name = spec.label.unwrap_or_else(|| shot_label(i + 1));
        if let Some(shot) = app.agents.get_mut(&shot_id) {
            shot.n_shot = Some(NShotMember {
                shot_label: shot_name.clone(),
                creation_error: None,
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
        shot_labels.push(shot_name);
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
        stored: vec![None; shot_ids.len()],
        shots: shot_ids,
        shot_labels,
        pending_write: None,
        write_retry_at: None,
        joined: false,
        last_statuses: Vec::new(),
        started_at: std::time::Instant::now(),
        settled_after: None,
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
///     4. When every shot settled, write `report.md`. Compare only after the write succeeds.
pub(crate) fn tick(app: &mut AppView) -> Vec<Effect> {
    let mut effects = start_ready_requests(app);
    effects.extend(start_pending_shots(app));
    for group_idx in 0..app.n_shot_groups.len() {
        remember_shots(app, group_idx);
        let group = app.n_shot_groups[group_idx].clone();
        if group.joined
            || group.pending_write.is_some()
            || group
                .write_retry_at
                .is_some_and(|at| at > std::time::Instant::now())
        {
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
        let record = (
            dir.join("shots.json"),
            pretty(&build_record(
                &joined,
                &parent_session_id,
                &report_path,
                &shots,
            )),
        );
        let mut files = back_links(app, &group, &parent_session_id, &dir);
        if all_settled && !all_closed {
            files.push((report_path.clone(), build_report(&group, &shots)));
        }
        // Commit the joined record only after the report exists.
        files.push(record);
        effects.push(Effect::WriteNShotFiles {
            parent: group.parent,
            label: group.label.clone(),
            files,
        });
        upsert_group_block(app, &group, &shots, false);
        let state = &mut app.n_shot_groups[group_idx];
        state.pending_write = Some(shots);
        state.write_retry_at = None;
        state.settled_after = joined.settled_after;
    }
    effects
}

/// Purpose: Complete a group write before the parent can compare its report.
/// Input: The app, the group identity, and the file writer result.
/// Output: Compare effects on success. A failed write remains open for a retry.
pub(in crate::app::dispatch) fn files_written(
    app: &mut AppView,
    parent: AgentId,
    label: &str,
    result: Result<(), String>,
) -> Vec<Effect> {
    let Some(state) = app
        .n_shot_groups
        .iter_mut()
        .find(|g| g.parent == parent && g.label == label)
    else {
        return vec![];
    };
    let Some(shots) = state.pending_write.take() else {
        return vec![];
    };
    if let Err(error) = result {
        state.write_retry_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
        tracing::warn!(%error, %label, "failed to write n-shot files; will retry");
        push_parent_note(
            app,
            parent,
            format!("Cannot save {label}: {error}. Will retry before comparison."),
        );
        return vec![];
    }
    state.last_statuses = shots.iter().map(|s| s.status).collect();
    state.joined = shots.iter().all(|s| s.status.is_settled());
    let group = state.clone();
    upsert_group_block(app, &group, &shots, group.joined);
    if group.joined
        && shots.iter().any(|s| s.status != ShotStatus::Closed)
        && let Some(dir) = group_dir(app, &group)
    {
        return ask_parent_to_compare(app, &group, &dir.join("report.md"));
    }
    vec![]
}

/// Purpose: Retain each live shot's identity after its view closes.
/// Input: The app and the group index.
/// Output: The group stores the session id and worktree of each live shot.
fn remember_shots(app: &mut AppView, group_idx: usize) {
    let group = &mut app.n_shot_groups[group_idx];
    for (i, shot_id) in group.shots.iter().enumerate() {
        let Some(agent) = app.agents.get(shot_id) else {
            continue;
        };
        let session_id = agent.session.session_id.as_ref();
        let status = classify_shot(Some(agent));
        if session_id.is_none() && status != ShotStatus::Failed {
            continue;
        }
        group.stored[i] = Some(StoredShot {
            label: group.shot_labels[i].clone(),
            session_id: session_id.map(|id| id.0.to_string()),
            creation_error: agent
                .n_shot
                .as_ref()
                .and_then(|shot| shot.creation_error.clone()),
            worktree: (session_id.is_some() && agent.session.is_worktree)
                .then(|| agent.session.cwd.clone()),
            status,
        });
    }
}

/// Capture shot identities before the shared close helper removes a view.
pub(in crate::app::dispatch) fn remember_closed_shot(app: &mut AppView, shot_id: AgentId) {
    for i in 0..app.n_shot_groups.len() {
        if app.n_shot_groups[i].shots.contains(&shot_id) {
            remember_shots(app, i);
        }
    }
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
                title: format!("{} {}", shot.label, shot.status.label()),
                state: match shot.status {
                    ShotStatus::Settled => "done",
                    ShotStatus::Closed => "closed",
                    ShotStatus::Failed => "failed",
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
            let stored = group.stored.get(i).and_then(Option::as_ref);
            ShotSummary {
                label: group.shot_labels[i].clone(),
                index: i + 1,
                session_id: agent
                    .and_then(|a| a.session.session_id.as_ref())
                    .map(|sid| sid.0.to_string())
                    .or_else(|| stored.and_then(|s| s.session_id.clone())),
                worktree: agent
                    .filter(|a| a.session.is_worktree && a.session.session_id.is_some())
                    .map(|a| a.session.cwd.to_string_lossy().into_owned())
                    .or_else(|| {
                        stored
                            .and_then(|s| s.worktree.as_ref())
                            .map(|p| p.to_string_lossy().into_owned())
                    }),
                status: match agent {
                    Some(agent) => classify_shot(Some(agent)),
                    None if stored.is_some_and(|s| s.status == ShotStatus::Failed) => {
                        ShotStatus::Failed
                    }
                    None => ShotStatus::Closed,
                },
                final_message: agent.and_then(last_agent_message),
                creation_error: agent
                    .and_then(|a| a.n_shot.as_ref())
                    .and_then(|shot| shot.creation_error.clone())
                    .or_else(|| stored.and_then(|s| s.creation_error.clone())),
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
            "shot": group.shot_labels[i],
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

/// Switch from the active shot to its parent, with the group's columns open
/// on that shot.
pub(in crate::app::dispatch) fn open_parent_columns(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(shot_id) = app.active_view else {
        return vec![];
    };
    let Some(member) = app.agents.get(&shot_id).and_then(|a| a.n_shot.clone()) else {
        return vec![];
    };
    if !app.agents.contains_key(&member.parent) {
        app.show_toast("The parent session of this shot is closed");
        return vec![];
    }
    crate::app::dispatch::ctx::switch_to_agent(
        app,
        member.parent,
        crate::app::dispatch::ctx::SwitchCause::Picker,
    );
    let effects = open_columns(app, &member.label);
    if let Some(columns) = app.n_shot_columns.as_mut() {
        columns.focus = member.index - 1;
    }
    effects
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
        shot_labels: stored.shots.iter().map(|s| s.label.clone()).collect(),
        pending_write: None,
        write_retry_at: None,
        joined: stored.joined,
        last_statuses: stored.shots.iter().map(|s| s.status).collect(),
        started_at: std::time::Instant::now()
            .checked_sub(elapsed)
            .unwrap_or_else(std::time::Instant::now),
        settled_after: stored.joined.then_some(elapsed),
        stored: stored.shots.into_iter().map(Some).collect(),
    }
}

fn stored_summaries(group: &NShotGroup) -> Vec<ShotSummary> {
    group
        .stored
        .iter()
        .enumerate()
        .filter_map(|(i, shot)| {
            let shot = shot.as_ref()?;
            Some(ShotSummary {
                label: shot.label.clone(),
                index: i + 1,
                session_id: shot.session_id.clone(),
                worktree: shot
                    .worktree
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                status: shot.status,
                final_message: None,
                creation_error: shot.creation_error.clone(),
            })
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
        let Some(stored) = group.stored.get(i).and_then(Clone::clone) else {
            continue;
        };
        let Some(session_id) = stored.session_id.as_ref() else {
            continue;
        };
        if find_agent_by_session(app, session_id).is_none() {
            effects.extend(super::load::dispatch_load_session(
                app,
                session_id.clone(),
                stored.worktree.clone(),
                false,
            ));
        }
        let Some(shot_id) = find_agent_by_session(app, session_id) else {
            continue;
        };
        if let Some(shot) = app.agents.get_mut(&shot_id) {
            shot.n_shot = Some(NShotMember {
                shot_label: stored.label.clone(),
                creation_error: stored.creation_error.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::agent::AgentState;
    use crate::app::dispatch::tests::test_app_with_agent;

    /// Purpose: Create settled shots with custom labels and an inspectable parent queue.
    /// Input: None.
    /// Output: The app, without any filesystem or ACP effects.
    fn settled_group_app() -> AppView {
        let mut app = test_app_with_agent();
        let specs = crate::app::n_shot::parse_request_shots(&serde_json::json!({
            "prompt": "fix it",
            "shots": [{ "label": "minimal-fix" }, { "label": "rewrite" }],
        }))
        .unwrap();
        assert_eq!(
            start_group(&mut app, AgentId(0), "n-shot-1".into(), specs).len(),
            2
        );
        for shot_id in app.n_shot_groups[0].shots.clone() {
            let shot = app.agents.get_mut(&shot_id).unwrap();
            shot.session.session_id = Some(agent_client_protocol::SessionId::new(format!(
                "s{}",
                shot_id.0
            )));
            shot.session.cwd = std::path::PathBuf::from(format!("/wt/{}", shot_id.0));
            shot.session.is_worktree = true;
            shot.session.state = AgentState::Idle;
            shot.pending_first_prompt = None;
        }
        // Hold comparison prompts in the queue so tests can inspect them.
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;
        app
    }

    #[test]
    fn n_shot_unavailable_cannot_fork_from_typed_prompt() {
        use agent_client_protocol::AvailableCommand;
        let skill_meta = serde_json::json!({ "path": "/skills/n-shot.md" })
            .as_object()
            .cloned();
        let cases = [
            vec![],
            vec![AvailableCommand::new("workflow", "Manage old runs")],
            vec![AvailableCommand::new("n-shot", "User skill").meta(skill_meta)],
        ];
        for commands in cases {
            let mut app = test_app_with_agent();
            let parent = app.agents.get_mut(&AgentId(0)).unwrap();
            parent.current_branch = Some("main".into());
            parent.session.available_commands = commands;
            let text = "/goal fix it /n-shot 2";
            parent.prompt.set_text(text);
            let effects = crate::app::dispatch::dispatch(
                crate::app::actions::Action::SendPrompt(text.into()),
                &mut app,
            );
            assert!(effects.is_empty());
            assert_eq!(app.agents.len(), 1);
            assert!(app.n_shot_groups.is_empty());
            assert!(app.n_shot_requests.is_empty());
            assert_eq!(app.agents[&AgentId(0)].prompt.text(), text);
        }
    }

    #[test]
    fn n_shot_advertised_builtin_can_fork_from_typed_prompt() {
        let mut app = test_app_with_agent();
        let parent = app.agents.get_mut(&AgentId(0)).unwrap();
        parent.current_branch = Some("main".into());
        parent.session.available_commands = vec![agent_client_protocol::AvailableCommand::new(
            "n-shot",
            "Parallel attempts",
        )];
        let effects = crate::app::dispatch::dispatch(
            crate::app::actions::Action::SendPrompt("/n-shot 2 fix it".into()),
            &mut app,
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::CreateWorktreeSession { .. }))
                .count(),
            2
        );
        assert_eq!(app.n_shot_groups.len(), 1);
        assert_eq!(app.agents.len(), 3);
    }

    /// Purpose: Keep the group open until subagents and workflows finish.
    /// Input: Two idle shots, each with a different type of background work.
    /// Output: No report or comparison until both types of background work finish.
    #[test]
    fn n_shot_waits_for_subagents_and_workflows() {
        let mut app = settled_group_app();
        let shot_ids = app.n_shot_groups[0].shots.clone();
        let mut child = crate::app::agent_view::test_fixtures::running_subagent_info("child");
        child.is_background = true;
        app.agents
            .get_mut(&shot_ids[0])
            .unwrap()
            .subagent_sessions
            .insert("child".into(), child);
        app.agents
            .get_mut(&shot_ids[1])
            .unwrap()
            .workflow_runs
            .push(crate::views::workflows::WorkflowRunSnapshot {
                run_id: "wf-1".into(),
                name: "workflow".into(),
                objective: "fix it".into(),
                status: "active".into(),
                management_available: true,
                builtin: false,
                phases: vec![],
                current_phase: None,
                agents: vec![],
                agent_budget: None,
                agents_used: 0,
                agents_reserved: 0,
                agents_remaining: None,
                agent_usage_incomplete: false,
                active_agents: 0,
                elapsed_ms: 0,
                received_at: std::time::Instant::now(),
                pause_message: None,
                result_summary: None,
            });
        for id in &shot_ids {
            assert_eq!(classify_shot(app.agents.get(id)), ShotStatus::Working);
        }
        let effects = tick(&mut app);
        let Effect::WriteNShotFiles { files, .. } = &effects[0] else {
            panic!("missing writer")
        };
        assert!(!files.iter().any(|(path, _)| path.ends_with("report.md")));
        files_written(&mut app, AgentId(0), "n-shot-1", Ok(()));
        assert!(!app.n_shot_groups[0].joined);
        assert!(app.agents[&AgentId(0)].session.pending_prompts.is_empty());
        app.agents
            .get_mut(&shot_ids[0])
            .unwrap()
            .subagent_sessions
            .get_mut("child")
            .unwrap()
            .finished = true;
        assert_eq!(
            classify_shot(app.agents.get(&shot_ids[0])),
            ShotStatus::Settled
        );
        tick(&mut app);
        files_written(&mut app, AgentId(0), "n-shot-1", Ok(()));
        assert!(!app.n_shot_groups[0].joined);
        assert!(app.agents[&AgentId(0)].session.pending_prompts.is_empty());
        app.agents.get_mut(&shot_ids[1]).unwrap().workflow_runs[0].status = "complete".into();
        assert_eq!(
            classify_shot(app.agents.get(&shot_ids[1])),
            ShotStatus::Settled
        );
        let effects = tick(&mut app);
        let Effect::WriteNShotFiles { files, .. } = &effects[0] else {
            panic!("missing writer")
        };
        assert!(files.iter().any(|(path, _)| path.ends_with("report.md")));
        files_written(&mut app, AgentId(0), "n-shot-1", Ok(()));
        assert!(app.n_shot_groups[0].joined);
        assert_eq!(app.agents[&AgentId(0)].session.pending_prompts.len(), 1);
    }

    #[test]
    fn n_shot_workflow_owned_child_keeps_shot_open() {
        let mut app = settled_group_app();
        let shot_id = app.n_shot_groups[0].shots[0];
        let shot = app.agents.get_mut(&shot_id).unwrap();
        let mut child = crate::app::agent_view::test_fixtures::running_subagent_info("child");
        child.workflow_run_id = Some("paused-workflow".into());
        shot.subagent_sessions.insert("child".into(), child);
        assert_eq!(classify_shot(Some(shot)), ShotStatus::Working);
        shot.subagent_sessions.get_mut("child").unwrap().finished = true;
        assert_eq!(classify_shot(Some(shot)), ShotStatus::Settled);
    }

    #[test]
    fn n_shot_closed_view_keeps_identity_record_and_click_through() {
        let mut app = settled_group_app();
        let closed_id = app.n_shot_groups[0].shots[0];
        // Close before the first tick to cover the earliest possible removal.
        super::super::modal::remove_agent_and_cleanup(&mut app, closed_id);
        let effects = tick(&mut app);
        let group = &app.n_shot_groups[0];
        let shots = group.pending_write.as_ref().unwrap();
        assert_eq!(shots[0].session_id.as_deref(), Some("s1"));
        assert_eq!(shots[0].worktree.as_deref(), Some("/wt/1"));
        assert_eq!(shots[0].status, ShotStatus::Closed);
        let record = build_record(group, "parent", std::path::Path::new("/r/report.md"), shots);
        let stored = parse_record(&record).unwrap();
        assert_eq!(stored.shots.len(), 2);
        assert_eq!(stored.shots[0].label, "minimal-fix");
        assert_eq!(stored.shots[1].session_id.as_deref(), Some("s2"));
        assert!(matches!(&effects[0], Effect::WriteNShotFiles { .. }));
        let restored = restored_group(&mut app, AgentId(0), stored);
        assert_eq!(restored.shot_labels, ["minimal-fix", "rewrite"]);
        assert_eq!(
            stored_summaries(&restored)[0].session_id.as_deref(),
            Some("s1")
        );
        let loads = open_columns(&mut app, "n-shot-1");
        assert!(!loads.is_empty());
        let group = &app.n_shot_groups[0];
        let reloaded = &app.agents[&group.shots[0]];
        assert_eq!(reloaded.n_shot.as_ref().unwrap().shot_label, "minimal-fix");
    }

    #[test]
    fn n_shot_failed_creation_settles_reports_and_restores() {
        let mut app = settled_group_app();
        let failed_id = app.n_shot_groups[0].shots[0];
        let shot = app.agents.get_mut(&failed_id).unwrap();
        shot.session.session_id = None;
        shot.session.state = AgentState::CommandRunning {
            command: crate::app::agent::AgentCommand::CreateWorktree,
            started_at: std::time::Instant::now(),
        };
        assert_eq!(classify_shot(Some(shot)), ShotStatus::Starting);
        super::super::lifecycle::handle_worktree_session_failed(
            &mut app,
            failed_id,
            "disk full".into(),
        );
        assert_eq!(
            classify_shot(app.agents.get(&failed_id)),
            ShotStatus::Failed
        );
        let effects = tick(&mut app);
        let Effect::WriteNShotFiles { files, .. } = &effects[0] else {
            panic!("missing writer")
        };
        let report = &files
            .iter()
            .find(|(path, _)| path.ends_with("report.md"))
            .unwrap()
            .1;
        assert!(report.contains("- Status: failed"));
        assert!(report.contains("Creation error: disk full"));
        let record: serde_json::Value = serde_json::from_str(&files.last().unwrap().1).unwrap();
        assert_eq!(record["joined"], true);
        assert_eq!(record["shots"][0]["worktree"], serde_json::Value::Null);
        let stored = parse_record(&record).unwrap();
        assert_eq!(stored.shots.len(), 2);
        files_written(&mut app, AgentId(0), "n-shot-1", Ok(()));
        assert!(app.n_shot_groups[0].joined);
        assert_eq!(app.agents[&AgentId(0)].session.pending_prompts.len(), 1);
        super::super::modal::remove_agent_and_cleanup(&mut app, failed_id);
        assert_eq!(
            summarize_shots(&app, &app.n_shot_groups[0])[0].status,
            ShotStatus::Failed
        );
        let successful_id = app.n_shot_groups[0].shots[1];
        super::super::modal::remove_agent_and_cleanup(&mut app, successful_id);
        let restored = restored_group(&mut app, AgentId(0), stored);
        let summaries = summarize_shots(&app, &restored);
        assert_eq!(summaries[0].status, ShotStatus::Failed);
        assert_eq!(summaries[0].creation_error.as_deref(), Some("disk full"));
        app.n_shot_groups[0] = restored;
        let loads = open_columns(&mut app, "n-shot-1");
        let session_ids: Vec<&str> = loads
            .iter()
            .filter_map(|effect| match effect {
                Effect::LoadSession { session_id, .. } => Some(session_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            session_ids,
            ["s2"],
            "only the successful shot has a session to load"
        );
    }

    #[test]
    fn n_shot_comparison_waits_for_report_write_success() {
        let mut app = settled_group_app();
        let effects = tick(&mut app);
        assert!(!app.n_shot_groups[0].joined);
        assert!(app.agents[&AgentId(0)].session.pending_prompts.is_empty());
        assert!(tick(&mut app).is_empty(), "one write at a time");
        let Effect::WriteNShotFiles { files, .. } = &effects[0] else {
            panic!("missing writer")
        };
        assert!(files.last().unwrap().0.ends_with("shots.json"));
        assert!(
            files
                .iter()
                .any(|(path, report)| path.ends_with("report.md")
                    && report.contains("## minimal-fix"))
        );
        files_written(&mut app, AgentId(0), "n-shot-1", Ok(()));
        assert!(app.n_shot_groups[0].joined);
        assert_eq!(app.agents[&AgentId(0)].session.pending_prompts.len(), 1);
        files_written(&mut app, AgentId(0), "n-shot-1", Ok(()));
        assert_eq!(app.agents[&AgentId(0)].session.pending_prompts.len(), 1);
    }

    #[test]
    fn n_shot_failed_report_write_retries_without_join_or_comparison() {
        let mut app = settled_group_app();
        tick(&mut app);
        files_written(&mut app, AgentId(0), "n-shot-1", Err("disk full".into()));
        assert!(!app.n_shot_groups[0].joined);
        assert!(app.n_shot_groups[0].pending_write.is_none());
        assert!(app.agents[&AgentId(0)].session.pending_prompts.is_empty());
        assert!(tick(&mut app).is_empty(), "wait for the retry delay");
        app.n_shot_groups[0].write_retry_at = Some(std::time::Instant::now());
        assert!(matches!(&tick(&mut app)[0], Effect::WriteNShotFiles { .. }));
        files_written(&mut app, AgentId(0), "n-shot-1", Ok(()));
        assert!(app.n_shot_groups[0].joined);
        assert_eq!(app.agents[&AgentId(0)].session.pending_prompts.len(), 1);
    }
}
