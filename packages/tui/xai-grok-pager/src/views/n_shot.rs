//! Side-by-side columns for a built-in `n-shot` workflow run.
//!
//! The `/workflows` detail view draws this instead of the phase rail when the
//! run is an n-shot run. Each column is one shot: a header with live status,
//! the last outcome line from the run result, and a dense live tail of the
//! shot's own transcript.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::render::SafeBuf;
use crate::scrollback::state::ScrollbackState;
use crate::theme::Theme;
use crate::views::agent_status::format_tokens_compact;
use crate::views::goal_detail::{format_elapsed, strip_control_chars, truncate_to_width};
use crate::views::workflows::{
    WorkflowAgentLiveMap, WorkflowAgentRowView, WorkflowRunSnapshot, WorkflowsViewState,
};

const N_SHOT_WORKFLOW_NAME: &str = "n-shot";
const MIN_COLUMN_WIDTH: u16 = 24;
const HEADER_ROWS: u16 = 3;

/// Transcript of each workflow agent, keyed by agent id.
pub type WorkflowAgentScrollbacks<'a> = HashMap<String, &'a ScrollbackState>;

/// Whether `run` is an n-shot workflow run (display names may carry a `-2`
/// style suffix when several runs share a session).
///
/// Matches by name only: the `/n-shot` slash command takes the workflow's
/// command name, so the run is never flagged `builtin` by the command list.
pub fn is_n_shot_run(run: &WorkflowRunSnapshot) -> bool {
    run.name == N_SHOT_WORKFLOW_NAME
        || run
            .name
            .strip_prefix(N_SHOT_WORKFLOW_NAME)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// The shots of an n-shot run, in launch order.
pub fn shots(run: &WorkflowRunSnapshot) -> Vec<&WorkflowAgentRowView> {
    run.agents_in_phase(Some("Shots"))
}

/// Purpose: Draw the n-shot detail view into the inner area of the workflows modal.
/// Input: The run, the view state, the animation tick, live agent status, and agent
///     transcripts.
/// Output: None. The function writes into `buf` and records column hit areas in
///     `state.agent_hits`.
/// Method:
///     1. Draw the run header and the shared prompt.
///     2. Pick a window of columns that fits the width and contains the focused shot.
///     3. Draw one bordered column per visible shot.
#[allow(clippy::too_many_arguments)]
pub fn render_n_shot_detail(
    buf: &mut Buffer,
    inner: Rect,
    run: &WorkflowRunSnapshot,
    state: &mut WorkflowsViewState,
    tick: usize,
    theme: &Theme,
    live: &WorkflowAgentLiveMap,
    scrollbacks: &WorkflowAgentScrollbacks<'_>,
) {
    let shots = shots(run);
    render_header(buf, inner, run, shots.len(), tick, theme);
    let body = Rect {
        y: inner.y + HEADER_ROWS,
        height: inner.height.saturating_sub(HEADER_ROWS),
        ..inner
    };
    if shots.is_empty() || body.height < 3 || body.width < 4 {
        return;
    }
    state.n_shot_focus = state.n_shot_focus.min(shots.len() - 1);
    let (first, visible) = column_window(body.width, shots.len(), state.n_shot_focus);
    let column_w = body.width / visible as u16;
    for (slot, shot) in shots.iter().skip(first).take(visible).enumerate() {
        let x = body.x + slot as u16 * column_w;
        let width = if slot + 1 == visible {
            body.right() - x
        } else {
            column_w
        };
        let rect = Rect { x, width, ..body };
        let column = ShotColumn {
            shot,
            index: first + slot,
            is_focused: first + slot == state.n_shot_focus,
            live: live.get(&shot.agent_id),
            scrollback: scrollbacks.get(&shot.agent_id).copied(),
            outcome: shot_outcome(run.result_summary.as_deref(), &shot.label),
        };
        render_column(buf, rect, &column, tick, theme);
        state.agent_hits.push((rect, shot.agent_id.clone()));
    }
}

/// Purpose: Draw the columns of an n-shot group whose shots are full sessions.
/// Input: The buffer, the area to cover, the columns state, the group, and the
///     agents that hold each shot's session.
/// Output: None. The function draws a bordered window over `area`.
/// Method:
///     1. Describe the group as a run snapshot, so the subagent column code applies.
///     2. Take each shot's live status and transcript from its agent.
///     3. Draw the window, the columns, and a key hint.
pub(crate) fn render_session_columns(
    buf: &mut Buffer,
    area: Rect,
    columns: &mut crate::app::n_shot::NShotColumnsState,
    group: &crate::app::n_shot::NShotGroup,
    agents: &indexmap::IndexMap<crate::app::agent::AgentId, crate::app::agent_view::AgentView>,
) {
    use crate::app::n_shot::{classify_shot, shot_label};
    use crate::views::workflows::WorkflowAgentLiveStatus;

    let theme = Theme::current();
    let window = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if window.width < 10 || window.height < 6 {
        return;
    }
    ratatui::widgets::Clear.render(window, buf);
    let frame = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent_plan))
        .style(Style::default().bg(theme.bg_base))
        .title(format!(" {} ", group.label));
    let inner = frame.inner(window);
    frame.render(window, buf);

    let mut live = WorkflowAgentLiveMap::new();
    let mut scrollbacks = WorkflowAgentScrollbacks::new();
    let mut rows = Vec::with_capacity(group.shots.len());
    for (i, shot_id) in group.shots.iter().enumerate() {
        let agent = agents.get(shot_id);
        let status = classify_shot(agent);
        let key = shot_id.0.to_string();
        rows.push(WorkflowAgentRowView {
            agent_id: key.clone(),
            label: shot_label(i + 1),
            phase: Some("Shots".to_string()),
            model: None,
            state: if status.is_settled() {
                "done"
            } else {
                "running"
            }
            .to_string(),
            tokens_used: 0,
            duration_ms: 0,
        });
        live.insert(
            key.clone(),
            WorkflowAgentLiveStatus {
                activity: Some(status.label()),
                ..Default::default()
            },
        );
        if let Some(agent) = agent {
            scrollbacks.insert(key, &agent.scrollback);
        }
    }
    let run = WorkflowRunSnapshot {
        run_id: group.label.clone(),
        name: group.label.clone(),
        objective: group.prompt.clone(),
        status: if group.joined { "complete" } else { "active" }.to_string(),
        management_available: false,
        builtin: true,
        phases: Vec::new(),
        current_phase: None,
        agents: rows,
        agent_budget: None,
        agents_used: 0,
        agents_reserved: 0,
        agents_remaining: None,
        agent_usage_incomplete: false,
        active_agents: 0,
        elapsed_ms: if group.joined {
            group.elapsed().as_millis() as u64
        } else {
            0
        },
        received_at: group.started_at,
        pause_message: None,
        result_summary: None,
    };
    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let mut state = WorkflowsViewState {
        n_shot_focus: columns.focus,
        ..Default::default()
    };
    render_n_shot_detail(buf, body, &run, &mut state, 0, &theme, &live, &scrollbacks);
    columns.focus = state.n_shot_focus;
    let hint = "\u{2190}\u{2192} shot \u{00b7} Enter attach \u{00b7} Esc close";
    write_line(
        buf,
        inner.x + 1,
        inner.bottom().saturating_sub(1),
        hint,
        Style::default().fg(theme.gray_dim),
        inner.width.saturating_sub(2),
    );
}

struct ShotColumn<'a> {
    shot: &'a WorkflowAgentRowView,
    index: usize,
    is_focused: bool,
    live: Option<&'a crate::views::workflows::WorkflowAgentLiveStatus>,
    scrollback: Option<&'a ScrollbackState>,
    outcome: Option<String>,
}

fn render_header(
    buf: &mut Buffer,
    inner: Rect,
    run: &WorkflowRunSnapshot,
    shot_count: usize,
    tick: usize,
    theme: &Theme,
) {
    let glyph = if run.is_active() {
        let frames = crate::glyphs::dot_spinner_frames();
        frames[(tick / 4) % frames.len()].to_string()
    } else if run.status == "complete" {
        "✓".to_string()
    } else {
        "◌".to_string()
    };
    let title = format!(
        "{glyph} {} · {}/{shot_count} shots done · {}",
        strip_control_chars(&run.name, false),
        run.done_agents(),
        format_elapsed(run.live_elapsed_ms()),
    );
    let width = inner.width.saturating_sub(2);
    let title_style = Style::default()
        .fg(theme.accent_plan)
        .add_modifier(Modifier::BOLD);
    write_line(buf, inner.x + 1, inner.y, &title, title_style, width);
    let prompt = format!("prompt: {}", one_line(&run.objective));
    let prompt_style = Style::default().fg(theme.gray);
    write_line(buf, inner.x + 1, inner.y + 1, &prompt, prompt_style, width);
}

fn render_column(
    buf: &mut Buffer,
    rect: Rect,
    column: &ShotColumn<'_>,
    tick: usize,
    theme: &Theme,
) {
    let shot = column.shot;
    let (glyph, glyph_style) = shot_glyph(&shot.state, tick, theme);
    let border_style = if column.is_focused {
        Style::default().fg(theme.accent_plan)
    } else {
        Style::default().fg(theme.gray_dim)
    };
    let title = format!(" {glyph} {} ", strip_control_chars(&shot.label, false));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(ratatui::text::Span::styled(title, glyph_style));
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut y = inner.y;
    let dim = Style::default().fg(theme.gray_dim);
    write_line(buf, inner.x, y, &shot_meta(column), dim, inner.width);
    y += 1;
    let (status_text, status_style) = match (&column.outcome, column.live) {
        (Some(outcome), _) => (outcome.clone(), Style::default().fg(theme.gray)),
        (None, Some(live)) if shot.state == "running" => (
            live.activity
                .clone()
                .unwrap_or_else(|| "working…".to_string()),
            Style::default().fg(theme.accent_plan),
        ),
        _ => (shot.state.clone(), dim),
    };
    if y < inner.bottom() {
        write_line(
            buf,
            inner.x,
            y,
            &one_line(&status_text),
            status_style,
            inner.width,
        );
        y += 1;
    }
    let tail = Rect {
        y,
        height: inner.bottom().saturating_sub(y),
        ..inner
    };
    if let Some(scrollback) = column.scrollback {
        crate::views::dashboard::peek_tail::paint_peek_live_tail(scrollback, tail, buf);
    }
}

fn shot_meta(column: &ShotColumn<'_>) -> String {
    let shot = column.shot;
    let mut parts = vec![format!("#{}", column.index + 1)];
    if let Some(model) = shot.model.as_deref().filter(|m| !m.is_empty()) {
        parts.push(strip_control_chars(model, false));
    }
    let live_elapsed = column.live.and_then(|l| l.elapsed_ms);
    let elapsed_ms = live_elapsed.unwrap_or(shot.duration_ms);
    if elapsed_ms > 0 {
        parts.push(format_elapsed(elapsed_ms));
    }
    let tokens = column
        .live
        .and_then(|l| l.tokens_used)
        .unwrap_or(shot.tokens_used);
    if tokens > 0 {
        let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
        parts.push(format!("{} tok", format_tokens_compact(tokens)));
    }
    parts.join(" · ")
}

fn shot_glyph(state: &str, tick: usize, theme: &Theme) -> (String, Style) {
    match state {
        "running" => {
            let frames = crate::glyphs::dot_spinner_frames();
            (
                frames[(tick / 4) % frames.len()].to_string(),
                Style::default().fg(theme.accent_plan),
            )
        }
        "done" => ("✓".into(), Style::default().fg(theme.accent_success)),
        "failed" => ("✗".into(), Style::default().fg(theme.accent_error)),
        _ => ("◌".into(), Style::default().fg(theme.gray_dim)),
    }
}

/// Purpose: Pick the columns that fit the width.
/// Input: The body width, the shot count, and the focused shot index.
/// Output: The index of the first visible shot and the count of visible shots.
fn column_window(width: u16, count: usize, focus: usize) -> (usize, usize) {
    let fit = usize::from((width / MIN_COLUMN_WIDTH).max(1));
    let visible = fit.min(count);
    let first = focus.saturating_sub(visible - 1).min(count - visible);
    (first, visible)
}

/// Purpose: Find the outcome line of one shot in the run result.
/// Input: The run result summary and the shot label.
/// Output: The text after `- <label>: ` on the matching overview line, if any.
/// Note: The `n-shot` workflow writes one overview line per shot in this form.
fn shot_outcome(result_summary: Option<&str>, label: &str) -> Option<String> {
    let prefix = format!("- {label}: ");
    result_summary?
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::to_string)
}

fn one_line(text: &str) -> String {
    strip_control_chars(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        false,
    )
}

fn write_line(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, width: u16) {
    if width == 0 {
        return;
    }
    let text = truncate_to_width(text, usize::from(width));
    buf.set_span_safe(x, y, &ratatui::text::Span::styled(text, style), width);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_run(run_id: &str, name: &str, status: &str) -> WorkflowRunSnapshot {
        WorkflowRunSnapshot {
            run_id: run_id.into(),
            name: name.into(),
            objective: String::new(),
            status: status.into(),
            management_available: true,
            builtin: false,
            phases: Vec::new(),
            current_phase: None,
            agents: Vec::new(),
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
        }
    }

    fn shot(id: &str, label: &str, state: &str) -> WorkflowAgentRowView {
        WorkflowAgentRowView {
            agent_id: id.into(),
            label: label.into(),
            phase: Some("Shots".into()),
            model: None,
            state: state.into(),
            tokens_used: 0,
            duration_ms: 0,
        }
    }

    #[test]
    fn detects_n_shot_runs_by_display_name() {
        let mut run = make_run("r1", "n-shot", "active");
        assert!(is_n_shot_run(&run));
        run.name = "n-shot-2".into();
        assert!(is_n_shot_run(&run));
        for other in ["n-shot-extra", "n-shot-", "deep-research"] {
            run.name = other.into();
            assert!(!is_n_shot_run(&run), "{other}");
        }
    }

    #[test]
    fn column_window_keeps_focus_visible() {
        assert_eq!(column_window(200, 3, 0), (0, 3));
        assert_eq!(column_window(48, 5, 0), (0, 2));
        assert_eq!(column_window(48, 5, 4), (3, 2));
        assert_eq!(column_window(10, 4, 2), (2, 1));
    }

    #[test]
    fn shot_outcome_reads_the_overview_line() {
        let summary = "# n-shot\n\n- shot-1: done; 2 files changed; worktree /w/1\n\
                       - shot-10: failed; unknown; worktree (none)\n";
        assert_eq!(
            shot_outcome(Some(summary), "shot-1").as_deref(),
            Some("done; 2 files changed; worktree /w/1")
        );
        assert_eq!(
            shot_outcome(Some(summary), "shot-10").as_deref(),
            Some("failed; unknown; worktree (none)")
        );
        assert_eq!(shot_outcome(Some(summary), "shot-2"), None);
        assert_eq!(shot_outcome(None, "shot-1"), None);
    }

    #[test]
    fn renders_one_column_per_shot_and_records_hits() {
        let mut run = make_run("r1", "n-shot", "active");
        run.objective = "fix the bug".into();
        run.agents = vec![
            shot("a1", "shot-1", "running"),
            shot("a2", "shot-2", "done"),
        ];
        let mut state = WorkflowsViewState::default();
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        render_n_shot_detail(
            &mut buf,
            area,
            &run,
            &mut state,
            0,
            &Theme::current(),
            &WorkflowAgentLiveMap::new(),
            &WorkflowAgentScrollbacks::new(),
        );
        let ids: Vec<&str> = state.agent_hits.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, ["a1", "a2"]);
        let text: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("shot-1"));
        assert!(text.contains("shot-2"));
        assert!(text.contains("prompt: fix the bug"));
    }
}
