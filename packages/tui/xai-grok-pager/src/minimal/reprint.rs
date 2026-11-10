//! Decide when minimal mode reprints its history after a layout change.
//!
//! Minimal mode prints committed blocks once into the terminal's native
//! scrollback, wrapped at the width of that moment. After a resize, a
//! reflowing terminal re-wraps those rows and splits words, and a
//! non-reflowing one keeps the old wrap. A height change can also flip the
//! compact prompt layout. In each case the printed history no longer matches
//! the live region. This module tracks the layout that the history was
//! printed in and asks for a reprint once a new layout holds for
//! [`REPRINT_DEBOUNCE`]. `xai-grok-pager-minimal` does the printing.

use std::time::{Duration, Instant};

use crate::app::app_view::AppView;

/// A dragged window sends a burst of resize events. The reprint waits until
/// they stop for this long, so it runs once and not on each event.
pub const REPRINT_DEBOUNCE: Duration = Duration::from_millis(120);

/// What a minimal frame must do about the printed history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReprintDecision {
    /// The history is already printed in this layout.
    Keep,
    /// The layout changed less than [`REPRINT_DEBOUNCE`] ago.
    Wait,
    /// Reprint now, then call [`mark_minimal_history_printed`].
    Reprint,
}

/// The wrap width and compact flag that committed history is rendered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrintedLayout {
    width: u16,
    compact: bool,
}

/// What native scrollback holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrintedHistory {
    /// Every row was printed in this layout.
    Uniform(PrintedLayout),
    /// Some rows were printed in a different layout. A return to an earlier
    /// layout does not repair them, so only a reprint makes it uniform again.
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingReprint {
    layout: PrintedLayout,
    due_at: Instant,
}

/// The reprint bookkeeping. It lives on `AppView::minimal_state`.
#[derive(Debug, Default)]
pub(crate) struct ReprintState {
    /// `None` until the first minimal frame or the first printed rows.
    printed: Option<PrintedHistory>,
    pending: Option<PendingReprint>,
}

impl ReprintState {
    /// True while a reprint waits for its debounce. The tick loop keeps
    /// drawing during this time, so the reprint runs without new input.
    pub(crate) fn is_waiting(&self) -> bool {
        self.pending.is_some()
    }

    /*
    Purpose: This function decides what to do about the printed history for the
        layout of this frame.
    Input: The layout of this frame. The current time.
    Output: Keep, Wait or Reprint.
    Method:
        1. If nothing is printed yet, record the layout as printed and keep it.
        2. If the history is uniform in this layout, cancel a pending reprint.
        3. If a reprint for this layout is pending and due, reprint.
        4. If not, start or restart the debounce for this layout and wait.
    Note: Reprint repeats on each frame until mark_printed is called.
    */
    fn observe(&mut self, layout: PrintedLayout, now: Instant) -> ReprintDecision {
        match self.printed {
            None => {
                self.printed = Some(PrintedHistory::Uniform(layout));
                return ReprintDecision::Keep;
            }
            Some(PrintedHistory::Uniform(printed)) if printed == layout => {
                self.pending = None;
                return ReprintDecision::Keep;
            }
            Some(_) => {}
        }
        match self.pending {
            Some(pending) if pending.layout == layout && now >= pending.due_at => {
                ReprintDecision::Reprint
            }
            Some(pending) if pending.layout == layout => ReprintDecision::Wait,
            _ => {
                self.pending = Some(PendingReprint {
                    layout,
                    due_at: now + REPRINT_DEBOUNCE,
                });
                ReprintDecision::Wait
            }
        }
    }

    fn record_rows(&mut self, layout: PrintedLayout) {
        match self.printed {
            None => self.printed = Some(PrintedHistory::Uniform(layout)),
            Some(PrintedHistory::Uniform(printed)) if printed != layout => {
                self.printed = Some(PrintedHistory::Mixed);
            }
            Some(_) => {}
        }
    }

    fn mark_printed(&mut self, layout: PrintedLayout) {
        self.printed = Some(PrintedHistory::Uniform(layout));
        self.pending = None;
    }
}

/// Record the layout of this frame (`width` and the compact flag) and decide
/// whether the history must be reprinted.
pub fn observe_minimal_layout(app: &mut AppView, width: u16, now: Instant) -> ReprintDecision {
    let layout = current_layout(app, width);
    app.minimal_state.reprint.observe(layout, now)
}

/// Each history writer, except the reprint itself, calls this after it
/// prints rows at `width`. Rows in a layout different from the printed
/// history make the next reprint unconditional.
pub fn record_minimal_rows_printed(app: &mut AppView, width: u16) {
    let layout = current_layout(app, width);
    app.minimal_state.reprint.record_rows(layout);
}

/// The whole history, welcome card included, is now in native scrollback at
/// `width` in the current compact layout.
pub fn mark_minimal_history_printed(app: &mut AppView, width: u16) {
    let layout = current_layout(app, width);
    app.minimal_state.reprint.mark_printed(layout);
    // The reprint printed a welcome card, so a queued one is not needed.
    app.minimal_state.welcome_pending = false;
}

fn current_layout(app: &AppView, width: u16) -> PrintedLayout {
    PrintedLayout {
        width,
        compact: app.appearance.prompt.compact,
    }
}

#[cfg(test)]
#[path = "reprint_tests.rs"]
mod tests;
