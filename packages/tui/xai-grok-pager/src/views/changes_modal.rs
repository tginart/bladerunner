//! `/changes` — review pending agent edits and accept/reject them (A2).
//!
//! The shell's hunk-tracker already records every pending change (baseline
//! diff) with accept/reject machinery behind ACP extension methods. This modal
//! is the first pager consumer: it lists pending hunks grouped by file with
//! their patches, and dispatches accept/reject per hunk, per file, or for all.
//!
//! Data flows through `Effect::FetchChanges` / `Effect::ChangesAction` (see
//! app/dispatch); this file owns the wire DTOs (the shell's ACP DTOs are
//! serialize-only), the modal state, rendering, and key handling.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use xai_grok_pager_render::render::SafeBuf;

use crate::theme::Theme;
use crate::views::modal_window::{
    ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut, render_modal_window,
};

// ---------------------------------------------------------------------------
// Wire DTOs (mirror the shell's ACP payloads; camelCase on the wire)
// ---------------------------------------------------------------------------

/// One pending hunk from `x.ai/hunk-tracker/get-hunks`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkDto {
    pub id: String,
    pub path: String,
    /// Workspace path for repository reads; `path` remains the display path.
    #[serde(default)]
    pub workspace_path: Option<String>,
    pub line_info: HunkLineInfoDto,
    pub source: HunkSourceDto,
    #[serde(default)]
    pub patch: Option<String>,
}

impl HunkDto {
    pub fn review_path(&self) -> &str {
        self.workspace_path.as_deref().unwrap_or(&self.path)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HunksResponseDto {
    hunks: Vec<HunkDto>,
    #[serde(default)]
    workspace_paths: std::collections::HashMap<String, String>,
}

impl HunksResponseDto {
    pub(crate) fn into_hunks(mut self) -> Vec<HunkDto> {
        for hunk in &mut self.hunks {
            hunk.workspace_path = self
                .workspace_paths
                .remove(&hunk.id)
                .or(hunk.workspace_path.take());
        }
        self.hunks
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkLineInfoDto {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
}

/// Tagged `{"type": "agentEdit", "promptIndex": N}` | `externalEditOnAgentFile`
/// | `external`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HunkSourceDto {
    AgentEdit {
        #[serde(default)]
        prompt_index: usize,
    },
    ExternalEditOnAgentFile,
    External,
    /// Forward-compat sink: a source variant this pager doesn't know yet
    /// (oracle: an unknown variant must never silently drop the hunk list).
    #[serde(other)]
    Unknown,
}

impl HunkSourceDto {
    /// Short source label for the hunk row (localized by the caller's key).
    pub fn key(&self) -> (&'static str, Option<usize>) {
        match self {
            Self::AgentEdit { prompt_index } => ("changes.source.agent_turn", Some(*prompt_index)),
            Self::ExternalEditOnAgentFile => ("changes.source.external_on_agent", None),
            Self::External => ("changes.source.external", None),
            Self::Unknown => ("changes.source.unknown", None),
        }
    }

    /// Whether this hunk is the USER's external edit (not the agent's) —
    /// rejecting it reverts the user's own work on disk.
    pub fn is_external(&self) -> bool {
        matches!(self, Self::ExternalEditOnAgentFile | Self::External)
    }
}

/// One file row from `x.ai/hunk-tracker/get-files`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSummaryDto {
    pub path: String,
    #[serde(default)]
    pub is_agent_file: bool,
    #[serde(default)]
    pub staged: bool,
    pub hunk_count: usize,
    pub additions: usize,
    pub deletions: usize,
}

/// `x.ai/hunk-tracker/*-action` response.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResponseDto {
    pub success: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub affected_count: Option<usize>,
}

// ---------------------------------------------------------------------------
// Actions the modal can dispatch
// ---------------------------------------------------------------------------

/// A review action the user picked in the modal. Dispatched via
/// `Action::ChangesAction` → `Effect::ChangesAction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangesActionKind {
    AcceptHunk(String),
    RejectHunk(String),
    AcceptFile(String),
    RejectFile(String),
    AcceptAll,
    RejectAll,
}

impl ChangesActionKind {
    /// Wire verb for the ACP method.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::AcceptHunk(_) | Self::AcceptFile(_) | Self::AcceptAll => "accept",
            Self::RejectHunk(_) | Self::RejectFile(_) | Self::RejectAll => "reject",
        }
    }

    /// ACP extension method name for this action.
    pub fn method(&self) -> &'static str {
        match self {
            Self::AcceptHunk(_) | Self::RejectHunk(_) => "x.ai/hunk-tracker/hunk-action",
            Self::AcceptFile(_) | Self::RejectFile(_) => "x.ai/hunk-tracker/file-action",
            Self::AcceptAll | Self::RejectAll => "x.ai/hunk-tracker/all-action",
        }
    }

    /// Params JSON for the ACP request (session id injected by the caller).
    pub fn params(&self, session_id: &str) -> serde_json::Value {
        match self {
            Self::AcceptHunk(id) | Self::RejectHunk(id) => serde_json::json!({
                "sessionId": session_id,
                "hunkId": id,
                "action": self.verb(),
            }),
            Self::AcceptFile(path) | Self::RejectFile(path) => serde_json::json!({
                "sessionId": session_id,
                "path": path,
                "action": self.verb(),
            }),
            Self::AcceptAll | Self::RejectAll => serde_json::json!({
                "sessionId": session_id,
                "action": self.verb(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

/// Modal outcome for the app input router.
pub enum ChangesModalOutcome {
    Review(super::review_chat::ReviewChatOutcome),
    Changed,
    Unchanged,
    Closed,
    /// Dispatch a review action (accept/reject).
    Act(ChangesActionKind),
    /// Re-fetch pending changes (manual refresh).
    Refresh,
}

/// A pending reject confirmation: which reject is armed and how many of the
/// affected hunks are the USER's external edits (rejecting those reverts the
/// user's own work — the riskiest case).
pub struct RejectConfirm {
    pub kind: ChangesActionKind,
    pub external_count: usize,
}

pub struct ChangesModalState {
    pub review_chat: Option<super::review_chat::SharedReviewChat>,
    pub window: ModalWindowState,
    /// Flattened hunks in display order (grouped by file).
    pub hunks: Vec<HunkDto>,
    /// Per-file summaries (for the file header rows).
    pub files: Vec<FileSummaryDto>,
    /// Selected hunk index in `hunks` (display order).
    pub selected: usize,
    pub scroll: usize,
    /// First visible line within the selected hunk's patch preview.
    pub patch_scroll: usize,
    /// Last rendered diff bounds for pointer focus and scrolling.
    pub diff_area: Rect,
    /// Waiting on an action's ACP response (blocks further actions).
    pub action_in_flight: bool,
    /// Reject confirmation pending (`y` confirms, `n`/Esc cancels).
    pub confirm: Option<RejectConfirm>,
    /// Transient inline message (action errors, info).
    pub message: Option<String>,
}

impl ChangesModalState {
    pub fn new(files: Vec<FileSummaryDto>, hunks: Vec<HunkDto>) -> Self {
        Self {
            review_chat: None,
            window: ModalWindowState::default(),
            hunks,
            files,
            selected: 0,
            scroll: 0,
            patch_scroll: 0,
            diff_area: Rect::default(),
            action_in_flight: false,
            confirm: None,
            message: None,
        }
    }

    /// Empty state (no pending changes).
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn selected_hunk(&self) -> Option<&HunkDto> {
        self.hunks.get(self.selected)
    }

    pub fn review_anchor(&self) -> Option<xai_grok_shell::extensions::review_chat::ReviewAnchor> {
        let h = self.selected_hunk()?;
        Some(xai_grok_shell::extensions::review_chat::ReviewAnchor {
            path: h.review_path().to_owned(),
            hunk_id: h.id.clone(),
            patch: h.patch.clone().unwrap_or_default(),
            comparison: format!(
                "Session baseline → working files · @@ -{},{} +{},{} @@",
                h.line_info.old_start,
                h.line_info.old_count,
                h.line_info.new_start,
                h.line_info.new_count
            ),
        })
    }

    pub fn chat_focused(&self) -> bool {
        self.review_chat
            .as_ref()
            .is_some_and(|c| c.borrow().focused)
    }

    fn select_next(&mut self) {
        if !self.hunks.is_empty() {
            self.selected = (self.selected + 1).min(self.hunks.len() - 1);
        }
        self.patch_scroll = 0;
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.patch_scroll = 0;
    }

    /// After an action lands, the list is refetched; clamp the selection.
    pub fn clamp_selection(&mut self) {
        if self.hunks.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.hunks.len() - 1);
        }
        self.scroll = 0;
        self.patch_scroll = 0;
    }

    /// Arm the reject-confirmation flow for a reject action. Agent-sourced
    /// single-hunk rejects fire immediately (no confirm — the plan is the
    /// agent's own work); everything else (external hunk, file, all) asks
    /// first, because reject REWRITES THE DISK with no undo.
    fn arm_reject(&mut self, kind: ChangesActionKind) -> ChangesModalOutcome {
        let external_count = match &kind {
            ChangesActionKind::RejectHunk(id) => self
                .hunks
                .iter()
                .filter(|h| &h.id == id && h.source.is_external())
                .count(),
            ChangesActionKind::RejectFile(path) => self
                .hunks
                .iter()
                .filter(|h| &h.path == path && h.source.is_external())
                .count(),
            ChangesActionKind::RejectAll => {
                self.hunks.iter().filter(|h| h.source.is_external()).count()
            }
            _ => 0,
        };
        if let ChangesActionKind::RejectHunk(_) = kind
            && external_count == 0
        {
            self.action_in_flight = true;
            return ChangesModalOutcome::Act(kind);
        }
        self.confirm = Some(RejectConfirm {
            kind,
            external_count,
        });
        ChangesModalOutcome::Changed
    }

    /// Map a footer shortcut id (mouse click) to the same outcome the
    /// matching key would produce. `None` for non-action ids (nav hints).
    pub fn handle_shortcut_id(&mut self, id: usize) -> Option<ChangesModalOutcome> {
        if self.confirm.is_none() {
            if id == 10 {
                let anchor = self.review_anchor();
                if let Some(chat) = &self.review_chat {
                    chat.borrow_mut().open(anchor);
                }
                return Some(ChangesModalOutcome::Changed);
            }
            if id == 11 {
                if let Some(chat) = &self.review_chat {
                    chat.borrow_mut().open_draft();
                }
                return Some(ChangesModalOutcome::Changed);
            }
            if id == 12 {
                return Some(
                    self.handle_key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
                );
            }
            if id == 14 {
                return Some(self.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            }
            if id == 13 {
                if let Some(chat) = &self.review_chat {
                    chat.borrow_mut().focused = false;
                }
                return Some(ChangesModalOutcome::Changed);
            }
        }
        if self.confirm.is_some() {
            return match id {
                0 => {
                    let confirm = self.confirm.take().expect("checked is_some");
                    self.action_in_flight = true;
                    Some(ChangesModalOutcome::Act(confirm.kind))
                }
                1 => {
                    self.confirm = None;
                    Some(ChangesModalOutcome::Changed)
                }
                _ => None,
            };
        }
        if self.action_in_flight {
            return None;
        }
        match id {
            1 if !self.is_empty() => {
                let hunk = self.hunks[self.selected].id.clone();
                self.action_in_flight = true;
                Some(ChangesModalOutcome::Act(ChangesActionKind::AcceptHunk(
                    hunk,
                )))
            }
            2 if !self.is_empty() => {
                let hunk = self.hunks[self.selected].id.clone();
                Some(self.arm_reject(ChangesActionKind::RejectHunk(hunk)))
            }
            3 if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                self.action_in_flight = true;
                Some(ChangesModalOutcome::Act(ChangesActionKind::AcceptFile(
                    path,
                )))
            }
            4 if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                Some(self.arm_reject(ChangesActionKind::RejectFile(path)))
            }
            5 if !self.is_empty() => {
                self.action_in_flight = true;
                Some(ChangesModalOutcome::Act(ChangesActionKind::AcceptAll))
            }
            6 if !self.is_empty() => Some(self.arm_reject(ChangesActionKind::RejectAll)),
            7 => Some(ChangesModalOutcome::Refresh),
            8 => Some(ChangesModalOutcome::Closed),
            _ => None,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> ChangesModalOutcome {
        if self.confirm.is_none() {
            let anchor = self.review_anchor();
            if let Some(chat) = &self.review_chat {
                let mut chat = chat.borrow_mut();
                if chat.focused {
                    return ChangesModalOutcome::Review(chat.handle_key(key, anchor));
                }
                if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
                    || (key.code == KeyCode::Char('c')
                        && !key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    chat.open(anchor);
                    return ChangesModalOutcome::Changed;
                }
            }
        }
        // Reject confirmation mode: only y/n/Esc mean anything.
        if self.confirm.is_some() {
            return match key.code {
                KeyCode::Char('y') => {
                    let confirm = self.confirm.take().expect("checked is_some");
                    self.action_in_flight = true;
                    ChangesModalOutcome::Act(confirm.kind)
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.confirm = None;
                    ChangesModalOutcome::Changed
                }
                _ => ChangesModalOutcome::Unchanged,
            };
        }
        if self.action_in_flight {
            return ChangesModalOutcome::Unchanged;
        }
        match key.code {
            KeyCode::Esc => ChangesModalOutcome::Closed,
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                ChangesModalOutcome::Changed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                ChangesModalOutcome::Changed
            }
            KeyCode::PageDown => {
                let last_line = self
                    .selected_hunk()
                    .and_then(|h| h.patch.as_deref())
                    .map_or(0, |patch| patch.lines().count().saturating_sub(1));
                self.patch_scroll = self.patch_scroll.saturating_add(8).min(last_line);
                ChangesModalOutcome::Changed
            }
            KeyCode::PageUp => {
                self.patch_scroll = self.patch_scroll.saturating_sub(8);
                ChangesModalOutcome::Changed
            }
            // Crossterm reports Ctrl+letter as LOWERCASE Char + CONTROL
            // (oracle High: uppercase chords never fire).
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.is_empty() {
                    self.action_in_flight = true;
                    ChangesModalOutcome::Act(ChangesActionKind::AcceptAll)
                } else {
                    ChangesModalOutcome::Unchanged
                }
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.is_empty() {
                    self.arm_reject(ChangesActionKind::RejectAll)
                } else {
                    ChangesModalOutcome::Unchanged
                }
            }
            KeyCode::Char('a') if !self.is_empty() => {
                let id = self.hunks[self.selected].id.clone();
                self.action_in_flight = true;
                ChangesModalOutcome::Act(ChangesActionKind::AcceptHunk(id))
            }
            KeyCode::Char('x') if !self.is_empty() => {
                let id = self.hunks[self.selected].id.clone();
                self.arm_reject(ChangesActionKind::RejectHunk(id))
            }
            KeyCode::Char('A') if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                self.action_in_flight = true;
                ChangesModalOutcome::Act(ChangesActionKind::AcceptFile(path))
            }
            KeyCode::Char('X') if !self.is_empty() => {
                let path = self.hunks[self.selected].path.clone();
                self.arm_reject(ChangesActionKind::RejectFile(path))
            }
            KeyCode::Char('r') => ChangesModalOutcome::Refresh,
            _ => ChangesModalOutcome::Unchanged,
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Display rows: a file header followed by its hunks (display-only grouping —
/// selection moves over hunk rows only).
enum Row<'a> {
    File(&'a FileSummaryDto),
    Hunk(usize, &'a HunkDto),
}

fn build_rows<'a>(files: &'a [FileSummaryDto], hunks: &'a [HunkDto]) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for file in files {
        rows.push(Row::File(file));
        for (idx, hunk) in hunks
            .iter()
            .enumerate()
            .filter(|(_, h)| h.path == file.path)
        {
            rows.push(Row::Hunk(idx, hunk));
        }
    }
    // Hunks whose file is missing from the summary (defensive) go last.
    for (idx, hunk) in hunks
        .iter()
        .enumerate()
        .filter(|(_, h)| !files.iter().any(|f| f.path == h.path))
    {
        rows.push(Row::Hunk(idx, hunk));
    }
    rows
}

/// Render the changes review modal.
pub fn render_changes_modal(
    buf: &mut Buffer,
    area: Rect,
    state: &mut ChangesModalState,
    theme: &Theme,
) {
    state.diff_area = Rect::default();
    let shortcuts = build_shortcuts(state);
    let chat_visible = state
        .review_chat
        .as_ref()
        .is_some_and(|chat| chat.borrow().visible);
    let config = ModalWindowConfig {
        title: &rust_i18n::t!("changes.title"),
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing {
            // Give stacked panes room on shorter terminals.
            v_margin: if chat_visible {
                area.height.saturating_sub(24).min(2)
            } else {
                ModalSizing::large().v_margin
            },
            v_pad: if chat_visible && area.height < 30 {
                0
            } else {
                ModalSizing::large().v_pad
            },
            ..ModalSizing::large()
        },
        fold_info: None,
    };
    let Some(mca) = render_modal_window(buf, area, &mut state.window, &config, theme) else {
        return;
    };
    let mut inner = mca.content;
    if let Some(shared) = state.review_chat.clone() {
        let mut chat = shared.borrow_mut();
        if chat.draft.is_none() {
            chat.question_anchor = state.review_anchor();
        }
        chat.chat_area = Rect::default();
        if chat.visible {
            if area.width >= 120 {
                let diff_width = inner.width * 3 / 5;
                let chat_area = Rect::new(
                    inner.x + diff_width + 2,
                    inner.y,
                    inner.width.saturating_sub(diff_width + 2),
                    inner.height,
                );
                for y in inner.y..inner.bottom() {
                    buf.set_line_safe(
                        inner.x + diff_width,
                        y,
                        &Line::styled("│", Style::default().fg(theme.gray_dim)),
                        1,
                    );
                }
                chat.render(buf, chat_area, theme);
                inner.width = diff_width;
            } else {
                // Keep both panes on screen: focus changes input routing, never visibility.
                let diff_height = (inner.height / 2)
                    .max(4)
                    .min(inner.height.saturating_sub(10));
                let separator_y = inner.y + diff_height;
                let chat_area = Rect::new(
                    inner.x,
                    separator_y + 1,
                    inner.width,
                    inner.height.saturating_sub(diff_height + 1),
                );
                buf.set_line_safe(
                    inner.x,
                    separator_y,
                    &Line::styled(
                        "─".repeat(inner.width as usize),
                        Style::default().fg(theme.gray_dim),
                    ),
                    inner.width,
                );
                chat.render(buf, chat_area, theme);
                inner.height = diff_height;
            }
        }
    }

    state.diff_area = inner;
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    if state.is_empty() {
        let text = Line::from(Span::styled(
            rust_i18n::t!("changes.empty"),
            Style::default().fg(theme.gray),
        ));
        buf.set_line_safe(inner.x, inner.y + inner.height / 2, &text, inner.width);
        return;
    }

    let rows = build_rows(&state.files, &state.hunks);
    // Patch preview gets the bottom ~40%, list the rest.
    let preview_h = (inner.height * 2 / 5)
        .max(3)
        .min(inner.height.saturating_sub(2));
    let list_h = inner.height.saturating_sub(preview_h + 1);
    let mut y = inner.y;
    let list_end = inner.y + list_h;
    // The separator line between list and patch preview (also the reserved
    // banner slot for confirm/messages).
    let sep_line_y = list_end;

    // Scroll-follow-selection (oracle: state.scroll never advanced, so a
    // selection past the first page rendered nothing). Compute the selected
    // hunk's grouped-row index and clamp the window around it.
    let selected_row = rows
        .iter()
        .position(|r| matches!(r, Row::Hunk(idx, _) if *idx == state.selected))
        .unwrap_or(0);
    let mut scroll = state.scroll;
    if selected_row < scroll {
        scroll = selected_row;
    } else if list_h > 0 && selected_row >= scroll + list_h as usize {
        scroll = selected_row + 1 - list_h as usize;
    }
    state.scroll = scroll;
    let list_overflows = rows.len() > list_h as usize;
    let list_width = inner.width.saturating_sub(u16::from(list_overflows));
    let visible = scroll..(scroll + list_h as usize);
    for (row_idx, row) in rows.iter().enumerate() {
        if y >= list_end {
            break;
        }
        if !visible.contains(&row_idx) {
            continue;
        }
        match row {
            Row::File(file) => {
                let line = Line::from(vec![
                    Span::styled(
                        format!("{}", shorten_path(&file.path, list_width as usize / 2)),
                        Style::default()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "  +{} −{} · {}",
                            file.additions, file.deletions, file.hunk_count
                        ),
                        Style::default().fg(theme.gray_dim),
                    ),
                ]);
                buf.set_line_safe(inner.x, y, &line, list_width);
                y += 1;
            }
            Row::Hunk(idx, hunk) => {
                let selected = *idx == state.selected;
                let (key, turn) = hunk.source.key();
                let source = match turn {
                    Some(t) => rust_i18n::t!(key, turn = t),
                    None => rust_i18n::t!(key),
                };
                let marker = if selected { "▸" } else { " " };
                let style = if selected {
                    Style::default().fg(theme.accent_user)
                } else {
                    Style::default().fg(theme.text_secondary)
                };
                let line = Line::from(vec![
                    Span::styled(format!("{marker} "), style),
                    Span::styled(
                        format!(
                            "@@ -{},{} +{},{} @@",
                            hunk.line_info.old_start,
                            hunk.line_info.old_count,
                            hunk.line_info.new_start,
                            hunk.line_info.new_count
                        ),
                        style,
                    ),
                    Span::styled(format!("  {source}"), Style::default().fg(theme.gray_dim)),
                ]);
                if selected {
                    let mut bg = Style::default().bg(theme.bg_highlight);
                    if let Some(fg) = style.fg {
                        bg = bg.fg(fg);
                    }
                    for x in inner.x..inner.x + inner.width {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_style(bg);
                        }
                    }
                }
                buf.set_line_safe(inner.x, y, &line, list_width);
                y += 1;
            }
        }
    }

    render_changes_scrollbar(
        buf,
        Rect::new(inner.x, inner.y, inner.width, list_h),
        rows.len(),
        scroll,
        theme,
    );

    let patch_lines = state
        .selected_hunk()
        .and_then(|h| h.patch.as_ref())
        .map_or(1, |patch| patch.lines().count());
    state.patch_scroll = state
        .patch_scroll
        .min(patch_lines.saturating_sub(preview_h as usize));
    let patch_width = inner
        .width
        .saturating_sub(u16::from(patch_lines > preview_h as usize));
    // Patch preview of the selected hunk.
    if let Some(hunk) = state.selected_hunk() {
        let sep_y = list_end;
        buf.set_line_safe(
            inner.x,
            sep_y,
            &Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(theme.gray_dim),
            )),
            inner.width,
        );
        let mut py = sep_y + 1;
        let max_py = inner.y + inner.height;
        let patch_text = hunk
            .patch
            .clone()
            .unwrap_or_else(|| rust_i18n::t!("changes.no_patch").into_owned());
        for line in patch_text.lines().skip(state.patch_scroll) {
            if py >= max_py {
                break;
            }
            let (content, color) = if line.starts_with('+') && !line.starts_with("+++") {
                (line, theme.accent_success)
            } else if line.starts_with('-') && !line.starts_with("---") {
                (line, theme.accent_error)
            } else if line.starts_with("@@") {
                (line, theme.accent_user)
            } else {
                (line, theme.gray)
            };
            buf.set_line_safe(
                inner.x,
                py,
                &Line::from(Span::styled(
                    content.to_string(),
                    Style::default().fg(color),
                )),
                patch_width,
            );
            py += 1;
        }
    }

    render_changes_scrollbar(
        buf,
        Rect::new(inner.x, list_end + 1, inner.width, preview_h),
        patch_lines,
        state.patch_scroll,
        theme,
    );

    // Confirm banner (reject needs an explicit yes — reject REWRITES THE
    // DISK with no undo; external hunks are the USER's own edits) rendered on
    // the reserved separator line; otherwise a transient message there.
    if let Some(confirm) = &state.confirm {
        let msg = if confirm.external_count > 0 {
            rust_i18n::t!(
                "changes.confirm_reject_external",
                count = confirm.external_count
            )
        } else {
            rust_i18n::t!("changes.confirm_reject")
        };
        buf.set_line_safe(
            inner.x,
            sep_line_y,
            &Line::from(Span::styled(
                msg.into_owned(),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )),
            inner.width,
        );
    } else if let Some(msg) = &state.message {
        buf.set_line_safe(
            inner.x,
            sep_line_y,
            &Line::from(Span::styled(
                msg.clone(),
                Style::default().fg(theme.warning),
            )),
            inner.width,
        );
    }
}

fn render_changes_scrollbar(
    buf: &mut Buffer,
    area: Rect,
    total: usize,
    offset: usize,
    theme: &Theme,
) {
    let scale = total / u16::MAX as usize + 1;
    crate::render::scrollbar::render_scrollbar_styled(
        buf,
        Some(Rect::new(
            area.right().saturating_sub(1),
            area.y,
            1,
            area.height,
        )),
        (total / scale) as u16,
        area.height,
        (offset / scale) as u16,
        Style::default().bg(theme.scrollbar_bg),
        Style::default()
            .fg(theme.scrollbar_fg)
            .bg(theme.scrollbar_bg),
    );
}

pub(crate) fn build_shortcuts(state: &ChangesModalState) -> Vec<Shortcut<'static>> {
    if state.confirm.is_none() && state.chat_focused() {
        let drafting = state
            .review_chat
            .as_ref()
            .is_some_and(|chat| chat.borrow().draft.is_some());
        if drafting {
            return vec![
                Shortcut {
                    label: "Ctrl+S Send message".into(),
                    clickable: true,
                    id: 12,
                },
                Shortcut {
                    label: "Esc cancel".into(),
                    clickable: true,
                    id: 14,
                },
                Shortcut {
                    label: "Tab diff".into(),
                    clickable: true,
                    id: 13,
                },
                Shortcut {
                    label: "PgUp/PgDn context".into(),
                    clickable: false,
                    id: 9,
                },
            ];
        }
        return vec![
            Shortcut {
                label: "Enter send".into(),
                clickable: false,
                id: 12,
            },
            Shortcut {
                label: "Ctrl+E Message main AI".into(),
                clickable: true,
                id: 11,
            },
            Shortcut {
                label: "Tab / Esc diff".into(),
                clickable: true,
                id: 13,
            },
            Shortcut {
                label: "PgUp/PgDn chat".into(),
                clickable: false,
                id: 9,
            },
            Shortcut {
                label: "Ctrl+C stop".into(),
                clickable: false,
                id: 15,
            },
        ];
    }
    if state.confirm.is_some() {
        return vec![
            Shortcut {
                label: rust_i18n::t!("footer.y_confirm"),
                clickable: true,
                id: 0,
            },
            Shortcut {
                label: rust_i18n::t!("footer.n_esc_cancel"),
                clickable: true,
                id: 1,
            },
        ];
    }
    let mut sc = vec![
        Shortcut {
            label: "c Ask about this · Tab chat".into(),
            clickable: state.review_chat.is_some() && !state.is_empty(),
            id: 10,
        },
        Shortcut {
            label: rust_i18n::t!("footer.nav_jk"),
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "PgUp/PgDn scroll diff".into(),
            clickable: false,
            id: 9,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.accept_hunk"),
            clickable: !state.is_empty(),
            id: 1,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.reject_hunk"),
            clickable: !state.is_empty(),
            id: 2,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.accept_file"),
            clickable: !state.is_empty(),
            id: 3,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.reject_file"),
            clickable: !state.is_empty(),
            id: 4,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.accept_all"),
            clickable: !state.is_empty(),
            id: 5,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.reject_all"),
            clickable: !state.is_empty(),
            id: 6,
        },
        Shortcut {
            label: rust_i18n::t!("changes.footer.refresh"),
            clickable: true,
            id: 7,
        },
        Shortcut {
            label: rust_i18n::t!("footer.esc_close"),
            clickable: true,
            id: 8,
        },
    ];
    crate::views::modal_window::push_vim_nav_search_hint(&mut sc, false);
    sc
}

/// Shorten an absolute path for display: keep the tail within `max` chars.
fn shorten_path(path: &str, max: usize) -> String {
    if max == 0 || path.chars().count() <= max {
        return path.to_string();
    }
    let tail: String = path.chars().rev().take(max.saturating_sub(1)).collect();
    format!("…{}", tail.chars().rev().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(id: &str, path: &str) -> HunkDto {
        HunkDto {
            id: id.to_string(),
            path: path.to_string(),
            workspace_path: None,
            line_info: HunkLineInfoDto {
                old_start: 10,
                old_count: 3,
                new_start: 10,
                new_count: 5,
            },
            source: HunkSourceDto::AgentEdit { prompt_index: 2 },
            patch: Some("@@ -10,3 +10,5 @@\n-old\n+new\n+more".to_string()),
        }
    }

    fn file(path: &str, count: usize) -> FileSummaryDto {
        FileSummaryDto {
            path: path.to_string(),
            is_agent_file: true,
            staged: false,
            hunk_count: count,
            additions: 5,
            deletions: 2,
        }
    }

    #[test]
    fn review_uses_workspace_paths_while_display_and_file_actions_keep_display_paths() {
        let payload = serde_json::json!({
            "hunks": [{
                "id":"h1", "path":"/original/src/sample.rs",
                "lineInfo":{"oldStart":1,"oldCount":1,"newStart":1,"newCount":1},
                "source":{"type":"agentEdit","promptIndex":0}, "patch":"-old\n+new"
            }],
            "workspacePaths":{"h1":"src/sample.rs"}
        });
        let response: HunksResponseDto = serde_json::from_value(payload.clone()).unwrap();
        let mut state = ChangesModalState::new(vec![], response.into_hunks());
        assert_eq!(state.hunks[0].path, "/original/src/sample.rs");
        let anchor = state.review_anchor().unwrap();
        assert_eq!(anchor.path, "src/sample.rs");
        assert_eq!(anchor.patch, "-old\n+new");
        let mut chat = super::super::review_chat::ReviewChatState::new(
            "parent".into(),
            std::path::Path::new("/worktrees/review"),
        );
        chat.prompt.set_text("Explain this");
        chat.begin_question(anchor).unwrap();
        chat.refresh_anchors(&state.hunks);
        assert!(!chat.exchanges[0].stale);
        assert!(
            matches!(state.handle_key(&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)),
            ChangesModalOutcome::Act(ChangesActionKind::AcceptFile(path)) if path == "/original/src/sample.rs")
        );
        let mut legacy = payload;
        legacy.as_object_mut().unwrap().remove("workspacePaths");
        let legacy: HunksResponseDto = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            legacy.into_hunks()[0].review_path(),
            "/original/src/sample.rs"
        );
    }

    #[test]
    fn review_followups_keep_the_captured_anchor_after_all_hunks_disappear() {
        use super::super::review_chat::{ReviewChatOutcome, ReviewChatState};
        use std::{cell::RefCell, path::Path, rc::Rc};
        use xai_grok_shell::extensions::review_chat::AskResponse;
        let mut state = ChangesModalState::new(vec![], vec![hunk("h1", "/a.rs")]);
        let anchor = state.review_anchor().unwrap();
        let mut chat = ReviewChatState::new("parent".into(), Path::new("."));
        chat.open(Some(anchor.clone()));
        chat.prompt.set_text("Why this change?");
        let first = chat.begin_question(anchor.clone()).unwrap();
        chat.finish(Ok(AskResponse {
            child_id: first.child_id.clone(),
            success: true,
            output: "Explanation".into(),
            error: None,
        }));
        state.hunks.clear();
        chat.refresh_anchors(&state.hunks);
        chat.prompt.set_text("How would we undo it?");
        let shared = Rc::new(RefCell::new(chat));
        state.review_chat = Some(shared.clone());
        let ChangesModalOutcome::Review(ReviewChatOutcome::Ask(followup)) =
            state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("follow-up should remain available without pending hunks");
        };
        assert_eq!(followup.previous_child_id, Some(first.child_id));
        assert_eq!(followup.anchor, anchor);
        assert_eq!(followup.question, "How would we undo it?");
        assert!(shared.borrow().exchanges.iter().all(|e| e.stale));
        assert!(state.is_empty());
        assert!(shared.borrow().visible);
    }

    #[test]
    fn review_focus_keeps_diff_and_chat_visible_at_all_widths() {
        use super::super::review_chat::ReviewChatState;
        use std::{cell::RefCell, path::Path, rc::Rc};
        let shared = Rc::new(RefCell::new(ReviewChatState::new(
            "parent".into(),
            Path::new("."),
        )));
        let mut state = ChangesModalState::new(vec![file("/a.rs", 1)], vec![hunk("h1", "/a.rs")]);
        state.review_chat = Some(shared.clone());
        state.handle_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(shared.borrow().focused);
        state.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(!state.action_in_flight);
        assert_eq!(shared.borrow().prompt.text(), "a");
        let theme = Theme::current();
        for (width, height) in [
            (90, 20),
            (90, 24),
            (90, 30),
            (90, 80),
            (119, 40),
            (120, 40),
            (160, 40),
        ] {
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            for focused in [true, false] {
                shared.borrow_mut().focused = focused;
                buf.reset();
                render_changes_modal(&mut buf, area, &mut state, &theme);
                let chat = shared.borrow();
                let chat_area = chat.chat_area;
                let text = buf.content.iter().map(|c| c.symbol()).collect::<String>();
                assert!(text.contains("-old"), "diff hidden at {width}x{height}");
                assert!(text.contains("Review chat"));
                assert_eq!(chat.cursor_pos.is_some(), focused);
                assert!(state.diff_area.height > 0);
                if width >= 120 {
                    assert!(chat_area.width < width / 2);
                    assert!(chat_area.x >= state.diff_area.right());
                } else {
                    assert!(chat_area.width > width / 2);
                    assert!(chat_area.y > state.diff_area.bottom());
                }
                let bottom = chat_area.bottom() - 1;
                assert_eq!(buf[(chat_area.x, bottom)].symbol(), "╰");
                assert_eq!(buf[(chat_area.right() - 1, bottom)].symbol(), "╯");
            }
        }
        shared.borrow_mut().focused = true;
        state.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!shared.borrow().focused);
        state.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(shared.borrow().focused);
        state.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!shared.borrow().focused);
        assert!(matches!(
            state.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ChangesModalOutcome::Closed
        ));
    }

    #[test]
    fn action_kind_wire_shapes() {
        let k = ChangesActionKind::AcceptHunk("h-1".into());
        assert_eq!(k.method(), "x.ai/hunk-tracker/hunk-action");
        assert_eq!(k.verb(), "accept");
        let p = k.params("sess-1");
        assert_eq!(p["sessionId"], "sess-1");
        assert_eq!(p["hunkId"], "h-1");

        let k = ChangesActionKind::RejectFile("/a/b.rs".into());
        assert_eq!(k.method(), "x.ai/hunk-tracker/file-action");
        assert_eq!(k.params("s")["action"], "reject");

        let k = ChangesActionKind::AcceptAll;
        assert_eq!(k.method(), "x.ai/hunk-tracker/all-action");
        assert!(k.params("s")["hunkId"].is_null());
    }

    #[test]
    fn navigation_clamps_and_actions() {
        let mut s = ChangesModalState::new(
            vec![file("/a.rs", 2)],
            vec![hunk("h1", "/a.rs"), hunk("h2", "/a.rs")],
        );
        s.select_next();
        s.select_next();
        assert_eq!(s.selected, 1);
        s.select_prev();
        assert_eq!(s.selected, 0);
        match s.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Act(ChangesActionKind::AcceptHunk(id)) => assert_eq!(id, "h1"),
            _ => panic!("expected accept hunk"),
        }
        assert!(s.action_in_flight);
        // In flight: keys are blocked.
        assert!(matches!(
            s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            ChangesModalOutcome::Unchanged
        ));
    }

    #[test]
    fn long_patch_can_be_read_beyond_the_initial_preview() {
        let mut first = hunk("h1", "/a.rs");
        first.patch = Some(
            (0..60)
                .map(|i| format!("+readable change {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut state =
            ChangesModalState::new(vec![file("/a.rs", 2)], vec![first, hunk("h2", "/a.rs")]);
        let area = Rect::new(0, 0, 120, 40);
        let theme = Theme::current();
        let render_text = |state: &mut ChangesModalState| {
            let mut buf = Buffer::empty(area);
            render_changes_modal(&mut buf, area, state, &theme);
            buf.content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        assert!(render_text(&mut state).contains("readable change 00"));
        assert!(!render_text(&mut state).contains("readable change 24"));
        for _ in 0..3 {
            state.handle_key(&KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        let scrolled = render_text(&mut state);
        assert!(scrolled.contains("readable change 24"));
        assert!(!scrolled.contains("readable change 00"));
        state.handle_key(&KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(render_text(&mut state).contains("readable change 16"));
        for _ in 0..20 {
            state.handle_key(&KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        assert!(render_text(&mut state).contains("readable change 59"));
        state.select_next();
        assert!(render_text(&mut state).contains("+more"));
    }

    #[test]
    fn reject_all_requires_confirmation() {
        let mut s = ChangesModalState::new(vec![file("/a.rs", 1)], vec![hunk("h1", "/a.rs")]);
        // Crossterm reports Ctrl+letter as LOWERCASE Char + CONTROL.
        match s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)) {
            ChangesModalOutcome::Changed => {}
            _ => panic!("expected confirm mode"),
        }
        assert!(s.confirm.is_some());
        // n cancels.
        s.handle_key(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(s.confirm.is_none());
        // Re-arm and confirm with y.
        s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        match s.handle_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Act(ChangesActionKind::RejectAll) => {}
            _ => panic!("expected reject all"),
        }
    }

    #[test]
    fn external_reject_arms_confirmation_with_count() {
        let mut s = ChangesModalState::new(
            vec![file("/a.rs", 1)],
            vec![{
                let mut h = hunk("h1", "/a.rs");
                h.source = HunkSourceDto::External;
                h
            }],
        );
        // Agent-sourced single-hunk rejects fire immediately (no confirm).
        let mut agent_only =
            ChangesModalState::new(vec![file("/a.rs", 1)], vec![hunk("h1", "/a.rs")]);
        match agent_only.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Act(ChangesActionKind::RejectHunk(_)) => {}
            _ => panic!("agent hunk reject must not require confirmation"),
        }
        // External hunk: confirm with external_count = 1.
        match s.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)) {
            ChangesModalOutcome::Changed => {}
            _ => panic!("external reject must arm confirmation"),
        }
        let confirm = s.confirm.as_ref().expect("confirm armed");
        assert_eq!(confirm.external_count, 1);
    }

    #[test]
    fn rows_group_hunks_under_their_file() {
        let files = vec![file("/a.rs", 1), file("/b.rs", 1)];
        let hunks = vec![
            hunk("h1", "/a.rs"),
            hunk("h2", "/b.rs"),
            hunk("h3", "/orphan.rs"),
        ];
        let rows = build_rows(&files, &hunks);
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], Row::File(_)));
        assert!(matches!(rows[1], Row::Hunk(0, _)));
        assert!(matches!(rows[2], Row::File(_)));
        assert!(matches!(rows[3], Row::Hunk(1, _)));
        assert!(matches!(rows[4], Row::Hunk(2, _)));
    }
}
