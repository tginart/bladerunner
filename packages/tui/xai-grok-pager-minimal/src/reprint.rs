//! Reprint the session history at the new terminal width after a resize.
//!
//! Committed blocks are printed once, wrapped at the width of that moment. A
//! reflowing terminal re-wraps them after a resize and splits words; a
//! non-reflowing one keeps rows that no longer fit. A height change that
//! flips the compact layout also makes the history stale. When
//! [`xai_grok_pager::minimal_reprint`] asks for it, this module clears the
//! screen and the terminal scrollback and prints the committed entries again.
//!
//! The clear also removes terminal output from before grok started and from
//! earlier `/new` sessions. Only the newest [`REPRINT_MAX_ROWS`] rows are
//! reprinted. Older blocks stay available through `/transcript`.

use std::io;
use std::time::Instant;

use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{Clear, ClearType};
use ratatui::text::Span;

use xai_grok_pager::app::PagerTerminal;
use xai_grok_pager::app::app_view::{ActiveView, AppView};
use xai_grok_pager::minimal_api;
use xai_grok_pager::minimal_reprint::{self, ReprintDecision};
use xai_grok_pager::render::Renderable;
use xai_grok_pager::theme::Theme;

use crate::commit::{
    COMMITTED_TICK, committed_appearance, insert_committed, insert_committed_tail,
    minimal_commit_display_mode, minimal_renderer,
};

/// Row budget for one reprint, counted back from the newest committed block.
/// It bounds the write burst on a long session.
pub(crate) const REPRINT_MAX_ROWS: u16 = 4000;

/// Reprint the history once a width or compact-layout change has held for
/// the debounce. Call it before the commits of this frame.
pub fn maybe_reprint(app: &mut AppView, terminal: &mut PagerTerminal) {
    let width = terminal.viewport_area().width;
    // Too narrow for the welcome card. Rows printed meanwhile still mark the
    // history as mixed, so the reprint runs once the terminal is wide again.
    if width < crate::welcome::MIN_CARD_WIDTH {
        return;
    }
    if minimal_reprint::observe_minimal_layout(app, width, Instant::now())
        != ReprintDecision::Reprint
    {
        return;
    }
    // A reprint under a centered app-modal would scroll the popup away. The
    // decision repeats each frame, so the reprint runs after it closes.
    if let ActiveView::Agent(id) = &app.active_view
        && app
            .agents
            .get(id)
            .is_some_and(crate::overlay::app_modal_active)
    {
        return;
    }
    match reprint_history(app, terminal, width) {
        Ok(()) => minimal_reprint::mark_minimal_history_printed(app, width),
        Err(error) => {
            tracing::warn!(%error, width, "minimal: history reprint failed; retrying next frame")
        }
    }
}

fn reprint_history(app: &AppView, terminal: &mut PagerTerminal, width: u16) -> io::Result<()> {
    let backend = terminal.backend_mut();
    backend.queue(SetAttribute(Attribute::Reset))?;
    backend.queue(MoveTo(0, 0))?;
    backend.queue(Clear(ClearType::All))?;
    backend.queue(Clear(ClearType::Purge))?;
    backend.queue(MoveTo(0, 0))?;
    crate::welcome::print_welcome_card(app, terminal)?;
    reprint_committed(app, terminal, width)
}

/*
Purpose: This function prints the committed entries of the active agent
    again, oldest first.
Input: The app. The terminal. The new width.
Output: An I/O error if a terminal write fails.
Method:
    1. Collect the committed entries. An entry still in its commit display
       mode gets the normal row cap. An entry that the user expanded with
       Ctrl+E gets no cap.
    2. Walk back from the newest entry and keep entries while the total row
       count stays in REPRINT_MAX_ROWS. Clip the first entry that does not
       fit to its newest rows, so a tall expanded block stays visible.
    3. If older entries do not fit, print one note row that tells the user
       how many blocks are left out.
    4. Print each kept entry with the same renderer that the commit pass uses.
*/
fn reprint_committed(app: &AppView, terminal: &mut PagerTerminal, width: u16) -> io::Result<()> {
    let ActiveView::Agent(id) = &app.active_view else {
        return Ok(());
    };
    let Some(agent) = app.agents.get(id) else {
        return Ok(());
    };
    let appearance = committed_appearance(&app.appearance);
    let max_rows = appearance.minimal_max_commit_rows;
    let theme = Theme::current();
    let footer_style = theme.dim();
    let cwd = agent.session.cwd.as_path();
    let sb = &agent.scrollback;

    let mut committed: Vec<(usize, u16)> = Vec::new();
    for i in 0..sb.len() {
        let Some(entry) = sb.get(i).filter(|e| minimal_api::is_committed(sb, e)) else {
            continue;
        };
        let is_commit_mode =
            entry.display_mode() == minimal_commit_display_mode(&entry.block, &appearance);
        committed.push((i, if is_commit_mode { max_rows } else { 0 }));
    }

    // Lazy, so a long session measures only the blocks near the budget edge.
    let rows_newest_first = committed.iter().rev().map(|&(i, cap)| {
        let Some(entry) = sb.get(i) else { return 0 };
        let renderer = minimal_renderer(entry, &theme, appearance.clone(), cwd, COMMITTED_TICK);
        let full = renderer.desired_height(width);
        if cap > 0 { full.min(cap) } else { full }
    });
    let plan = plan_reprint(rows_newest_first, REPRINT_MAX_ROWS);
    let first = committed.len() - plan.kept;

    if first > 0 {
        let note = format!("\u{2026} {first} earlier blocks \u{00b7} /transcript to view");
        terminal.insert_before(1, |buf| {
            buf.set_span(0, 0, &Span::styled(note, footer_style), width);
        })?;
    }
    for (n, &(i, cap)) in committed[first..].iter().enumerate() {
        let Some(entry) = sb.get(i) else { continue };
        let renderer = minimal_renderer(entry, &theme, appearance.clone(), cwd, COMMITTED_TICK);
        match plan.tail_rows {
            Some(rows) if n == 0 => {
                insert_committed_tail(terminal, renderer, width, rows, footer_style)?
            }
            _ => insert_committed(terminal, renderer, width, cap, footer_style)?,
        }
    }
    Ok(())
}

/*
Purpose: This function selects the committed blocks that the reprint prints.
Input: The row count of each block, newest first. The row budget.
Output: A ReprintPlan.
Method:
    1. Keep each block while its rows fit in the remaining budget.
    2. If a block does not fit, keep its newest rows to fill the budget. Then
       stop.
Note: A clipped block needs a note row and a content row. If less than two
    rows remain, the plan does not keep the block.
*/
fn plan_reprint(rows_newest_first: impl IntoIterator<Item = u16>, budget: u16) -> ReprintPlan {
    let mut plan = ReprintPlan {
        kept: 0,
        tail_rows: None,
    };
    let mut budget = budget;
    for rows in rows_newest_first {
        if rows > budget {
            if budget >= 2 {
                plan.kept += 1;
                plan.tail_rows = Some(budget);
            }
            break;
        }
        budget -= rows;
        plan.kept += 1;
    }
    plan
}

/// The blocks that one reprint prints, counted back from the newest block.
#[derive(Debug, PartialEq, Eq)]
struct ReprintPlan {
    /// The number of blocks to print. It includes a clipped block.
    kept: usize,
    /// The row count of the oldest kept block, if the plan clips it to its
    /// newest rows.
    tail_rows: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_newest_block_keeps_its_tail() {
        let plan = plan_reprint([5000, 10, 10], REPRINT_MAX_ROWS);
        assert_eq!(
            plan,
            ReprintPlan {
                kept: 1,
                tail_rows: Some(REPRINT_MAX_ROWS),
            }
        );
    }

    #[test]
    fn boundary_block_fills_the_remaining_budget() {
        let plan = plan_reprint([3000, 2000, 10], REPRINT_MAX_ROWS);
        assert_eq!(
            plan,
            ReprintPlan {
                kept: 2,
                tail_rows: Some(1000),
            }
        );
    }

    #[test]
    fn blocks_that_fit_are_not_clipped() {
        let plan = plan_reprint([10, 20, 30], REPRINT_MAX_ROWS);
        assert_eq!(
            plan,
            ReprintPlan {
                kept: 3,
                tail_rows: None,
            }
        );
    }

    #[test]
    fn one_row_of_budget_does_not_clip() {
        let plan = plan_reprint([3999, 50], REPRINT_MAX_ROWS);
        assert_eq!(
            plan,
            ReprintPlan {
                kept: 1,
                tail_rows: None,
            }
        );
    }
}
